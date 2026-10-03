/** Minimal pair-api client. Every failure is a value: `post` never throws and never hangs past the timeout. */
import type { PairConnection } from "./config.ts";

export const ACTOR = "openclaw-adapter";
export const REQUEST_TIMEOUT_MS = 3_000;

export type CallFailure = {
  readonly ok: false;
  readonly kind: "unreachable" | "timeout" | "http" | "malformed";
  readonly status?: number;
  /** pair-api error code (e.g. `budget_exceeded`) when the error envelope was readable. */
  readonly errorCode?: string;
};

export type CallResult = { readonly ok: true; readonly data: unknown } | CallFailure;

export type PairClient = {
  readonly post: (path: string, body: unknown, traceId: string) => Promise<CallResult>;
};

export type ClientOptions = {
  readonly timeoutMs?: number;
  readonly fetchImpl?: typeof fetch;
};

const ENVELOPE_KEYS = ["data", "error", "success"];

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function errorCodeOf(envelope: unknown): string | undefined {
  if (!isRecord(envelope) || !isRecord(envelope["error"])) return undefined;
  const code = envelope["error"]["code"];
  return typeof code === "string" ? code : undefined;
}

/** Strict success envelope: exactly `{success: true, data, error: null}`. Anything else is malformed. */
function parseSuccess(envelope: unknown): CallResult {
  if (!isRecord(envelope)) return { ok: false, kind: "malformed" };
  const keys = Object.keys(envelope).sort();
  const shapeOk =
    keys.length === ENVELOPE_KEYS.length &&
    keys.every((k, i) => k === ENVELOPE_KEYS[i]) &&
    envelope["success"] === true &&
    envelope["error"] === null &&
    envelope["data"] !== null &&
    envelope["data"] !== undefined;
  return shapeOk ? { ok: true, data: envelope["data"] } : { ok: false, kind: "malformed" };
}

function isTimeout(err: unknown): boolean {
  return err instanceof Error && (err.name === "TimeoutError" || err.name === "AbortError");
}

export function createPairClient(connection: PairConnection, options: ClientOptions = {}): PairClient {
  const timeoutMs = options.timeoutMs ?? REQUEST_TIMEOUT_MS;
  const doFetch = options.fetchImpl ?? fetch;
  return {
    post: async (path, body, traceId) => {
      try {
        const response = await doFetch(`${connection.baseUrl}${path}`, {
          method: "POST",
          redirect: "error",
          signal: AbortSignal.timeout(timeoutMs),
          headers: {
            "content-type": "application/json",
            authorization: `Bearer ${connection.token}`,
            "x-actor": ACTOR,
            "x-trace-id": traceId,
          },
          body: JSON.stringify(body),
        });
        // The body read is inside the same abort signal, so a stalled body also times out.
        const text = await response.text();
        let parsed: unknown;
        try {
          parsed = JSON.parse(text);
        } catch {
          return { ok: false, kind: "malformed", status: response.status };
        }
        if (!response.ok) {
          const code = errorCodeOf(parsed);
          return {
            ok: false,
            kind: "http",
            status: response.status,
            ...(code === undefined ? {} : { errorCode: code }),
          };
        }
        return parseSuccess(parsed);
      } catch (err) {
        return { ok: false, kind: isTimeout(err) ? "timeout" : "unreachable" };
      }
    },
  };
}
