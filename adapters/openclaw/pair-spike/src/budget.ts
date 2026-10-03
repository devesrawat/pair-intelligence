/**
 * Budget gating for OpenClaw model calls.
 *
 * OpenClaw has NO supported way to abort a run from `before_model_resolve`: a throw is caught and
 * logged, and the run proceeds on the default model (src/agents/embedded-agent-runner/run/setup.ts).
 * The supported gate is `before_agent_run` (fail closed, runs after prompt build and before model
 * submission). So the flow is:
 *   1. `before_model_resolve` (once per RUN): reserve; record the outcome per run; return the model
 *      override only when the reservation succeeded.
 *   2. `before_agent_run` (once per ATTEMPT): block unless this run holds a live reservation. A
 *      missing record, a refused reservation or an already reconciled one means block, so a later
 *      attempt (compaction retry, empty-response retry, auth rotation) never runs unreserved.
 *   3. `llm_output` (once per ATTEMPT): reconcile with that attempt's usage. Usage from a model
 *      other than the override (failover) is reconciled with an unknown cost.
 * One reservation covers one attempt of one run. The hold is an ESTIMATE (see budget-estimate.ts).
 */
import type { PairClient } from "./client.ts";
import { parseReconcile, parseReserve, type ReconcileOutcome } from "./decision.ts";
import type { HookLog } from "./gate.ts";
import { newTraceId, taskIdFor } from "./ids.ts";
import { actualCost, estimateMaxCostMicros, type PriceConfig, type UsageCost } from "./budget-estimate.ts";
import { createRunStore, type RunState, type RunStore } from "./budget-runs.ts";

export * from "./budget-estimate.ts";

export const RESERVE_PATH = "/v1/budget/reserve";
export const RECONCILE_PATH = "/v1/budget/reconcile";
const BLOCK_MESSAGE = "PAIR budget gate: this run was not authorized.";

export type TaskKind = "default" | "research" | "coding";

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
export type ModelResolveEvent = {
  readonly prompt: string;
  readonly attachments?: ReadonlyArray<{ readonly kind?: string }>;
};
export type ModelResolveOverride = { readonly providerOverride: string; readonly modelOverride: string };
export type ReconcileEvent = {
  readonly runId: string;
  readonly usage: unknown;
  /** Provider and model that produced the usage; anything but the override is a failover. */
  readonly provider: string;
  readonly model: string;
};
export type RunDecision =
  | { readonly outcome: "pass" }
  | {
      readonly outcome: "block";
      readonly reason: string;
      readonly message: string;
      readonly category: "cost_limit";
    };

export type BudgetGate = {
  readonly beforeModelResolve: (event: ModelResolveEvent, ctx: RunContext) => Promise<ModelResolveOverride | undefined>;
  readonly beforeAgentRun: (event: unknown, ctx: RunContext) => Promise<RunDecision>;
  readonly reconcile: (event: ReconcileEvent) => Promise<ReconcileOutcome | undefined>;
};

type Deps = {
  readonly options: BudgetGateOptions;
  readonly store: RunStore;
  readonly now: () => number;
};

const block = (reason: string): RunDecision => ({ outcome: "block", reason, message: BLOCK_MESSAGE, category: "cost_limit" });
const runIdOf = (ctx: RunContext): string | undefined =>
  typeof ctx.runId === "string" && ctx.runId.length > 0 ? ctx.runId : undefined;

async function reserve(deps: Deps, runId: string, event: ModelResolveEvent): Promise<RunState> {
  const { options, now } = deps;
  const promptChars = event.prompt.length;
  const attachmentCount = event.attachments?.length ?? 0;
  const maxCost = estimateMaxCostMicros(promptChars, options.price, attachmentCount);
  const taskId = taskIdFor(runId);
  // Audit before the money moves: an unwritable log means no reservation and no override.
  options.log("before_model_resolve", { phase: "reserve-attempt", runId, taskId, promptChars, attachmentCount, maxCostMicros: maxCost });
  const body = {
    task_id: taskId,
    kind: options.taskKind,
    category: "metered",
    // The server computes the worst-case hold from this model's registry price; max_cost_micros
    // can only raise it. The model must be priced under price_version or the reserve is refused.
    model_id: options.overrideModel,
    max_cost_micros: maxCost,
    price_version: options.price.priceVersion,
  };
  const result = await options.client.post(RESERVE_PATH, body, newTraceId());
  if (!result.ok) {
    const errorCode = result.errorCode;
    options.log("before_model_resolve", { phase: "refused", runId, taskId, errorKind: result.kind, status: result.status, errorCode });
    return { status: "refused", why: errorCode ?? result.kind, at: now() };
  }
  const reserved = parseReserve(result.data);
  if (reserved === undefined) {
    options.log("before_model_resolve", { phase: "refused", runId, taskId, errorKind: "malformed-reserve" });
    return { status: "refused", why: "malformed-reserve", at: now() };
  }
  options.log("before_model_resolve", { phase: "reserved", runId, taskId, reservationId: reserved.reservationId });
  return { status: "reserved", reservationId: reserved.reservationId, at: now() };
}

async function resolveModel(deps: Deps, event: ModelResolveEvent, ctx: RunContext): Promise<ModelResolveOverride | undefined> {
  const { options, store, now } = deps;
  const runId = runIdOf(ctx);
  if (runId === undefined) return undefined;
  try {
    const existing = store.get(runId);
    if (existing?.status === "reconciled") return undefined;
    const state = existing?.status === "reserved" ? existing : await reserve(deps, runId, event);
    store.set(runId, state);
    return state.status === "reserved"
      ? { providerOverride: options.overrideProvider, modelOverride: options.overrideModel }
      : undefined;
  } catch (err) {
    store.set(runId, { status: "refused", why: "error", at: now() });
    try {
      options.log("before_model_resolve", { phase: "refused", runId, errorKind: "exception", error: String(err) });
    } catch {
      // Logging must not change the verdict.
    }
    return undefined;
  }
}

function gateRun(deps: Deps, ctx: RunContext): RunDecision {
  try {
    const runId = runIdOf(ctx);
    if (runId === undefined) return block("no run id: cannot tie this run to a reservation");
    const state = deps.store.get(runId);
    if (state === undefined) return block("no reservation recorded for this run");
    if (state.status === "reconciled") return block("reservation already reconciled: a later attempt has no hold");
    if (state.status === "refused") return block(`reservation refused: ${state.why}`);
    deps.options.log("before_agent_run", { phase: "pass", runId, reservationId: state.reservationId });
    return { outcome: "pass" };
  } catch {
    return block("budget gate error");
  }
}

/** Cost of the reported usage; unknown when the model is not the one the hold was priced for. */
function costFor(deps: Deps, event: ReconcileEvent): UsageCost {
  const { options } = deps;
  const usage = actualCost(event.usage, options.price);
  const priced = event.provider === options.overrideProvider && event.model === options.overrideModel;
  return priced ? usage : { ...usage, costMicros: null };
}

async function reconcileRun(deps: Deps, event: ReconcileEvent): Promise<ReconcileOutcome | undefined> {
  const { options, store, now } = deps;
  try {
    const state = store.get(event.runId);
    if (state === undefined || state.status !== "reserved") return undefined;
    store.set(event.runId, { status: "reconciled", reservationId: state.reservationId, at: now() });
    const usage = costFor(deps, event);
    const body = {
      reservation_id: state.reservationId,
      // Must match the reserve's task: the server refuses a settlement from a different task.
      task_id: taskIdFor(event.runId),
      input_tokens: usage.inputTokens,
      output_tokens: usage.outputTokens,
      actual_cost_micros: usage.costMicros,
      price_version: options.price.priceVersion,
    };
    const result = await options.client.post(RECONCILE_PATH, body, newTraceId());
    const outcome = result.ok ? parseReconcile(result.data) : undefined;
    options.log("llm_output", {
      phase: outcome === undefined ? "reconcile-failed" : "reconciled",
      runId: event.runId,
      reservationId: state.reservationId,
      inputTokens: usage.inputTokens,
      outputTokens: usage.outputTokens,
      costMicros: usage.costMicros,
      model: event.model,
      state: outcome?.state,
      overrun: outcome?.overrun,
      errorKind: result.ok ? undefined : result.kind,
    });
    return outcome;
  } catch {
    return undefined;
  }
}

export function createBudgetGate(options: BudgetGateOptions): BudgetGate {
  const now = options.now ?? Date.now;
  const deps: Deps = { options, store: createRunStore(now), now };
  return {
    beforeModelResolve: (event, ctx) => resolveModel(deps, event, ctx),
    beforeAgentRun: async (_event, ctx) => gateRun(deps, ctx),
    reconcile: (event) => reconcileRun(deps, event),
  };
}
