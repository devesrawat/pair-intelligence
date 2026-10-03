/**
 * `before_tool_call` gate backed by pair-api. Order: local rules first (a local deny wins and the
 * service is not consulted), then the explicit tool mapping, then POST /v1/policy/authorize.
 * Every failure blocks; the error path never throws.
 */
import type { PairClient } from "./client.ts";
import { parseAuthorize } from "./decision.ts";
import { APPROVAL_DECISIONS, FAIL_CLOSED_REASON, type GateResult, type HookLog, type ToolCallEvent } from "./gate.ts";
import { newTraceId, paramsHash, taskIdFor } from "./ids.ts";
import { mapToolCall } from "./mapping.ts";

export const AUTHORIZE_PATH = "/v1/policy/authorize";
export const PAIR_UNAVAILABLE_REASON = "pair: policy service unavailable, failing closed";
export const PAIR_UNMAPPED_REASON = "pair: tool call cannot be mapped to a PAIR action, denied";
export const PAIR_BAD_RESPONSE_REASON = "pair: unrecognised policy response, failing closed";
const DENY_REASON_PREFIX = "pair policy: ";
const REASON_MAX_CHARS = 300;
/** Calls whose class and data are not otherwise known are treated as personal data. */
const DEFAULT_DATA_CLASS = "personal";

export type PairGateOptions = {
  readonly client: PairClient;
  readonly log: HookLog;
  /** Local rules (`createToolGate`). Its verdict is final when it blocks. */
  readonly localGate: (event: ToolCallEvent) => GateResult;
  readonly approvalTimeoutMs: number;
};

const block = (blockReason: string): GateResult => ({ block: true, blockReason });

function approval(options: PairGateOptions, title: string, description: string): GateResult {
  return {
    requireApproval: {
      title,
      description,
      severity: "warning",
      timeoutMs: options.approvalTimeoutMs,
      allowedDecisions: [...APPROVAL_DECISIONS],
    },
  };
}

export function createPairToolGate(options: PairGateOptions): (event: ToolCallEvent) => Promise<GateResult> {
  return async (event) => {
    try {
      return await decide(options, event);
    } catch (err) {
      try {
        options.log("before_tool_call", { decision: "deny-on-error", error: String(err) });
      } catch {
        // Logging must never change the verdict, and must never throw out of a security gate.
      }
      return block(FAIL_CLOSED_REASON);
    }
  };
}

async function decide(options: PairGateOptions, event: ToolCallEvent): Promise<GateResult> {
  const local = options.localGate(event);
  if (local !== undefined && "block" in local) return local;

  const mapped = mapToolCall(event.toolName, event.params);
  const base = { toolName: event.toolName, paramsSha256: paramsHash(event.params), runId: event.runId };
  if (!mapped.ok) {
    options.log("before_tool_call", { ...base, source: "adapter", decision: "deny", reason: mapped.reason });
    return block(PAIR_UNMAPPED_REASON);
  }

  const taskId = taskIdFor(event.runId ?? newTraceId());
  const traceId = newTraceId();
  const result = await options.client.post(
    AUTHORIZE_PATH,
    { ...mapped.action, data_class: DEFAULT_DATA_CLASS, task_id: taskId },
    traceId,
  );
  const ids = { ...base, pairTool: mapped.action.tool, taskId, traceId, source: "pair-api" };
  if (!result.ok) {
    options.log("before_tool_call", { ...ids, decision: "deny-on-error", errorKind: result.kind, status: result.status });
    return block(PAIR_UNAVAILABLE_REASON);
  }
  const verdict = parseAuthorize(result.data);
  if (verdict === undefined) {
    options.log("before_tool_call", { ...ids, decision: "deny-on-error", errorKind: "unknown-decision" });
    return block(PAIR_BAD_RESPONSE_REASON);
  }

  // Audit before acting: a log that cannot be written makes the caller's catch block deny.
  if (verdict.kind === "deny") {
    options.log("before_tool_call", { ...ids, decision: "deny", reason: verdict.reason.slice(0, REASON_MAX_CHARS) });
    return block(`${DENY_REASON_PREFIX}${verdict.reason.slice(0, REASON_MAX_CHARS)}`);
  }
  if (verdict.kind === "needs_approval") {
    options.log("before_tool_call", { ...ids, decision: "needs_approval", payloadHash: verdict.payloadHash });
    return approval(
      options,
      `PAIR approval: ${mapped.action.tool}`,
      `${event.toolName} needs approval (payload_hash=${verdict.payloadHash})`,
    );
  }
  if (local !== undefined && "requireApproval" in local) {
    options.log("before_tool_call", { ...ids, decision: "needs_approval", reason: "local rule" });
    return local;
  }
  options.log("before_tool_call", { ...ids, decision: "allow" });
  return undefined;
}
