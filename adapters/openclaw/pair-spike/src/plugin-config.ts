/** Validation of the plugin's own config block (`plugins.entries.pair-spike.config`). Secrets never live here. */
import type { TaskKind } from "./budget.ts";

export type PluginConfig = {
  readonly usageLogPath: string;
  readonly hookLogPath: string;
  readonly overrideProvider: string;
  readonly overrideModel: string;
  readonly taskKind: TaskKind;
  readonly priceVersion: string;
  readonly inputPricePerMtokMicros: number;
  readonly outputPricePerMtokMicros: number;
  /** Test probe only: makes the local tool gate throw so fail-closed behaviour can be observed. */
  readonly injectPolicyError?: boolean;
  /** Tool-call attempts allowed per run; bounds budget overspend from tool-loop length. */
  readonly maxToolCallsPerRun?: number;
};

export type PluginConfigResult =
  | { readonly ok: true; readonly value: PluginConfig }
  | { readonly ok: false; readonly reason: string };

const STRING_KEYS = ["usageLogPath", "hookLogPath", "overrideProvider", "overrideModel", "priceVersion"] as const;
const PRICE_KEYS = ["inputPricePerMtokMicros", "outputPricePerMtokMicros"] as const;
const TASK_KINDS: ReadonlySet<string> = new Set(["default", "research", "coding"]);

export function parsePluginConfig(raw: unknown): PluginConfigResult {
  if (typeof raw !== "object" || raw === null || Array.isArray(raw)) {
    return { ok: false, reason: "plugin config missing" };
  }
  const v = raw as Record<string, unknown>;
  for (const key of STRING_KEYS) {
    const value = v[key];
    if (typeof value !== "string" || value.length === 0) return { ok: false, reason: `plugin config: ${key} must be a non-empty string` };
  }
  for (const key of PRICE_KEYS) {
    const value = v[key];
    if (typeof value !== "number" || !Number.isSafeInteger(value) || value <= 0) {
      return { ok: false, reason: `plugin config: ${key} must be a positive integer` };
    }
  }
  const kind = v["taskKind"];
  if (typeof kind !== "string" || !TASK_KINDS.has(kind)) {
    return { ok: false, reason: "plugin config: taskKind must be default, research or coding" };
  }
  const probe = v["injectPolicyError"];
  if (probe !== undefined && typeof probe !== "boolean") {
    return { ok: false, reason: "plugin config: injectPolicyError must be a boolean" };
  }
  const maxCalls = v["maxToolCallsPerRun"];
  if (maxCalls !== undefined && (typeof maxCalls !== "number" || !Number.isSafeInteger(maxCalls) || maxCalls < 1)) {
    return { ok: false, reason: "plugin config: maxToolCallsPerRun must be a positive integer" };
  }
  return {
    ok: true,
    value: {
      ...(maxCalls === undefined ? {} : { maxToolCallsPerRun: maxCalls }),
      usageLogPath: v["usageLogPath"] as string,
      hookLogPath: v["hookLogPath"] as string,
      overrideProvider: v["overrideProvider"] as string,
      overrideModel: v["overrideModel"] as string,
      taskKind: kind as TaskKind,
      priceVersion: v["priceVersion"] as string,
      inputPricePerMtokMicros: v["inputPricePerMtokMicros"] as number,
      outputPricePerMtokMicros: v["outputPricePerMtokMicros"] as number,
      ...(probe === undefined ? {} : { injectPolicyError: probe }),
    },
  };
}
