import assert from "node:assert/strict";
import { describe, it } from "node:test";

import { loadConnection } from "../src/config.ts";

const TOKEN = "t".repeat(32);

describe("config: PAIR_API_URL must be loopback unless explicitly allowed", () => {
  it("accepts loopback hosts", () => {
    for (const url of ["http://127.0.0.1:8080", "http://localhost:8080", "http://[::1]:8080", "http://127.0.0.2:1"]) {
      const r = loadConnection({ PAIR_API_URL: url, PAIR_SERVICE_TOKEN: TOKEN });
      assert.equal(r.ok, true, url);
    }
  });

  it("refuses remote hosts and lookalikes", () => {
    for (const url of [
      "http://10.0.0.5:8080",
      "https://pair.example.com",
      "http://127.0.0.1.evil.com:8080",
      "http://localhost.evil.com",
      "http://0.0.0.0:8080",
      "http://user:pw@127.0.0.1:8080",
      "ftp://127.0.0.1",
      "not a url",
      "",
    ]) {
      const r = loadConnection({ PAIR_API_URL: url, PAIR_SERVICE_TOKEN: TOKEN });
      assert.equal(r.ok, false, url);
    }
  });

  it("allows a remote host only with PAIR_ADAPTER_ALLOW_REMOTE=1", () => {
    const env = { PAIR_API_URL: "https://pair.example.com", PAIR_SERVICE_TOKEN: TOKEN };
    assert.equal(loadConnection(env).ok, false);
    assert.equal(loadConnection({ ...env, PAIR_ADAPTER_ALLOW_REMOTE: "1" }).ok, true);
    assert.equal(loadConnection({ ...env, PAIR_ADAPTER_ALLOW_REMOTE: "true" }).ok, false);
  });

  it("requires a service token of at least 16 characters", () => {
    assert.equal(loadConnection({ PAIR_API_URL: "http://127.0.0.1:1" }).ok, false);
    assert.equal(loadConnection({ PAIR_API_URL: "http://127.0.0.1:1", PAIR_SERVICE_TOKEN: "short" }).ok, false);
  });

  it("never echoes the token in a refusal", () => {
    const r = loadConnection({ PAIR_API_URL: "https://pair.example.com", PAIR_SERVICE_TOKEN: TOKEN });
    assert.equal(r.ok, false);
    if (!r.ok) assert.equal(r.reason.includes(TOKEN), false);
  });
});
