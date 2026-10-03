import { paramsHash } from "./ids.ts";
import { decideToolCall } from "./rules.ts";

export type ToolCallEvent = {
  readonly toolName: string;
  readonly params: Readonly<Record<string, unknown>>;
  readonly runId?: string;
};

export type GateResult =
  | { readonly block: true; readonly blockReason: string }
  | {
      readonly requireApproval: {
        readonly title: string;
        readonly description: string;
        readonly severity: "warning";
        readonly timeoutMs: number;
        /** `allow-always` is excluded: an approval must stay bound to one call. */
        readonly allowedDecisions: Array<"allow-once" | "deny">;
      };
    }
  | undefined;

export type HookLog = (hook: string, detail: Readonly<Record<string, unknown>>) => void;

export type GateOptions = {
  readonly log: HookLog;
  readonly approvalTimeoutMs: number;
  /** Test probe only: makes the gate throw so fail-closed behaviour can be observed. */
  readonly injectPolicyError?: boolean;
};

export const APPROVAL_DECISIONS = ["allow-once", "deny"] as const;

export const FAIL_CLOSED_REASON = "pair-spike: policy error, failing closed";

/**
 * Build the `before_tool_call` handler. Fail closed: any error while deciding or logging
 * (including an unwritable log path) blocks the call, and the error path itself can never throw.
 */
export function createToolGate(options: GateOptions): (event: ToolCallEvent) => GateResult {
  return (event) => {
    try {
      if (options.injectPolicyError === true) throw new Error("injected policy error");
      const decision = decideToolCall(event.toolName, event.params);
      // Logged before acting on the verdict: if the audit trail cannot be written, deny.
      options.log("before_tool_call", {
        toolName: event.toolName,
        paramsSha256: paramsHash(event.params),
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
            severity: "warning",
            timeoutMs: options.approvalTimeoutMs,
            allowedDecisions: [...APPROVAL_DECISIONS],
          },
        };
      }
      return undefined;
    } catch (err) {
      try {
        options.log("before_tool_call", { error: String(err), decision: "deny-on-error" });
      } catch {
        // Logging must never change the verdict (and must never throw out of a security gate).
      }
      return { block: true, blockReason: FAIL_CLOSED_REASON };
    }
  };
}
