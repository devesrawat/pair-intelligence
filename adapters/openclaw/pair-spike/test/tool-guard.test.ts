import assert from "node:assert/strict";
import { describe, it } from "node:test";

import { PAIR_TOOL_LIMIT_REASON } from "../src/pair-gate.ts";
import { allow } from "./helpers/mock-pair.ts";
import { ev, isBlock, withGate } from "./helpers/with-gate.ts";

const CAT = { command: "cat /w/hello.txt" };
const LIMIT = 3;

describe("per-run tool-call guard (H2)", () => {
  it("blocks tool calls beyond the per-run limit without consulting the service", async () => {
    await withGate(() => allow(), async (gate, mock, logs) => {
      for (let i = 0; i < LIMIT; i += 1) assert.equal(await gate(ev("exec", CAT)), undefined, `call ${i + 1}`);
      const over = await gate(ev("exec", CAT));
      assert.deepEqual(over, { block: true, blockReason: PAIR_TOOL_LIMIT_REASON });
      assert.equal(mock.requests.length, LIMIT);
      assert.ok(logs.some((l) => l.detail["decision"] === "deny" && l.detail["source"] === "adapter"));
    }, { extra: { maxToolCallsPerRun: LIMIT } });
  });

  it("counts per run, so another run starts fresh", async () => {
    await withGate(() => allow(), async (gate) => {
      for (let i = 0; i < LIMIT; i += 1) await gate(ev("exec", CAT, "run-a"));
      assert.ok(isBlock(await gate(ev("exec", CAT, "run-a"))));
      assert.equal(await gate(ev("exec", CAT, "run-b")), undefined);
    }, { extra: { maxToolCallsPerRun: LIMIT } });
  });

  it("counts denied and blocked attempts too (a looping model cannot dodge the guard)", async () => {
    await withGate(() => allow(), async (gate, mock) => {
      for (let i = 0; i < LIMIT; i += 1) assert.ok(isBlock(await gate(ev("exec", { command: "rm -rf /tmp/x" }))));
      assert.ok(isBlock(await gate(ev("exec", CAT))));
      assert.equal(mock.requests.length, 0);
    }, { extra: { maxToolCallsPerRun: LIMIT } });
  });
});
