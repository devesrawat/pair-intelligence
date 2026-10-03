import assert from "node:assert/strict";
import { describe, it } from "node:test";

import { loadConnection } from "../src/config.ts";

const TOKEN = "t".repeat(32);
const env = (url: string, extra: Record<string, string> = {}) => ({ PAIR_API_URL: url, PAIR_SERVICE_TOKEN: TOKEN, ...extra });

describe("connection hardening (H3, LOW)", () => {
  it("refuses_non_loopback_http_even_with_allow_remote", () => {
    const r = loadConnection(env("http://pair.example.com:8080", { PAIR_ADAPTER_ALLOW_REMOTE: "1" }));
    assert.equal(r.ok, false);
    if (!r.ok) {
      assert.match(r.reason, /https/);
      assert.equal(r.reason.includes(TOKEN), false);
    }
  });

  it("accepts a remote https URL only with the explicit opt-in", () => {
    assert.equal(loadConnection(env("https://pair.example.com")).ok, false);
    const r = loadConnection(env("https://pair.example.com", { PAIR_ADAPTER_ALLOW_REMOTE: "1" }));
    assert.deepEqual(r, { ok: true, value: { baseUrl: "https://pair.example.com", token: TOKEN } });
  });

  it("still accepts loopback http without any flag", () => {
    assert.equal(loadConnection(env("http://127.0.0.1:8080")).ok, true);
    assert.equal(loadConnection(env("http://localhost:8080")).ok, true);
  });

  it("rejects a URL with a path, query or fragment instead of silently dropping it", () => {
    for (const url of ["http://127.0.0.1:8080/pair", "http://127.0.0.1:8080/v1/", "http://127.0.0.1:8080/?a=1", "http://127.0.0.1:8080/#x"]) {
      assert.equal(loadConnection(env(url)).ok, false, url);
    }
    assert.equal(loadConnection(env("http://127.0.0.1:8080/")).ok, true);
  });
});
