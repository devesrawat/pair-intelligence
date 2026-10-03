import assert from "node:assert/strict";
import { describe, it } from "node:test";

import { parsePluginConfig } from "../src/plugin-config.ts";

const VALID = {
  usageLogPath: "/tmp/u.jsonl",
  hookLogPath: "/tmp/h.jsonl",
  overrideProvider: "mock",
  overrideModel: "mock-routed",
  taskKind: "default",
  priceVersion: "pv",
  inputPricePerMtokMicros: 1_000_000,
  outputPricePerMtokMicros: 5_000_000,
};

describe("plugin config", () => {
  it("accepts a complete config", () => {
    const r = parsePluginConfig(VALID);
    assert.equal(r.ok, true);
  });

  it("rejects missing, mistyped and out-of-range fields", () => {
    const bad: unknown[] = [
      undefined,
      null,
      [],
      "x",
      {},
      { ...VALID, taskKind: "other" },
      { ...VALID, taskKind: undefined },
      { ...VALID, priceVersion: "" },
      { ...VALID, inputPricePerMtokMicros: 0 },
      { ...VALID, inputPricePerMtokMicros: 1.5 },
      { ...VALID, outputPricePerMtokMicros: "5" },
      { ...VALID, overrideModel: 3 },
      { ...VALID, injectPolicyError: "yes" },
    ];
    for (const raw of bad) assert.equal(parsePluginConfig(raw).ok, false, JSON.stringify(raw));
  });
});
