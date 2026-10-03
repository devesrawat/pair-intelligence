/**
 * Budget gating for OpenClaw model calls.
 *
 * OpenClaw has NO supported way to abort a run from `before_model_resolve`: a throw is caught and
 * logged, and the run proceeds on the default model (src/agents/embedded-agent-runner/run/setup.ts).
 * The supported gate is `before_agent_run` (fail closed, runs after prompt build and before model
 * submission). So the flow is:
 *   1. `before_model_resolve`: reserve; record the outcome per run; return the model override only
 *      when the reservation succeeded.
 *   2. `before_agent_run`: block the run unless this run has a successful reservation. A missing
 *      record (hook never fired, locked model selection, no run id, any error) means block.
 *   3. `llm_output`: reconcile with the run's real usage.
 * One reservation covers one run (llm_output.usage is a per-run aggregate over all its model calls).
 */
import type { PairClient } from "./client.ts";
import { parseReconcile, parseReserve, type ReconcileOutcome } from "./decision.ts";
import type { HookLog } from "./gate.ts";
import { newTraceId, taskIdFor } from "./ids.ts";

export const RESERVE_PATH = "/v1/budget/reserve";
export const RECONCILE_PATH = "/v1/budget/reconcile";

/** Rough chars-per-token for English prompts; the estimate is deliberately an upper-leaning guess. */
export const CHARS_PER_TOKEN = 4;
/**
 * Tokens OpenClaw adds around the user prompt on every model call (system prompt, tool schemas,
 * workspace files, history). Calibrated from the live harness: a 24-char prompt produced a 53,580-byte request (about 13,400 tokens at 4 chars per token), so 16,000 leaves headroom.
 */
export const SYSTEM_OVERHEAD_TOKENS = 16_000;
/** Output allowance per call; matches the largest `maxTokens` configured for the routed model. */
export const ASSUMED_MAX_OUTPUT_TOKENS = 2_048;
/** Model calls one run may make (tool round trips). Reconcile flags `overrun` if reality exceeds the hold. */
export const ASSUMED_MODEL_CALLS_PER_RUN = 4;
const MICROS_PER_MTOK_DIVISOR = 1_000_000;
const STATE_CAP = 2_000;
const STATE_TTL_MS = 60 * 60 * 1_000;
const BLOCK_MESSAGE = "PAIR budget gate: this run was not authorized.";

export type TaskKind = "default" | "research" | "coding";

export type PriceConfig = {
  readonly priceVersion: string;
  readonly inputPerMtokMicros: number;
  readonly outputPerMtokMicros: number;
};

export function estimateMaxCostMicros(promptChars: number, price: PriceConfig): number {
  const inputTokens = Math.ceil(promptChars / CHARS_PER_TOKEN) + SYSTEM_OVERHEAD_TOKENS;
  const perCall = inputTokens * price.inputPerMtokMicros + ASSUMED_MAX_OUTPUT_TOKENS * price.outputPerMtokMicros;
  return Math.max(1, Math.ceil((ASSUMED_MODEL_CALLS_PER_RUN * perCall) / MICROS_PER_MTOK_DIVISOR));
}

export type UsageCost = {
  readonly inputTokens: number;
  readonly outputTokens: number;
  /** `null` when usage is unknown: the reservation then stays unresolved (counted in full). */
  readonly costMicros: number | null;
};

const isCount = (v: unknown): v is number => typeof v === "number" && Number.isSafeInteger(v) && v >= 0;

/**
 * Cost from reported usage and the configured prices. Cache read/write tokens are priced as input
 * (an upper bound; the adapter has no cache price). Unknown or malformed usage is `null`, never 0.
 */
export function actualCost(usage: unknown, price: PriceConfig): UsageCost {
  const unknown: UsageCost = { inputTokens: 0, outputTokens: 0, costMicros: null };
  if (typeof usage !== "object" || usage === null) return unknown;
  const u = usage as Record<string, unknown>;
  const cacheRead = u["cacheRead"] ?? 0;
  const cacheWrite = u["cacheWrite"] ?? 0;
  if (!isCount(u["input"]) || !isCount(u["output"]) || !isCount(cacheRead) || !isCount(cacheWrite)) return unknown;
  const inputTokens = u["input"] + cacheRead + cacheWrite;
  const outputTokens = u["output"];
  const costMicros = Math.ceil(
    (inputTokens * price.inputPerMtokMicros + outputTokens * price.outputPerMtokMicros) / MICROS_PER_MTOK_DIVISOR,
  );
  return { inputTokens, outputTokens, costMicros };
}

export type BudgetGateOptions = {
  readonly client: PairClient;
  readonly log: HookLog;
  readonly price: PriceConfig;
  readonly taskKind: TaskKind;
  readonly overrideProvider: string;
  readonly overrideModel: string;
  readonly now?: () => number;
};

export type RunContext = { readonly runId?: string };
export type ModelResolveOverride = { readonly providerOverride: string; readonly modelOverride: string };
export type RunDecision =
  | { readonly outcome: "pass" }
  | {
      readonly outcome: "block";
      readonly reason: string;
      readonly message: string;
      readonly category: "cost_limit";
    };

type RunState =
  | { readonly status: "reserved"; readonly reservationId: string; readonly at: number; reconciled: boolean }
  | { readonly status: "refused"; readonly why: string; readonly at: number };

export type BudgetGate = {
  readonly beforeModelResolve: (event: { readonly prompt: string }, ctx: RunContext) => Promise<ModelResolveOverride | undefined>;
  readonly beforeAgentRun: (event: unknown, ctx: RunContext) => Promise<RunDecision>;
  readonly reconcile: (event: { readonly runId: string; readonly usage: unknown }) => Promise<ReconcileOutcome | undefined>;
};

const block = (reason: string): RunDecision => ({ outcome: "block", reason, message: BLOCK_MESSAGE, category: "cost_limit" });

export function createBudgetGate(options: BudgetGateOptions): BudgetGate {
  const now = options.now ?? Date.now;
  const runs = new Map<string, RunState>();

  const remember = (runId: string, state: RunState): void => {
    runs.delete(runId);
    runs.set(runId, state);
    for (const [key, value] of runs) {
      if (runs.size <= STATE_CAP && now() - value.at <= STATE_TTL_MS) break;
      runs.delete(key);
    }
  };

  const reserve = async (runId: string, prompt: string): Promise<RunState> => {
    const promptChars = prompt.length;
    const maxCost = estimateMaxCostMicros(promptChars, options.price);
    const taskId = taskIdFor(runId);
    // Audit before the money moves: an unwritable log means no reservation and no override.
    options.log("before_model_resolve", { phase: "reserve-attempt", runId, taskId, promptChars, maxCostMicros: maxCost });
    const result = await options.client.post(
      RESERVE_PATH,
      {
        task_id: taskId,
        kind: options.taskKind,
        category: "metered",
        max_cost_micros: maxCost,
        price_version: options.price.priceVersion,
      },
      newTraceId(),
    );
    if (!result.ok) {
      options.log("before_model_resolve", {
        phase: "refused",
        runId,
        taskId,
        errorKind: result.kind,
        status: result.status,
        errorCode: result.errorCode,
      });
      return { status: "refused", why: result.errorCode ?? result.kind, at: now() };
    }
    const reserved = parseReserve(result.data);
    if (reserved === undefined) {
      options.log("before_model_resolve", { phase: "refused", runId, taskId, errorKind: "malformed-reserve" });
      return { status: "refused", why: "malformed-reserve", at: now() };
    }
    options.log("before_model_resolve", { phase: "reserved", runId, taskId, reservationId: reserved.reservationId });
    return { status: "reserved", reservationId: reserved.reservationId, at: now(), reconciled: false };
  };

  return {
    beforeModelResolve: async (event, ctx) => {
      const runId = typeof ctx.runId === "string" && ctx.runId.length > 0 ? ctx.runId : undefined;
      if (runId === undefined) return undefined;
      try {
        const existing = runs.get(runId);
        const state = existing?.status === "reserved" ? existing : await reserve(runId, event.prompt);
        remember(runId, state);
        return state.status === "reserved"
          ? { providerOverride: options.overrideProvider, modelOverride: options.overrideModel }
          : undefined;
      } catch (err) {
        remember(runId, { status: "refused", why: "error", at: now() });
        try {
          options.log("before_model_resolve", { phase: "refused", runId, errorKind: "exception", error: String(err) });
        } catch {
          // Logging must not change the verdict.
        }
        return undefined;
      }
    },

    beforeAgentRun: async (_event, ctx) => {
      try {
        const runId = typeof ctx.runId === "string" && ctx.runId.length > 0 ? ctx.runId : undefined;
        if (runId === undefined) return block("no run id: cannot tie this run to a reservation");
        const state = runs.get(runId);
        if (state === undefined) return block("no reservation recorded for this run");
        if (state.status !== "reserved") return block(`reservation refused: ${state.why}`);
        options.log("before_agent_run", { phase: "pass", runId, reservationId: state.reservationId });
        return { outcome: "pass" };
      } catch {
        return block("budget gate error");
      }
    },

    reconcile: async (event) => {
      try {
        const state = runs.get(event.runId);
        if (state === undefined || state.status !== "reserved" || state.reconciled) return undefined;
        state.reconciled = true;
        const usage = actualCost(event.usage, options.price);
        const result = await options.client.post(
          RECONCILE_PATH,
          {
            reservation_id: state.reservationId,
            input_tokens: usage.inputTokens,
            output_tokens: usage.outputTokens,
            actual_cost_micros: usage.costMicros,
            price_version: options.price.priceVersion,
          },
          newTraceId(),
        );
        const outcome = result.ok ? parseReconcile(result.data) : undefined;
        options.log("llm_output", {
          phase: outcome === undefined ? "reconcile-failed" : "reconciled",
          runId: event.runId,
          reservationId: state.reservationId,
          inputTokens: usage.inputTokens,
          outputTokens: usage.outputTokens,
          costMicros: usage.costMicros,
          state: outcome?.state,
          overrun: outcome?.overrun,
          errorKind: result.ok ? undefined : result.kind,
        });
        return outcome;
      } catch {
        return undefined;
      }
    },
  };
}
