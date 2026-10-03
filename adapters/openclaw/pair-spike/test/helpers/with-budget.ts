import { createBudgetGate, type BudgetGateOptions } from "../../src/budget.ts";
import { createPairClient } from "../../src/client.ts";
import type { HookLog } from "../../src/gate.ts";
import { ok, startMockPair, type MockPair, type Reply, type Script } from "./mock-pair.ts";

export const BUDGET_TOKEN = "b".repeat(32);
export const PRICE = { priceVersion: "pv-test", inputPerMtokMicros: 1_000_000, outputPerMtokMicros: 5_000_000 } as const;
export const RESERVATION = "0199c0de-aaaa-7abc-8def-0123456789ab";
export const CTX = { runId: "run-1", sessionKey: "agent:main:s1" };
export const ROUTED = { provider: "mock", model: "mock-routed" } as const;
const SHORT_TIMEOUT_MS = 200;

export const reserveOk = (): Reply =>
  ok({ reservation_id: RESERVATION, task_id: "0199c0de-bbbb-7abc-8def-0123456789ab", kind: "default", category: "metered", price_version: "pv-test", max_cost_micros: 1 });
export const reconcileOk = (state: string, overrun = false): Reply =>
  ok({ entry_id: "e", reservation_id: RESERVATION, amount_micros: 1, settled: state === "settled", state, overrun });

/** reserve -> reserveOk, reconcile -> reconcileOk(state). */
export const reserveThenReconcile = (state: string): Script => (path) => (path === "/v1/budget/reserve" ? reserveOk() : reconcileOk(state));

export async function withBudget<T>(
  script: Script,
  fn: (g: ReturnType<typeof createBudgetGate>, mock: MockPair) => Promise<T>,
  opts: { log?: HookLog; price?: BudgetGateOptions["price"] } = {},
): Promise<T> {
  const mock = await startMockPair(script);
  const g = createBudgetGate({
    client: createPairClient({ baseUrl: mock.url, token: BUDGET_TOKEN }, { timeoutMs: SHORT_TIMEOUT_MS }),
    log: opts.log ?? (() => {}),
    price: opts.price ?? PRICE,
    taskKind: "default",
    overrideProvider: "mock",
    overrideModel: "mock-routed",
  });
  try {
    return await fn(g, mock);
  } finally {
    await mock.close();
  }
}
