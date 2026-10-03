import { createPairClient } from "../../src/client.ts";
import { createToolGate, type GateResult, type HookLog } from "../../src/gate.ts";
import { createPairToolGate, type PairGateOptions } from "../../src/pair-gate.ts";
import { startMockPair, type MockPair, type Script } from "./mock-pair.ts";

export const TOKEN = "g".repeat(32);
export const RUN = "run-1";
const SHORT_TIMEOUT_MS = 200;
const APPROVAL_TIMEOUT_MS = 3_000;

export type LogLine = { readonly hook: string; readonly detail: Readonly<Record<string, unknown>> };

export async function withGate<T>(
  script: Script,
  fn: (g: ReturnType<typeof createPairToolGate>, mock: MockPair, logs: LogLine[]) => Promise<T>,
  opts: { baseUrl?: string; extra?: Partial<PairGateOptions> } = {},
): Promise<T> {
  const mock = await startMockPair(script);
  const logs: LogLine[] = [];
  const log: HookLog = (hook, detail) => void logs.push({ hook, detail });
  const client = createPairClient({ baseUrl: opts.baseUrl ?? mock.url, token: TOKEN }, { timeoutMs: SHORT_TIMEOUT_MS });
  const gate = createPairToolGate({
    client,
    log,
    localGate: createToolGate({ log, approvalTimeoutMs: APPROVAL_TIMEOUT_MS }),
    approvalTimeoutMs: APPROVAL_TIMEOUT_MS,
    ...opts.extra,
  });
  try {
    return await fn(gate, mock, logs);
  } finally {
    await mock.close();
  }
}

export const ev = (toolName: string, params: Record<string, unknown> = {}, runId: string = RUN) => ({ toolName, params, runId });
export const isBlock = (r: GateResult): boolean => r !== undefined && "block" in r && r.block === true;
