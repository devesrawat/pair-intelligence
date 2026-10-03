/** Strict parsing of pair-api response payloads. Anything unrecognised is `undefined` (callers fail closed). */

export type AuthorizeDecision =
  | { readonly kind: "allow" }
  | { readonly kind: "deny"; readonly reason: string }
  | { readonly kind: "needs_approval"; readonly payloadHash: string };

export type ReserveOutcome = { readonly reservationId: string; readonly taskId: string };

export type ReconcileOutcome = {
  readonly state: "settled" | "unresolved";
  readonly overrun: boolean;
};

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

const nonEmpty = (v: unknown): v is string => typeof v === "string" && v.length > 0;

/** `data` of POST /v1/policy/authorize: `{decision: {decision: allow|deny|needs_approval, ...}, policy_version}`. */
export function parseAuthorize(data: unknown): AuthorizeDecision | undefined {
  if (!isRecord(data) || !isRecord(data["decision"]) || !nonEmpty(data["policy_version"])) return undefined;
  const d = data["decision"];
  switch (d["decision"]) {
    case "allow":
      return { kind: "allow" };
    case "deny":
      return nonEmpty(d["reason"]) ? { kind: "deny", reason: d["reason"] } : undefined;
    case "needs_approval":
      return nonEmpty(d["payload_hash"]) ? { kind: "needs_approval", payloadHash: d["payload_hash"] } : undefined;
    default:
      return undefined;
  }
}

export function parseReserve(data: unknown): ReserveOutcome | undefined {
  if (!isRecord(data) || !nonEmpty(data["reservation_id"]) || !nonEmpty(data["task_id"])) return undefined;
  return { reservationId: data["reservation_id"], taskId: data["task_id"] };
}

export function parseReconcile(data: unknown): ReconcileOutcome | undefined {
  if (!isRecord(data)) return undefined;
  const state = data["state"];
  if ((state !== "settled" && state !== "unresolved") || typeof data["overrun"] !== "boolean") return undefined;
  return { state, overrun: data["overrun"] };
}
