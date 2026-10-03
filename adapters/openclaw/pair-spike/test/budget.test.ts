import assert from "node:assert/strict";
import { describe, it } from "node:test";

import {
  actualCost,
  ASSUMED_MAX_OUTPUT_TOKENS,
  ASSUMED_MODEL_CALLS_PER_RUN,
  CHARS_PER_TOKEN,
  createBudgetGate,
  estimateMaxCostMicros,
  SYSTEM_OVERHEAD_TOKENS,
  type BudgetGateOptions,
} from "../src/budget.ts";
import { createPairClient } from "../src/client.ts";
import type { HookLog } from "../src/gate.ts";
import { fail, ok, startMockPair, unreachableUrl, type Script } from "./helpers/mock-pair.ts";

const TOKEN = "b".repeat(32);
const SHORT_TIMEOUT_MS = 200;
const PRICE = { priceVersion: "pv-test", inputPerMtokMicros: 1_000_000, outputPerMtokMicros: 5_000_000 } as const;
const RESERVATION = "0199c0de-aaaa-7abc-8def-0123456789ab";
const CTX = { runId: "run-1", sessionKey: "agent:main:s1" };
const ROUTED = { provider: "mock", model: "mock-routed" } as const;

const reserveOk = (): ReturnType<Script> =>
  ok({ reservation_id: RESERVATION, task_id: "0199c0de-bbbb-7abc-8def-0123456789ab", kind: "default", category: "metered", price_version: "pv-test", max_cost_micros: 1 });
const reconcileOk = (state: string, overrun = false): ReturnType<Script> =>
  ok({ entry_id: "e", reservation_id: RESERVATION, amount_micros: 1, settled: state === "settled", state, overrun });

async function withBudget<T>(
  script: Script,
  fn: (g: ReturnType<typeof createBudgetGate>, mock: Awaited<ReturnType<typeof startMockPair>>) => Promise<T>,
  opts: { baseUrl?: string; log?: HookLog; price?: BudgetGateOptions["price"] } = {},
): Promise<T> {
  const mock = await startMockPair(script);
  const g = createBudgetGate({
    client: createPairClient({ baseUrl: opts.baseUrl ?? mock.url, token: TOKEN }, { timeoutMs: SHORT_TIMEOUT_MS }),
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

describe("estimate and actual cost", () => {
  it("estimate follows the documented constants", () => {
    const promptChars = 4_000;
    const inputTokens = promptChars / CHARS_PER_TOKEN + SYSTEM_OVERHEAD_TOKENS;
    const expected = Math.ceil(
      (ASSUMED_MODEL_CALLS_PER_RUN *
        (inputTokens * PRICE.inputPerMtokMicros + ASSUMED_MAX_OUTPUT_TOKENS * PRICE.outputPerMtokMicros)) /
        1_000_000,
    );
    assert.equal(estimateMaxCostMicros(promptChars, PRICE), expected);
    assert.ok(estimateMaxCostMicros(promptChars, PRICE) > 0);
    assert.ok(estimateMaxCostMicros(8_000, PRICE) > estimateMaxCostMicros(4_000, PRICE));
  });

  it("actual cost rounds up and counts cache tokens as input (conservative)", () => {
    assert.deepEqual(actualCost({ input: 11, output: 7, cacheRead: 0, cacheWrite: 0 }, PRICE), {
      inputTokens: 11,
      outputTokens: 7,
      costMicros: 46, // 11*1 + 7*5 = 46 micros
    });
    assert.equal(actualCost({ input: 1, output: 0, cacheRead: 5, cacheWrite: 4 }, PRICE).inputTokens, 10);
    assert.equal(actualCost({ input: 1, output: 1 }, PRICE).costMicros, 6);
  });

  it("unknown usage has a null cost, never zero", () => {
    for (const usage of [
      undefined,
      null,
      {},
      { input: 1 },
      { output: 1 },
      { input: -1, output: 1 },
      { input: Number.NaN, output: 1 },
      { input: 1.5, output: 1 },
      { input: "11", output: 7 },
      { input: Number.POSITIVE_INFINITY, output: 1 },
    ]) {
      const r = actualCost(usage, PRICE);
      assert.equal(r.costMicros, null, JSON.stringify(usage));
      assert.equal(r.inputTokens, 0);
      assert.equal(r.outputTokens, 0);
    }
  });
});

describe("before_model_resolve: reserve first, override only on success", () => {
  it("reserves with explicit kind, category, price version and returns the override", async () => {
    await withBudget(() => reserveOk(), async (g, mock) => {
      const r = await g.beforeModelResolve({ prompt: "hello" }, CTX);
      assert.deepEqual(r, { providerOverride: "mock", modelOverride: "mock-routed" });
      assert.equal(mock.requests.length, 1);
      const req = mock.requests[0];
      assert.ok(req);
      assert.equal(req.path, "/v1/budget/reserve");
      const body = req.body as Record<string, unknown>;
      assert.equal(body["kind"], "default");
      assert.equal(body["category"], "metered");
      assert.equal(body["price_version"], "pv-test");
      assert.equal(body["max_cost_micros"], estimateMaxCostMicros("hello".length, PRICE));
      assert.match(String(body["task_id"]), /^[0-9a-f-]{36}$/);
      assert.equal(req.headers["x-actor"], "openclaw-adapter");
    });
  });

  it("a refused reserve (402) returns no override and marks the run blocked", async () => {
    await withBudget(() => fail(402, "budget_exceeded"), async (g) => {
      assert.equal(await g.beforeModelResolve({ prompt: "hello" }, CTX), undefined);
      const gate = await g.beforeAgentRun({ prompt: "hello" }, CTX);
      assert.equal(gate.outcome, "block");
    });
  });

  const failures: Array<[string, Script]> = [
    ["http 500", () => fail(500, "internal")],
    ["unknown price 422", () => fail(422, "budget_unknown_price")],
    ["bad json", () => ({ kind: "raw", status: 200, text: "nope" })],
    ["reservation id missing", () => ok({ task_id: "x" })],
    ["timeout", () => ({ kind: "hang" })],
    ["reset", () => ({ kind: "destroy" })],
  ];
  for (const [name, script] of failures) {
    it(`no override and blocked run when reserve fails: ${name}`, async () => {
      await withBudget(script, async (g) => {
        assert.equal(await g.beforeModelResolve({ prompt: "hi" }, CTX), undefined);
        assert.equal((await g.beforeAgentRun({ prompt: "hi" }, CTX)).outcome, "block");
      });
    });
  }

  it("no override and blocked run when the service is unreachable", async () => {
    await withBudget(() => reserveOk(), async (g) => {
      assert.equal(await g.beforeModelResolve({ prompt: "hi" }, CTX), undefined);
      assert.equal((await g.beforeAgentRun({ prompt: "hi" }, CTX)).outcome, "block");
    }, { baseUrl: await unreachableUrl() });
  });

  it("no run key means no reservation and a blocked run", async () => {
    await withBudget(() => reserveOk(), async (g, mock) => {
      assert.equal(await g.beforeModelResolve({ prompt: "hi" }, {}), undefined);
      assert.equal(mock.requests.length, 0);
      assert.equal((await g.beforeAgentRun({ prompt: "hi" }, {})).outcome, "block");
    });
  });

  it("an unwritable log means no reservation attempt, no override, and a blocked run", async () => {
    const boom: HookLog = () => {
      throw new Error("disk on fire");
    };
    await withBudget(() => reserveOk(), async (g, mock) => {
      assert.equal(await g.beforeModelResolve({ prompt: "hi" }, CTX), undefined);
      assert.equal(mock.requests.length, 0);
      assert.equal((await g.beforeAgentRun({ prompt: "hi" }, CTX)).outcome, "block");
    }, { log: boom });
  });

  it("never throws on a hostile event", async () => {
    await withBudget(() => reserveOk(), async (g) => {
      const hostile = {
        get prompt(): string {
          throw new Error("no prompt for you");
        },
      };
      assert.equal(await g.beforeModelResolve(hostile, CTX), undefined);
      assert.equal((await g.beforeAgentRun({ prompt: "x" }, CTX)).outcome, "block");
    });
  });
});

describe("before_agent_run: the model-call gate", () => {
  it("passes only a run with a successful reservation", async () => {
    await withBudget(() => reserveOk(), async (g) => {
      await g.beforeModelResolve({ prompt: "hi" }, CTX);
      assert.deepEqual(await g.beforeAgentRun({ prompt: "hi" }, CTX), { outcome: "pass" });
      // Repeated attempts of the same run keep passing.
      assert.deepEqual(await g.beforeAgentRun({ prompt: "hi" }, CTX), { outcome: "pass" });
    });
  });

  it("blocks a run that never reached before_model_resolve (e.g. locked model selection)", async () => {
    await withBudget(() => reserveOk(), async (g) => {
      assert.equal((await g.beforeAgentRun({ prompt: "hi" }, { runId: "other-run" })).outcome, "block");
    });
  });

  it("block carries an internal reason and a category but no service detail", async () => {
    await withBudget(() => fail(402, "budget_exceeded"), async (g) => {
      await g.beforeModelResolve({ prompt: "hi" }, CTX);
      const r = await g.beforeAgentRun({ prompt: "hi" }, CTX);
      assert.equal(r.outcome, "block");
      if (r.outcome === "block") {
        assert.equal(r.category, "cost_limit");
        assert.ok(r.reason.length > 0);
        assert.equal((r.message ?? "").includes("budget_exceeded"), false);
      }
    });
  });

  it("does not leak one run's reservation to another run", async () => {
    await withBudget(() => reserveOk(), async (g) => {
      await g.beforeModelResolve({ prompt: "hi" }, CTX);
      assert.equal((await g.beforeAgentRun({ prompt: "hi" }, { runId: "run-2" })).outcome, "block");
    });
  });

  it("a second before_model_resolve for the same run reuses the reservation", async () => {
    await withBudget(() => reserveOk(), async (g, mock) => {
      await g.beforeModelResolve({ prompt: "hi" }, CTX);
      const again = await g.beforeModelResolve({ prompt: "hi" }, CTX);
      assert.deepEqual(again, { providerOverride: "mock", modelOverride: "mock-routed" });
      assert.equal(mock.requests.length, 1);
    });
  });
});

describe("llm_output: reconcile", () => {
  const usage = { input: 11, output: 7, cacheRead: 0, cacheWrite: 0, total: 18 };

  it("reconciles the reservation with explicit tokens and cost", async () => {
    await withBudget(
      (path) => (path === "/v1/budget/reserve" ? reserveOk() : reconcileOk("settled")),
      async (g, mock) => {
        await g.beforeModelResolve({ prompt: "hi" }, CTX);
        const r = await g.reconcile({ runId: "run-1", usage, ...ROUTED });
        assert.deepEqual(r, { state: "settled", overrun: false });
        const req = mock.requests[1];
        assert.ok(req);
        assert.equal(req.path, "/v1/budget/reconcile");
        assert.deepEqual(req.body, {
          reservation_id: RESERVATION,
          input_tokens: 11,
          output_tokens: 7,
          actual_cost_micros: 46,
          price_version: "pv-test",
        });
      },
    );
  });

  it("unknown usage reconciles with a null cost (unresolved), never zero", async () => {
    await withBudget(
      (path) => (path === "/v1/budget/reserve" ? reserveOk() : reconcileOk("unresolved")),
      async (g, mock) => {
        await g.beforeModelResolve({ prompt: "hi" }, CTX);
        const r = await g.reconcile({ runId: "run-1", usage: undefined, ...ROUTED });
        assert.deepEqual(r, { state: "unresolved", overrun: false });
        const body = mock.requests[1]?.body as Record<string, unknown>;
        assert.equal(body["actual_cost_micros"], null);
        assert.equal(body["input_tokens"], 0);
      },
    );
  });

  it("does nothing for a run with no reservation, and never throws on service failure", async () => {
    await withBudget(
      (path) => (path === "/v1/budget/reserve" ? reserveOk() : fail(500, "internal")),
      async (g, mock) => {
        assert.equal(await g.reconcile({ runId: "ghost", usage, ...ROUTED }), undefined);
        assert.equal(mock.requests.length, 0);
        await g.beforeModelResolve({ prompt: "hi" }, CTX);
        assert.equal(await g.reconcile({ runId: "run-1", usage, ...ROUTED }), undefined);
      },
    );
  });

  it("a malformed reconcile response is not treated as settled", async () => {
    await withBudget(
      (path) => (path === "/v1/budget/reserve" ? reserveOk() : ok({ state: "settled" })),
      async (g) => {
        await g.beforeModelResolve({ prompt: "hi" }, CTX);
        assert.equal(await g.reconcile({ runId: "run-1", usage, ...ROUTED }), undefined);
      },
    );
  });

  it("reconciles a reservation only once per run", async () => {
    await withBudget(
      (path) => (path === "/v1/budget/reserve" ? reserveOk() : reconcileOk("settled")),
      async (g, mock) => {
        await g.beforeModelResolve({ prompt: "hi" }, CTX);
        await g.reconcile({ runId: "run-1", usage, ...ROUTED });
        assert.equal(await g.reconcile({ runId: "run-1", usage, ...ROUTED }), undefined);
        assert.equal(mock.requests.filter((r) => r.path === "/v1/budget/reconcile").length, 1);
      },
    );
  });
});
