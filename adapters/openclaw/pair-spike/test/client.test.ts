import assert from "node:assert/strict";
import { describe, it } from "node:test";

import { createPairClient } from "../src/client.ts";
import { fail, ok, startMockPair, unreachableUrl } from "./helpers/mock-pair.ts";

const TOKEN = "k".repeat(32);
const TRACE = "0199c0de-1234-7abc-8def-0123456789ab";
const SHORT_TIMEOUT_MS = 200;

describe("client", () => {
  it("sends bearer token, actor and trace id and returns the envelope data", async () => {
    const mock = await startMockPair(() => ok({ hello: 1 }));
    try {
      const client = createPairClient({ baseUrl: mock.url, token: TOKEN });
      const result = await client.post("/v1/policy/authorize", { a: 1 }, TRACE);
      assert.deepEqual(result, { ok: true, data: { hello: 1 } });
      const req = mock.requests[0];
      assert.ok(req);
      assert.equal(req.headers["authorization"], `Bearer ${TOKEN}`);
      assert.equal(req.headers["x-actor"], "openclaw-adapter");
      assert.equal(req.headers["x-trace-id"], TRACE);
      assert.deepEqual(req.body, { a: 1 });
    } finally {
      await mock.close();
    }
  });

  it("reports unreachable, timeout, http (with error code), and malformed as failures", async () => {
    const dead = createPairClient({ baseUrl: await unreachableUrl(), token: TOKEN }, { timeoutMs: SHORT_TIMEOUT_MS });
    assert.equal((await dead.post("/x", {}, TRACE)).ok, false);
    assert.equal((await dead.post("/x", {}, TRACE) as { kind: string }).kind, "unreachable");

    const cases: Array<[string, Parameters<typeof startMockPair>[0], string, string?]> = [
      ["hang", () => ({ kind: "hang" }), "timeout"],
      ["500", () => ({ kind: "json", status: 500, body: { success: false, data: null, error: { code: "internal" } } }), "http", "internal"],
      ["402 budget", () => fail(402, "budget_exceeded"), "http", "budget_exceeded"],
      ["not json", () => ({ kind: "raw", status: 200, text: "<html>" }), "malformed"],
      ["empty body", () => ({ kind: "raw", status: 200, text: "" }), "malformed"],
      ["success false", () => ({ kind: "json", status: 200, body: { success: false, data: {}, error: null } }), "malformed"],
      ["data null", () => ({ kind: "json", status: 200, body: { success: true, data: null, error: null } }), "malformed"],
      ["extra key", () => ({ kind: "json", status: 200, body: { success: true, data: {}, error: null, extra: 1 } }), "malformed"],
      ["array", () => ({ kind: "json", status: 200, body: [] }), "malformed"],
      ["destroyed socket", () => ({ kind: "destroy" }), "unreachable"],
      ["redirect", () => ({ kind: "raw", status: 302, text: "" }), "unreachable"],
    ];
    for (const [name, script, kind, code] of cases) {
      const mock = await startMockPair(script);
      try {
        const client = createPairClient({ baseUrl: mock.url, token: TOKEN }, { timeoutMs: SHORT_TIMEOUT_MS });
        const result = await client.post("/x", {}, TRACE);
        assert.equal(result.ok, false, name);
        if (!result.ok) {
          assert.equal(result.kind, kind, name);
          assert.equal(result.errorCode, code, name);
        }
      } finally {
        await mock.close();
      }
    }
  });

  it("never throws even when fetch throws a non-Error", async () => {
    const client = createPairClient(
      { baseUrl: "http://127.0.0.1:1", token: TOKEN },
      {
        fetchImpl: () => {
          // eslint-disable-next-line @typescript-eslint/only-throw-error
          throw "boom";
        },
      },
    );
    const result = await client.post("/x", {}, TRACE);
    assert.equal(result.ok, false);
  });
});
