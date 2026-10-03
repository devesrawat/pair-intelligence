import { definePluginEntry } from "openclaw/plugin-sdk/plugin-entry";
import { createBudgetGate } from "./budget.ts";
import { createPairClient } from "./client.ts";
import { loadConnection } from "./config.ts";
import { createToolGate, type HookLog } from "./gate.ts";
import { appendJsonl } from "./jsonl.ts";
import { createPairToolGate } from "./pair-gate.ts";
import { parsePluginConfig } from "./plugin-config.ts";

const APPROVAL_TIMEOUT_MS = 3_000;
const GATE_PRIORITY = 100;
const MISCONFIGURED_TOOL_REASON = "pair: adapter is misconfigured, tool calls are denied";
const MISCONFIGURED_RUN_REASON = "pair: adapter is misconfigured, runs are denied";

export default definePluginEntry({
  id: "pair-spike",
  name: "PAIR Spike",
  description: "PAIR adapter: policy-gated tools, budget-gated model calls, reconciled usage",
  register(api) {
    const parsed = parsePluginConfig(api.pluginConfig);
    const connection = loadConnection(process.env);

    // A broken config must not leave OpenClaw ungated (and `register` must not throw, which could
    // unload the plugin): install deny-all handlers instead. Tool gating works without the
    // conversation opt-in; the run gate needs it, so an unconfigured run may still proceed if the
    // opt-in is missing (see docs/spike-openclaw-plugin.md).
    if (!parsed.ok || !connection.ok) {
      const why = !parsed.ok ? parsed.reason : connection.ok ? "unknown" : connection.reason;
      api.logger.error(`pair-spike: refusing to run ungated: ${why}`);
      api.on("before_tool_call", () => ({ block: true, blockReason: MISCONFIGURED_TOOL_REASON }), { priority: GATE_PRIORITY });
      api.on("before_agent_run", () => ({ outcome: "block", reason: why, message: MISCONFIGURED_RUN_REASON }));
      return;
    }

    const cfg = parsed.value;
    const client = createPairClient(connection.value);
    const hookLog: HookLog = (hook, detail) => {
      appendJsonl(cfg.hookLogPath, { hook, ...detail });
      api.logger.info(`pair-spike hook=${hook} ${JSON.stringify(detail)}`);
    };

    const localGate = createToolGate({
      log: hookLog,
      approvalTimeoutMs: APPROVAL_TIMEOUT_MS,
      ...(cfg.injectPolicyError === undefined ? {} : { injectPolicyError: cfg.injectPolicyError }),
    });
    api.on(
      "before_tool_call",
      createPairToolGate({ client, log: hookLog, localGate, approvalTimeoutMs: APPROVAL_TIMEOUT_MS }),
      { priority: GATE_PRIORITY },
    );

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
      await budget.reconcile({ runId: event.runId, usage: event.usage });
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
  },
});
