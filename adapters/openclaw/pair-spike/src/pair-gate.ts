/**
 * `before_tool_call` gate backed by pair-api. Order: per-run tool-call guard, local rules (a local
 * deny wins and the service is not consulted), the explicit tool mapping, then
 * POST /v1/policy/authorize. Every failure blocks; the error path never throws.
 *
 * A PAIR `needs_approval` blocks: the adapter cannot mint a PAIR approval (that needs a second
 * credential and a human), and an OpenClaw allow-once is not one. `git.push` is never allowed by
 * the adapter, whatever the service says.
 */
import type { PairClient } from "./client.ts";
import { parseAuthorize } from "./decision.ts";
import { APPROVAL_DECISIONS, FAIL_CLOSED_REASON, type GateResult, type HookLog, type ToolCallEvent } from "./gate.ts";
import { newTraceId, paramsHash, taskIdFor } from "./ids.ts";
import { mapToolCall } from "./mapping.ts";
import { scrubPaths } from "./scrub.ts";

export const AUTHORIZE_PATH = "/v1/policy/authorize";
export const PAIR_UNAVAILABLE_REASON = "pair: policy service unavailable, failing closed";
export const PAIR_UNMAPPED_REASON = "pair: tool call cannot be mapped to a PAIR action, denied";
export const PAIR_BAD_RESPONSE_REASON = "pair: unrecognised policy response, failing closed";
export const PAIR_APPROVAL_UNAVAILABLE_REASON = "pair: this action needs a PAIR approval, which this adapter cannot obtain yet; denied";
export const PAIR_TOOL_LIMIT_REASON = "pair: per-run tool call limit reached, denied";
const DENY_REASON_PREFIX = "pair policy: ";
const REASON_MAX_CHARS = 300;
/** Calls whose class and data are not otherwise known are treated as personal data. */
const DEFAULT_DATA_CLASS = "personal";
/** Tool calls one run may attempt (allowed or not). Bounds budget overspend from tool-loop length. */
export const DEFAULT_MAX_TOOL_CALLS_PER_RUN = 10;
const NO_RUN_KEY = "<no-run-id>";
const RUN_COUNTER_CAP = 2_000;
const GIT_PUSH_TOOL = "git.push";

export type PairGateOptions = {
  readonly client: PairClient;
  readonly log: HookLog;
  /** Local rules (`createToolGate`). Its verdict is final when it blocks. */
  readonly localGate: (event: ToolCallEvent) => GateResult;
  readonly approvalTimeoutMs: number;
  /** Tool-call attempts per run before every further call is blocked. */
  readonly maxToolCallsPerRun?: number;
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
  const limit = options.maxToolCallsPerRun ?? DEFAULT_MAX_TOOL_CALLS_PER_RUN;
  const counts = new Map<string, number>();
  const overLimit = (runId: string | undefined): boolean => {
    const key = runId ?? NO_RUN_KEY;
    const next = (counts.get(key) ?? 0) + 1;
    counts.delete(key);
    counts.set(key, next);
    for (const oldest of counts.keys()) {
      if (counts.size <= RUN_COUNTER_CAP) break;
      counts.delete(oldest);
    }
    return next > limit;
  };
  return async (event) => {
    try {
      if (overLimit(event.runId)) {
        options.log("before_tool_call", {
          toolName: event.toolName,
          runId: event.runId,
          source: "adapter",
          decision: "deny",
          reason: "tool call limit",
        });
        return block(PAIR_TOOL_LIMIT_REASON);
      }
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
    const reason = scrubPaths(verdict.reason).slice(0, REASON_MAX_CHARS);
    options.log("before_tool_call", { ...ids, decision: "deny", reason });
    return block(`${DENY_REASON_PREFIX}${reason}`);
  }
  if (verdict.kind === "needs_approval") {
    options.log("before_tool_call", { ...ids, decision: "needs_approval", payloadHash: verdict.payloadHash, outcome: "blocked" });
    return block(PAIR_APPROVAL_UNAVAILABLE_REASON);
  }
  if (mapped.action.tool === GIT_PUSH_TOOL) {
    options.log("before_tool_call", { ...ids, decision: "deny", reason: "git.push allow is not honoured" });
    return block(PAIR_APPROVAL_UNAVAILABLE_REASON);
  }
  if (local !== undefined && "requireApproval" in local) {
    options.log("before_tool_call", { ...ids, decision: "needs_approval", reason: "local rule" });
    return local;
  }
  options.log("before_tool_call", { ...ids, decision: "allow" });
  return undefined;
}
