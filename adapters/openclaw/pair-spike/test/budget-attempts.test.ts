import assert from "node:assert/strict";
import { describe, it } from "node:test";

import {
  ASSUMED_MODEL_CALLS_PER_RUN,
  ATTACHMENT_TOKENS,
  estimateMaxCostMicros,
  MAX_ATTACHMENTS_COUNTED,
  MAX_PROMPT_CHARS,
} from "../src/budget.ts";
import { CTX, PRICE, ROUTED, reserveThenReconcile, withBudget } from "./helpers/with-budget.ts";

const USAGE = { input: 11, output: 7, cacheRead: 0, cacheWrite: 0 };

describe("attempts within one run (H1)", () => {
  it("second_attempt_without_reservation_is_blocked", async () => {
    await withBudget(reserveThenReconcile("settled"), async (g, mock) => {
      await g.beforeModelResolve({ prompt: "hi" }, CTX);
      assert.deepEqual(await g.beforeAgentRun({}, CTX), { outcome: "pass" });
      await g.reconcile({ runId: "run-1", usage: USAGE, ...ROUTED });
      // Attempt 2 (compaction retry, auth rotation, ...): before_model_resolve does not fire again
      // and the hold is gone, so the attempt would be unreserved and unrecorded spend.
      const second = await g.beforeAgentRun({}, CTX);
      assert.equal(second.outcome, "block");
      if (second.outcome === "block") assert.match(second.reason, /already reconciled/);
      assert.equal(mock.requests.filter((r) => r.path === "/v1/budget/reserve").length, 1);
      assert.equal(mock.requests.filter((r) => r.path === "/v1/budget/reconcile").length, 1);
    });
  });

  it("a repeated before_model_resolve after reconcile neither re-reserves nor overrides", async () => {
    await withBudget(reserveThenReconcile("settled"), async (g, mock) => {
      await g.beforeModelResolve({ prompt: "hi" }, CTX);
      await g.reconcile({ runId: "run-1", usage: USAGE, ...ROUTED });
      assert.equal(await g.beforeModelResolve({ prompt: "hi" }, CTX), undefined);
      assert.equal(mock.requests.filter((r) => r.path === "/v1/budget/reserve").length, 1);
      assert.equal((await g.beforeAgentRun({}, CTX)).outcome, "block");
    });
  });

  it("failover_model_reconciles_with_unknown_cost", async () => {
    await withBudget(reserveThenReconcile("unresolved"), async (g, mock) => {
      await g.beforeModelResolve({ prompt: "hi" }, CTX);
      const r = await g.reconcile({ runId: "run-1", usage: USAGE, provider: "mock", model: "mock-fallback" });
      assert.deepEqual(r, { state: "unresolved", overrun: false });
      const body = mock.requests[1]?.body as Record<string, unknown>;
      assert.equal(body["actual_cost_micros"], null);
    });
  });

  it("a different provider with the same model name is also a failover", async () => {
    await withBudget(reserveThenReconcile("unresolved"), async (g, mock) => {
      await g.beforeModelResolve({ prompt: "hi" }, CTX);
      await g.reconcile({ runId: "run-1", usage: USAGE, provider: "other", model: "mock-routed" });
      assert.equal((mock.requests[1]?.body as Record<string, unknown>)["actual_cost_micros"], null);
    });
  });

  it("the routed model keeps its computed cost", async () => {
    await withBudget(reserveThenReconcile("settled"), async (g, mock) => {
      await g.beforeModelResolve({ prompt: "hi" }, CTX);
      await g.reconcile({ runId: "run-1", usage: USAGE, ...ROUTED });
      assert.equal((mock.requests[1]?.body as Record<string, unknown>)["actual_cost_micros"], 46);
    });
  });
});

describe("estimate input is clamped and counts attachments (H2)", () => {
  it("a huge prompt cannot inflate the hold beyond the clamp", () => {
    const atCap = estimateMaxCostMicros(MAX_PROMPT_CHARS, PRICE);
    assert.equal(estimateMaxCostMicros(MAX_PROMPT_CHARS * 50, PRICE), atCap);
    assert.ok(atCap > estimateMaxCostMicros(1_000, PRICE));
  });

  it("each attachment adds tokens, up to a cap", () => {
    const none = estimateMaxCostMicros(100, PRICE, 0);
    const one = estimateMaxCostMicros(100, PRICE, 1);
    assert.equal(one - none, ASSUMED_MODEL_CALLS_PER_RUN * ATTACHMENT_TOKENS);
    assert.equal(estimateMaxCostMicros(100, PRICE, MAX_ATTACHMENTS_COUNTED + 30), estimateMaxCostMicros(100, PRICE, MAX_ATTACHMENTS_COUNTED));
  });

  it("before_model_resolve sends the attachment-aware estimate", async () => {
    await withBudget(reserveThenReconcile("settled"), async (g, mock) => {
      await g.beforeModelResolve({ prompt: "hi", attachments: [{ kind: "image" }, { kind: "document" }] }, CTX);
      const body = mock.requests[0]?.body as Record<string, unknown>;
      assert.equal(body["max_cost_micros"], estimateMaxCostMicros(2, PRICE, 2));
    });
  });
});
