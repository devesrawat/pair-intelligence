/**
 * Hook registration, kept free of the OpenClaw SDK import so it can be tested with a fake api.
 * `index.ts` is the thin entry that hands the real api here.
 */
import { createBudgetGate, type ModelResolveEvent, type ModelResolveOverride, type RunContext, type RunDecision } from "./budget.ts";
import { createPairClient } from "./client.ts";
import { loadConnection, type AdapterEnv } from "./config.ts";
import { createToolGate, type GateResult, type HookLog, type ToolCallEvent } from "./gate.ts";
import { paramsHash } from "./ids.ts";
import { appendJsonl } from "./jsonl.ts";
import { createPairToolGate } from "./pair-gate.ts";
import { createParamsWatch } from "./params-watch.ts";
import { parsePluginConfig, type PluginConfig } from "./plugin-config.ts";

export const PLUGIN_ID = "pair-spike";
const APPROVAL_TIMEOUT_MS = 3_000;
const GATE_PRIORITY = 100;
export const MISCONFIGURED_TOOL_REASON = "pair: adapter is misconfigured, tool calls are denied";
const MISCONFIGURED_RUN_REASON = "pair: adapter is misconfigured, runs are denied";

export type HookOptions = { readonly priority?: number };
export type LlmOutputEvent = {
  readonly runId: string;
  readonly provider: string;
  readonly model: string;
  readonly resolvedRef?: string;
  readonly usage?: unknown;
};
export type ModelCallEndedEvent = {
  readonly runId?: string;
  readonly callId?: string;
  readonly provider?: string;
  readonly model?: string;
  readonly outcome?: string;
  readonly durationMs?: number;
};
export type AfterToolCallEvent = ToolCallEvent;
type RunBlock = { readonly outcome: "block"; readonly reason: string; readonly message: string };

/** The slice of the OpenClaw plugin api this adapter uses. */
export type PairApi = {
  readonly config: unknown;
  readonly pluginConfig?: Record<string, unknown>;
  readonly logger: { readonly info: (message: string) => void; readonly error: (message: string) => void };
  readonly on: {
    (hook: "before_tool_call", handler: (event: ToolCallEvent) => GateResult | Promise<GateResult>, opts?: HookOptions): void;
    (hook: "after_tool_call", handler: (event: AfterToolCallEvent) => void): void;
    (hook: "before_model_resolve", handler: (event: ModelResolveEvent, ctx: RunContext) => Promise<ModelResolveOverride | undefined>): void;
    (hook: "before_agent_run", handler: (event: unknown, ctx: RunContext) => RunDecision | RunBlock | Promise<RunDecision | RunBlock>): void;
    (hook: "llm_output", handler: (event: LlmOutputEvent) => Promise<void>): void;
    (hook: "model_call_ended", handler: (event: ModelCallEndedEvent) => void): void;
  };
};

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * Whether `plugins.entries.<id>.hooks.allowConversationAccess` is `true` in the host config.
 * OpenClaw drops `before_model_resolve`, `before_agent_run` and `llm_output` of a non-bundled
 * plugin unless it is exactly `true` (src/plugins/hook-policy-decisions.ts); anything else here
 * therefore means the model-call gate would silently not exist.
 */
export function conversationAccessGranted(hostConfig: unknown, pluginId: string): boolean {
  if (!isRecord(hostConfig) || !isRecord(hostConfig["plugins"])) return false;
  const entries = hostConfig["plugins"]["entries"];
  if (!isRecord(entries) || !isRecord(entries[pluginId])) return false;
  const hooks = entries[pluginId]["hooks"];
  return isRecord(hooks) && hooks["allowConversationAccess"] === true;
}

function installDenyAllTools(api: PairApi): void {
  api.on("before_tool_call", () => ({ block: true, blockReason: MISCONFIGURED_TOOL_REASON }), { priority: GATE_PRIORITY });
}

function registerToolHooks(api: PairApi, cfg: PluginConfig, hookLog: HookLog, deps: ToolHookDeps): void {
  const watch = createParamsWatch();
  const localGate = createToolGate({
    log: hookLog,
    approvalTimeoutMs: APPROVAL_TIMEOUT_MS,
    ...(cfg.injectPolicyError === undefined ? {} : { injectPolicyError: cfg.injectPolicyError }),
  });
  api.on(
    "before_tool_call",
    createPairToolGate({
      client: deps.client,
      log: hookLog,
      localGate,
      approvalTimeoutMs: APPROVAL_TIMEOUT_MS,
      onAllowed: watch.approved,
      ...(cfg.maxToolCallsPerRun === undefined ? {} : { maxToolCallsPerRun: cfg.maxToolCallsPerRun }),
    }),
    { priority: GATE_PRIORITY },
  );
  api.on("after_tool_call", (event) => {
    const hash = paramsHash(event.params);
    if (watch.check(event.toolCallId, hash) !== "mismatch") return;
    api.logger.error(`pair-spike: tool call params differ from what PAIR approved (another plugin rewrote them): tool=${event.toolName}`);
    try {
      hookLog("after_tool_call", { decision: "params-changed-after-approval", toolName: event.toolName, runId: event.runId, paramsSha256: hash });
    } catch {
      // The error above is already out; a broken audit log must not break the hook.
    }
  });
}

type ToolHookDeps = { readonly client: ReturnType<typeof createPairClient> };

function registerModelHooks(api: PairApi, cfg: PluginConfig, hookLog: HookLog, client: ToolHookDeps["client"]): void {
  const budget = createBudgetGate({
    client,
    log: hookLog,
    price: {
      priceVersion: cfg.priceVersion,
      inputPerMtokMicros: cfg.inputPricePerMtokMicros,
      outputPerMtokMicros: cfg.outputPricePerMtokMicros,
    },
    taskKind: cfg.taskKind,
    overrideProvider: cfg.overrideProvider,
    overrideModel: cfg.overrideModel,
  });
  api.on("before_model_resolve", (event, ctx) => budget.beforeModelResolve(event, ctx));
  api.on("before_agent_run", (event, ctx) => budget.beforeAgentRun(event, ctx));
  api.on("llm_output", async (event) => {
    try {
      appendJsonl(cfg.usageLogPath, {
        source: "llm_output",
        runId: event.runId,
        provider: event.provider,
        model: event.model,
        resolvedRef: event.resolvedRef,
        usage: event.usage,
      });
    } catch (err) {
      api.logger.error(`pair-spike: usage log unwritable: ${String(err)}`);
    }
    await budget.reconcile({ runId: event.runId, usage: event.usage, provider: event.provider, model: event.model });
  });
  api.on("model_call_ended", (event) => {
    appendJsonl(cfg.usageLogPath, {
      source: "model_call_ended",
      runId: event.runId,
      callId: event.callId,
      provider: event.provider,
      model: event.model,
      outcome: event.outcome,
      durationMs: event.durationMs,
    });
  });
}

export function registerPair(api: PairApi, env: AdapterEnv): void {
  const parsed = parsePluginConfig(api.pluginConfig);
  const connection = loadConnection(env);

  // A broken config must not leave OpenClaw ungated (and `register` must not throw, which could
  // unload the plugin): install deny-all handlers instead.
  if (!parsed.ok || !connection.ok) {
    const why = !parsed.ok ? parsed.reason : connection.ok ? "unknown" : connection.reason;
    api.logger.error(`pair-spike: refusing to run ungated: ${why}`);
    installDenyAllTools(api);
    api.on("before_agent_run", () => ({ outcome: "block", reason: why, message: MISCONFIGURED_RUN_REASON }));
    return;
  }

  // Without the opt-in OpenClaw silently drops the model-call hooks, so model calls would run
  // with no reservation. Tool gating does not need it; deny every tool instead of half-gating.
  if (!conversationAccessGranted(api.config, PLUGIN_ID)) {
    api.logger.error(
      `pair-spike: plugins.entries.${PLUGIN_ID}.hooks.allowConversationAccess is not true: ` +
        "the budget gate cannot run and model calls are NOT gated. Denying every tool call. " +
        "Set it to true and restart the gateway.",
    );
    installDenyAllTools(api);
    return;
  }

  const cfg = parsed.value;
  const client = createPairClient(connection.value);
  const hookLog: HookLog = (hook, detail) => {
    appendJsonl(cfg.hookLogPath, { hook, ...detail });
    api.logger.info(`pair-spike hook=${hook} ${JSON.stringify(detail)}`);
  };
  registerToolHooks(api, cfg, hookLog, { client });
  registerModelHooks(api, cfg, hookLog, client);
}
