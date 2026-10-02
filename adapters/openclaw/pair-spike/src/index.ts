import { appendFileSync, mkdirSync } from "node:fs";
import { dirname } from "node:path";
import { definePluginEntry } from "openclaw/plugin-sdk/plugin-entry";
import { decideToolCall } from "./rules.js";

type SpikeConfig = {
  readonly usageLogPath: string;
  readonly hookLogPath: string;
  readonly overrideProvider: string;
  readonly overrideModel: string;
  /** Test probe only: makes the tool gate throw so fail-closed behaviour can be observed. */
  readonly injectPolicyError?: boolean;
};

const PROMPT_PREVIEW_CHARS = 80;
const APPROVAL_TIMEOUT_MS = 3_000;
const GATE_PRIORITY = 100;

function isSpikeConfig(value: unknown): value is SpikeConfig {
  if (typeof value !== "object" || value === null) return false;
  const v = value as Record<string, unknown>;
  const stringsOk = ["usageLogPath", "hookLogPath", "overrideProvider", "overrideModel"].every(
    (k) => typeof v[k] === "string" && (v[k] as string).length > 0,
  );
  return stringsOk && (v["injectPolicyError"] === undefined || typeof v["injectPolicyError"] === "boolean");
}

function appendJsonl(path: string, record: Readonly<Record<string, unknown>>): void {
  mkdirSync(dirname(path), { recursive: true });
  appendFileSync(path, `${JSON.stringify({ ts: new Date().toISOString(), ...record })}\n`);
}

export default definePluginEntry({
  id: "pair-spike",
  name: "PAIR Spike",
  description: "Task 1 spike: tool gate, model override, usage capture",
  register(api) {
    // Fail fast on bad config rather than running ungated.
    if (!isSpikeConfig(api.pluginConfig)) {
      throw new Error("pair-spike: invalid plugin config");
    }
    const cfg: SpikeConfig = api.pluginConfig;
    const hookLog = (hook: string, detail: Readonly<Record<string, unknown>>): void => {
      appendJsonl(cfg.hookLogPath, { hook, ...detail });
      api.logger.info(`pair-spike hook=${hook} ${JSON.stringify(detail)}`);
    };

    api.on(
      "before_tool_call",
      (event) => {
        try {
          if (cfg.injectPolicyError === true) throw new Error("injected policy error");
          const decision = decideToolCall(event.toolName, event.params);
          hookLog("before_tool_call", {
            toolName: event.toolName,
            params: event.params,
            decision: decision.kind,
          });
          if (decision.kind === "deny") {
            return { block: true, blockReason: decision.reason };
          }
          if (decision.kind === "approve") {
            return {
              requireApproval: {
                title: decision.title,
                description: decision.description,
                severity: "warning" as const,
                timeoutMs: APPROVAL_TIMEOUT_MS,
              },
            };
          }
          return undefined;
        } catch (err) {
          // Fail closed: any policy error blocks the call.
          hookLog("before_tool_call", { error: String(err), decision: "deny-on-error" });
          return { block: true, blockReason: "pair-spike: policy error, failing closed" };
        }
      },
      { priority: GATE_PRIORITY },
    );

    api.on("before_model_resolve", (event) => {
      hookLog("before_model_resolve", {
        promptPreview: event.prompt.slice(0, PROMPT_PREVIEW_CHARS),
        providerOverride: cfg.overrideProvider,
        modelOverride: cfg.overrideModel,
      });
      return { providerOverride: cfg.overrideProvider, modelOverride: cfg.overrideModel };
    });

    api.on("llm_output", (event) => {
      appendJsonl(cfg.usageLogPath, {
        source: "llm_output",
        runId: event.runId,
        provider: event.provider,
        model: event.model,
        resolvedRef: event.resolvedRef,
        usage: event.usage,
      });
      hookLog("llm_output", { provider: event.provider, model: event.model, usage: event.usage });
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
      hookLog("model_call_ended", { provider: event.provider, model: event.model, outcome: event.outcome });
    });
  },
});
