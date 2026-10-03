/**
 * Cost estimate for the reservation and cost computation for reconcile.
 *
 * The estimate is a per-run ESTIMATE, not a bound: it assumes ASSUMED_MODEL_CALLS_PER_RUN model
 * calls of a fixed size. History and tool output are not visible at `before_model_resolve`, so a
 * long tool loop can realise more spend than the hold (the cap is enforced on holds). The
 * per-run tool-call guard in `pair-gate.ts` limits how far a run can go.
 */

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
/** Prompt characters counted at most; a larger prompt is truncated by the model's context anyway. */
export const MAX_PROMPT_CHARS = 400_000;
/** Input tokens assumed per attachment (only the kind is visible to the hook, not the size). */
export const ATTACHMENT_TOKENS = 2_000;
export const MAX_ATTACHMENTS_COUNTED = 16;
const MICROS_PER_MTOK_DIVISOR = 1_000_000;

export type PriceConfig = {
  readonly priceVersion: string;
  readonly inputPerMtokMicros: number;
  readonly outputPerMtokMicros: number;
};

export function estimateMaxCostMicros(promptChars: number, price: PriceConfig, attachmentCount = 0): number {
  const chars = Math.min(Math.max(promptChars, 0), MAX_PROMPT_CHARS);
  const attachments = Math.min(Math.max(attachmentCount, 0), MAX_ATTACHMENTS_COUNTED);
  const inputTokens = Math.ceil(chars / CHARS_PER_TOKEN) + SYSTEM_OVERHEAD_TOKENS + attachments * ATTACHMENT_TOKENS;
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
