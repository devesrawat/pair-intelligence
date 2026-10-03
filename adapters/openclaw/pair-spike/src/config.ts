/** Adapter connection settings, read from the environment only. */

export type AdapterEnv = Readonly<Record<string, string | undefined>>;

export type PairConnection = {
  readonly baseUrl: string;
  readonly token: string;
};

export type ConnectionResult =
  | { readonly ok: true; readonly value: PairConnection }
  | { readonly ok: false; readonly reason: string };

/** pair-api requires service tokens of at least this many characters. */
const MIN_TOKEN_CHARS = 16;
const ALLOW_REMOTE_FLAG = "1";
const LOOPBACK_V4 = /^127\.\d{1,3}\.\d{1,3}\.\d{1,3}$/;

function isLoopbackHost(hostname: string): boolean {
  return hostname === "localhost" || hostname === "[::1]" || LOOPBACK_V4.test(hostname);
}

/**
 * Validate PAIR_API_URL and PAIR_SERVICE_TOKEN. Refusals never include the token. The service
 * token goes in an Authorization header, so a non-loopback URL is refused unless the operator
 * opts in with PAIR_ADAPTER_ALLOW_REMOTE=1.
 */
export function loadConnection(env: AdapterEnv): ConnectionResult {
  const rawUrl = env["PAIR_API_URL"];
  if (rawUrl === undefined || rawUrl.trim() === "") {
    return { ok: false, reason: "PAIR_API_URL is not set" };
  }
  let url: URL;
  try {
    url = new URL(rawUrl);
  } catch {
    return { ok: false, reason: "PAIR_API_URL is not a valid URL" };
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") {
    return { ok: false, reason: "PAIR_API_URL must be http or https" };
  }
  if (url.username !== "" || url.password !== "") {
    return { ok: false, reason: "PAIR_API_URL must not embed credentials" };
  }
  if (url.pathname !== "/" || url.search !== "" || url.hash !== "") {
    return { ok: false, reason: "PAIR_API_URL must be a bare origin (no path, query or fragment)" };
  }
  if (!isLoopbackHost(url.hostname)) {
    if (env["PAIR_ADAPTER_ALLOW_REMOTE"] !== ALLOW_REMOTE_FLAG) {
      return {
        ok: false,
        reason: "PAIR_API_URL is not loopback; set PAIR_ADAPTER_ALLOW_REMOTE=1 to allow a remote https service",
      };
    }
    if (url.protocol !== "https:") {
      return { ok: false, reason: "PAIR_API_URL is not loopback and must be https (the bearer token would be sent in clear)" };
    }
  }
  const token = env["PAIR_SERVICE_TOKEN"];
  if (token === undefined || token.length < MIN_TOKEN_CHARS) {
    return { ok: false, reason: `PAIR_SERVICE_TOKEN is not set or shorter than ${MIN_TOKEN_CHARS} characters` };
  }
  return { ok: true, value: { baseUrl: url.origin, token } };
}
