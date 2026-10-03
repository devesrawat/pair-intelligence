import assert from "node:assert/strict";
import { describe, it } from "node:test";

import { createPairClient, MAX_RESPONSE_BYTES } from "../src/client.ts";
import { ok, startMockPair } from "./helpers/mock-pair.ts";

const TOKEN = "c".repeat(32);

describe("client response size cap", () => {
  it("response_over_the_cap_is_a_malformed_failure", async () => {
    const mock = await startMockPair(() => ({ kind: "raw", status: 200, text: "x".repeat(MAX_RESPONSE_BYTES + 10) }));
    try {
      const client = createPairClient({ baseUrl: mock.url, token: TOKEN });
      const r = await client.post("/x", {}, "t");
      assert.deepEqual(r, { ok: false, kind: "malformed", status: 200 });
    } finally {
      await mock.close();
    }
  });

  it("a normal response still parses", async () => {
    const mock = await startMockPair(() => ok({ a: 1 }));
    try {
      const r = await createPairClient({ baseUrl: mock.url, token: TOKEN }).post("/x", {}, "t");
      assert.deepEqual(r, { ok: true, data: { a: 1 } });
    } finally {
      await mock.close();
    }
  });
});
