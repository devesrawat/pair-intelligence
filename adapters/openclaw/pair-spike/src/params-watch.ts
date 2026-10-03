/**
 * Detects a params rewrite between PAIR's verdict and execution. Another plugin's
 * `before_tool_call` hook that runs after ours may return `params` and change what executes after
 * PAIR approved something else (OpenClaw hooks.ts merges `params` across plugins). The adapter
 * cannot prevent that, so it records the hash PAIR approved and compares at `after_tool_call`.
 * `pair-spike` must be the only plugin returning `params` from `before_tool_call`.
 */

const WATCH_CAP = 2_000;

export type ParamsWatch = {
  /** Remember the hash of the params PAIR allowed for this tool call. */
  readonly approved: (toolCallId: string | undefined, paramsSha256: string) => void;
  /** `mismatch` only when this call was approved here and the executed params hash differs. */
  readonly check: (toolCallId: string | undefined, paramsSha256: string) => "match" | "mismatch" | "unknown";
};

export function createParamsWatch(): ParamsWatch {
  const approved = new Map<string, string>();
  return {
    approved: (toolCallId, paramsSha256) => {
      if (toolCallId === undefined) return;
      approved.delete(toolCallId);
      approved.set(toolCallId, paramsSha256);
      for (const oldest of approved.keys()) {
        if (approved.size <= WATCH_CAP) break;
        approved.delete(oldest);
      }
    },
    check: (toolCallId, paramsSha256) => {
      if (toolCallId === undefined) return "unknown";
      const expected = approved.get(toolCallId);
      if (expected === undefined) return "unknown";
      approved.delete(toolCallId);
      return expected === paramsSha256 ? "match" : "mismatch";
    },
  };
}
