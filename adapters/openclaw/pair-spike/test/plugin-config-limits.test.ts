import assert from "node:assert/strict";
import { describe, it } from "node:test";

import { parsePluginConfig } from "../src/plugin-config.ts";

const BASE = {
  usageLogPath: "/u",
  hookLogPath: "/h",
  overrideProvider: "p",
  overrideModel: "m",
  taskKind: "default",
  priceVersion: "v",
  inputPricePerMtokMicros: 1,
  outputPricePerMtokMicros: 1,
};

describe("maxToolCallsPerRun", () => {
  it("is optional and validated as a positive integer", () => {
    const absent = parsePluginConfig(BASE);
    assert.ok(absent.ok && absent.value.maxToolCallsPerRun === undefined);
    const set = parsePluginConfig({ ...BASE, maxToolCallsPerRun: 5 });
    assert.ok(set.ok && set.value.maxToolCallsPerRun === 5);
    for (const bad of [0, -1, 1.5, "5", null]) {
      assert.equal(parsePluginConfig({ ...BASE, maxToolCallsPerRun: bad }).ok, false, String(bad));
    }
  });
});
