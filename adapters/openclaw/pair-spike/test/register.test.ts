import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, it } from "node:test";

import { PLUGIN_ID, registerPair, type PairApi } from "../src/register.ts";
import { allow, ok, startMockPair, type Script } from "./helpers/mock-pair.ts";

const TOKEN = "r".repeat(32);
const RESERVATION = "0199c0de-aaaa-7abc-8def-0123456789ab";

type Handler = (...args: unknown[]) => unknown;
type FakeApi = {
  readonly api: PairApi;
  readonly handlers: Map<string, Handler>;
  readonly errors: string[];
  readonly infos: string[];
};

function fakeApi(config: unknown, pluginConfig: Record<string, unknown> | undefined): FakeApi {
  const handlers = new Map<string, Handler>();
  const errors: string[] = [];
  const infos: string[] = [];
  const api = {
    config,
    pluginConfig,
    logger: { info: (m: string) => void infos.push(m), error: (m: string) => void errors.push(m) },
    on: (hook: string, handler: Handler) => void handlers.set(hook, handler),
  } as unknown as PairApi;
  return { api, handlers, errors, infos };
}

const hostConfig = (access: boolean | undefined) => ({
  plugins: { entries: { [PLUGIN_ID]: { enabled: true, ...(access === undefined ? {} : { hooks: { allowConversationAccess: access } }) } } },
});

function pluginConfig(dir: string): Record<string, unknown> {
  return {
    usageLogPath: join(dir, "usage.jsonl"),
    hookLogPath: join(dir, "hooks.jsonl"),
    overrideProvider: "mock",
    overrideModel: "mock-routed",
    taskKind: "default",
    priceVersion: "pv-test",
    inputPricePerMtokMicros: 1_000_000,
    outputPricePerMtokMicros: 5_000_000,
  };
}

const flow: Script = (path) => {
  if (path === "/v1/budget/reserve") return ok({ reservation_id: RESERVATION, task_id: "0199c0de-bbbb-7abc-8def-0123456789ab" });
  if (path === "/v1/budget/reconcile") return ok({ state: "unresolved", overrun: false });
  return allow();
};

async function withPlugin<T>(
  access: boolean | undefined,
  fn: (f: FakeApi, dir: string, requests: () => ReadonlyArray<{ path: string; body: unknown }>) => Promise<T>,
): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "pair-register-"));
  const mock = await startMockPair(flow);
  try {
    const f = fakeApi(hostConfig(access), pluginConfig(dir));
    registerPair(f.api, { PAIR_API_URL: mock.url, PAIR_SERVICE_TOKEN: TOKEN });
    return await fn(f, dir, () => mock.requests);
  } finally {
    await mock.close();
    rmSync(dir, { recursive: true, force: true });
  }
}

const handler = (f: FakeApi, hook: string): Handler => {
  const h = f.handlers.get(hook);
  assert.ok(h, `no handler for ${hook}`);
  return h;
};

describe("registration with conversation access (H4)", () => {
  it("registers the tool gate and every model hook", async () => {
    await withPlugin(true, async (f) => {
      for (const hook of ["before_tool_call", "before_model_resolve", "before_agent_run", "llm_output", "model_call_ended", "after_tool_call"]) {
        assert.ok(f.handlers.has(hook), hook);
      }
      assert.deepEqual(f.errors, []);
    });
  });

  for (const [name, access] of [["off", false], ["unset", undefined]] as const) {
    it(`access_${name}_installs_deny_all_tool_gate_and_logs_an_error`, async () => {
      await withPlugin(access, async (f, _dir, requests) => {
        assert.equal(f.errors.length, 1);
        assert.match(f.errors[0] ?? "", /allowConversationAccess/);
        assert.ok(!f.handlers.has("before_model_resolve"), "no model hooks are registered (OpenClaw would drop them)");
        const verdict = await handler(f, "before_tool_call")({ toolName: "read", params: { path: "/w/a" }, runId: "r1" });
        assert.deepEqual(verdict, { block: true, blockReason: "pair: adapter is misconfigured, tool calls are denied" });
        assert.equal(requests().length, 0);
      });
    });
  }

  it("a host config without plugins.entries counts as access off", async () => {
    for (const config of [undefined, null, {}, { plugins: {} }, { plugins: { entries: { [PLUGIN_ID]: { hooks: { allowConversationAccess: "true" } } } } }]) {
      const f = fakeApi(config, pluginConfig("/unused"));
      registerPair(f.api, { PAIR_API_URL: "http://127.0.0.1:1", PAIR_SERVICE_TOKEN: TOKEN });
      assert.equal(f.errors.length, 1, JSON.stringify(config));
      assert.ok(f.handlers.has("before_tool_call"));
      assert.ok(!f.handlers.has("llm_output"));
    }
  });

  it("an invalid plugin config installs deny-all handlers for tools and runs", () => {
    const f = fakeApi(hostConfig(true), undefined);
    registerPair(f.api, { PAIR_API_URL: "http://127.0.0.1:1", PAIR_SERVICE_TOKEN: TOKEN });
    assert.equal(f.errors.length, 1);
    assert.ok(f.handlers.has("before_tool_call"));
    assert.ok(f.handlers.has("before_agent_run"));
  });
});

describe("multi-attempt runs through the registered hooks (H1)", () => {
  const usage = { input: 11, output: 7, cacheRead: 0, cacheWrite: 0 };
  const llmOutput = (model: string) => ({ runId: "run-9", provider: "mock", model, usage });

  it("blocks the second attempt and reconciles a failover model with unknown cost", async () => {
    await withPlugin(true, async (f, _dir, requests) => {
      const ctx = { runId: "run-9" };
      await handler(f, "before_model_resolve")({ prompt: "hi" }, ctx);
      assert.deepEqual(await handler(f, "before_agent_run")({}, ctx), { outcome: "pass" });
      await handler(f, "llm_output")(llmOutput("mock-fallback"));
      const reconcile = requests().find((r) => r.path === "/v1/budget/reconcile");
      assert.equal((reconcile?.body as Record<string, unknown>)["actual_cost_micros"], null);
      const second = (await handler(f, "before_agent_run")({}, ctx)) as { outcome: string };
      assert.equal(second.outcome, "block");
    });
  });

  it("the routed model is reconciled with its computed cost", async () => {
    await withPlugin(true, async (f, _dir, requests) => {
      await handler(f, "before_model_resolve")({ prompt: "hi" }, { runId: "run-9" });
      await handler(f, "llm_output")(llmOutput("mock-routed"));
      const reconcile = requests().find((r) => r.path === "/v1/budget/reconcile");
      assert.equal((reconcile?.body as Record<string, unknown>)["actual_cost_micros"], 46);
    });
  });
});

describe("params changed after PAIR approved a call (M1)", () => {
  const call = { toolName: "exec", params: { command: "cat /w/hello.txt" }, runId: "r1", toolCallId: "tc-1" };

  it("other_plugin_params_rewrite_is_detected_and_logged", async () => {
    await withPlugin(true, async (f, dir) => {
      assert.equal(await handler(f, "before_tool_call")(call), undefined);
      await handler(f, "after_tool_call")({ ...call, params: { command: "cat /w/other.txt" } });
      assert.equal(f.errors.length, 1);
      assert.match(f.errors[0] ?? "", /params differ/);
      const log = readFileSync(join(dir, "hooks.jsonl"), "utf8");
      assert.match(log, /params-changed-after-approval/);
      assert.equal(log.includes("other.txt"), false, "params are never logged");
    });
  });

  it("unchanged params and unknown calls produce no error", async () => {
    await withPlugin(true, async (f) => {
      await handler(f, "before_tool_call")(call);
      await handler(f, "after_tool_call")(call);
      await handler(f, "after_tool_call")({ ...call, toolCallId: "never-seen" });
      assert.deepEqual(f.errors, []);
    });
  });
});
