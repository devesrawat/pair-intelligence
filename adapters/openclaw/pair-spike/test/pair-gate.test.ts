import assert from "node:assert/strict";
import { describe, it } from "node:test";

import { createPairClient } from "../src/client.ts";
import { createToolGate, type GateResult, type HookLog } from "../src/gate.ts";
import { createPairToolGate, PAIR_APPROVAL_UNAVAILABLE_REASON, PAIR_UNAVAILABLE_REASON } from "../src/pair-gate.ts";
import {
  allow,
  deny,
  needsApproval,
  ok,
  startMockPair,
  unreachableUrl,
  type Script,
} from "./helpers/mock-pair.ts";

const TOKEN = "g".repeat(32);
const SHORT_TIMEOUT_MS = 200;
const APPROVAL_TIMEOUT_MS = 3_000;
const RUN = "run-1";

type LogLine = { readonly hook: string; readonly detail: Readonly<Record<string, unknown>> };

async function withGate<T>(
  script: Script,
  fn: (g: ReturnType<typeof createPairToolGate>, mock: Awaited<ReturnType<typeof startMockPair>>, logs: LogLine[]) => Promise<T>,
  opts: { log?: HookLog; baseUrl?: string } = {},
): Promise<T> {
  const mock = await startMockPair(script);
  const logs: LogLine[] = [];
  const log: HookLog = opts.log ?? ((hook, detail) => void logs.push({ hook, detail }));
  const client = createPairClient({ baseUrl: opts.baseUrl ?? mock.url, token: TOKEN }, { timeoutMs: SHORT_TIMEOUT_MS });
  const gate = createPairToolGate({
    client,
    log,
    localGate: createToolGate({ log, approvalTimeoutMs: APPROVAL_TIMEOUT_MS }),
    approvalTimeoutMs: APPROVAL_TIMEOUT_MS,
  });
  try {
    return await fn(gate, mock, logs);
  } finally {
    await mock.close();
  }
}

const ev = (toolName: string, params: Record<string, unknown> = {}) => ({ toolName, params, runId: RUN });
const isBlock = (r: GateResult): boolean => r !== undefined && "block" in r && r.block === true;
const CAT = { command: "cat /w/hello.txt" };

describe("pair gate: decisions", () => {
  it("allow from the service lets the call through and sends the mapped request", async () => {
    await withGate(() => allow(), async (gate, mock) => {
      assert.equal(await gate(ev("exec", CAT)), undefined);
      assert.equal(mock.requests.length, 1);
      const req = mock.requests[0];
      assert.ok(req);
      assert.equal(req.path, "/v1/policy/authorize");
      assert.equal(req.headers["authorization"], `Bearer ${TOKEN}`);
      assert.equal(req.headers["x-actor"], "openclaw-adapter");
      assert.match(String(req.headers["x-trace-id"]), /^[0-9a-f-]{36}$/);
      const body = req.body as Record<string, unknown>;
      assert.equal(body["tool"], "shell.exec");
      assert.equal(body["executable"], "cat");
      assert.deepEqual(body["args"], ["/w/hello.txt"]);
      assert.equal(body["data_class"], "personal");
      assert.match(String(body["task_id"]), /^[0-9a-f]{8}-[0-9a-f]{4}-5[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
    });
  });

  it("uses the same task id for every call of one run, and a different one for another run", async () => {
    await withGate(() => allow(), async (gate, mock) => {
      await gate(ev("exec", CAT));
      await gate(ev("read", { path: "/w/a" }));
      await gate({ toolName: "read", params: { path: "/w/a" }, runId: "run-2" });
      const ids = mock.requests.map((r) => (r.body as Record<string, unknown>)["task_id"]);
      assert.equal(ids[0], ids[1]);
      assert.notEqual(ids[0], ids[2]);
    });
  });

  it("deny from the service blocks with the reason", async () => {
    await withGate(() => deny("executable \"python3\" is not allowed"), async (gate) => {
      const r = await gate(ev("exec", CAT));
      assert.ok(r && "block" in r);
      assert.ok(r && "blockReason" in r && r.blockReason.includes("python3"));
    });
  });

  it("needs_approval_from_pair_blocks_until_the_approval_flow_is_wired (M4)", async () => {
    for (const [tool, params] of [
      ["web_fetch", { url: "https://github.com/x" }],
      ["exec", { command: "git push https://github.com/x/y.git main" }],
    ] as const) {
      await withGate(() => needsApproval("hash-abc"), async (gate, _mock, logs) => {
        const r = await gate(ev(tool, params));
        assert.ok(isBlock(r), tool);
        assert.equal(r !== undefined && "requireApproval" in r, false);
        assert.ok(r && "blockReason" in r && r.blockReason === PAIR_APPROVAL_UNAVAILABLE_REASON);
        assert.ok(logs.some((l) => l.detail["decision"] === "needs_approval"));
      });
    }
  });

  it("local approval is kept even when the service allows (the stricter verdict wins)", async () => {
    await withGate(() => allow(), async (gate) => {
      const r = await gate(ev("exec", { command: "curl http://127.0.0.1:1/x" }));
      assert.ok(r && "requireApproval" in r);
    });
  });
});

describe("pair gate: local deny wins and unmapped tools are denied", () => {
  it("a local deny blocks even though the service would allow, without needing the service", async () => {
    await withGate(() => allow(), async (gate, mock) => {
      for (const params of [{ command: "rm -rf /tmp/x" }, { command: "dd of=/dev/sda" }]) {
        assert.ok(isBlock(await gate(ev("exec", params))));
      }
      assert.ok(isBlock(await gate(ev("write", { path: "/w/a", content: "x" }))), "local allow-list denies write");
      assert.equal(mock.requests.length, 0);
    });
  });

  it("an unmapped tool is denied and never sent to the service", async () => {
    await withGate(() => allow(), async (gate, mock) => {
      for (const tool of ["sessions_list", "image", "process", "openclaw", "tool_call", "apply_patch", "mystery"]) {
        assert.ok(isBlock(await gate(ev(tool))), tool);
      }
      assert.equal(mock.requests.length, 0);
    });
  });

  it("a locally allowed but unmappable exec (shell syntax) is denied without a service call", async () => {
    await withGate(() => allow(), async (gate, mock) => {
      assert.ok(isBlock(await gate(ev("exec", { command: "cat a; id" }))));
      assert.ok(isBlock(await gate(ev("exec", { command: "cat a", workdir: "/" }))));
      assert.equal(mock.requests.length, 0);
    });
  });
});

describe("pair gate: every failure mode blocks", () => {
  const failureScripts: Array<[string, Script]> = [
    ["http 500", () => ({ kind: "json", status: 500, body: { success: false, data: null, error: { code: "internal", message: "x" } } })],
    ["http 401", () => ({ kind: "json", status: 401, body: { success: false, data: null, error: { code: "unauthenticated", message: "x" } } })],
    ["bad json", () => ({ kind: "raw", status: 200, text: "{not json" })],
    ["html body", () => ({ kind: "raw", status: 200, text: "<html></html>" })],
    ["unknown decision", () => ok({ decision: { decision: "maybe" }, policy_version: "pv" })],
    ["decision missing", () => ok({ policy_version: "pv" })],
    ["decision is a string", () => ok({ decision: "allow", policy_version: "pv" })],
    ["deny without reason", () => ok({ decision: { decision: "deny" }, policy_version: "pv" })],
    ["needs_approval without hash", () => ok({ decision: { decision: "needs_approval" }, policy_version: "pv" })],
    ["no policy_version", () => ok({ decision: { decision: "allow" } })],
    ["timeout", () => ({ kind: "hang" })],
    ["connection reset", () => ({ kind: "destroy" })],
  ];
  for (const [name, script] of failureScripts) {
    it(`blocks on ${name}`, async () => {
      await withGate(script, async (gate) => {
        const r = await gate(ev("exec", CAT));
        assert.ok(isBlock(r), name);
      });
    });
  }

  it("blocks when the service is unreachable, with a stable reason", async () => {
    await withGate(() => allow(), async (gate) => {
      const r = await gate(ev("exec", CAT));
      assert.deepEqual(r, { block: true, blockReason: PAIR_UNAVAILABLE_REASON });
    }, { baseUrl: await unreachableUrl() });
  });

  it("blocks and never throws when logging throws", async () => {
    const boom: HookLog = () => {
      throw new Error("disk on fire");
    };
    await withGate(() => allow(), async (gate) => {
      for (const e of [ev("exec", CAT), ev("read", { path: "/w/a" }), ev("exec", { command: "rm -rf x" }), ev("nope")]) {
        const r = await gate(e);
        assert.ok(isBlock(r), e.toolName);
      }
    }, { log: boom });
  });

  it("blocks and never throws when the event itself is hostile", async () => {
    await withGate(() => allow(), async (gate) => {
      const hostile = {
        toolName: "exec",
        runId: RUN,
        get params(): Record<string, unknown> {
          throw { toString() { throw new Error("nested"); } };
        },
      };
      assert.ok(isBlock(await gate(hostile)));
    });
  });

  it("blocks when the injected policy error probe is on", async () => {
    const mock = await startMockPair(() => allow());
    try {
      const log: HookLog = () => {};
      const gate = createPairToolGate({
        client: createPairClient({ baseUrl: mock.url, token: TOKEN }),
        log,
        localGate: createToolGate({ log, approvalTimeoutMs: APPROVAL_TIMEOUT_MS, injectPolicyError: true }),
        approvalTimeoutMs: APPROVAL_TIMEOUT_MS,
      });
      assert.ok(isBlock(await gate(ev("exec", CAT))));
      assert.equal(mock.requests.length, 0);
    } finally {
      await mock.close();
    }
  });
});

describe("pair gate: logging hygiene", () => {
  it("logs tool, decision, ids and hashes but never params, tokens or commands", async () => {
    await withGate(() => allow(), async (gate, _mock, logs) => {
      await gate(ev("exec", { command: "cat /w/SECRET-FILE-NAME" }));
      const text = JSON.stringify(logs);
      assert.equal(text.includes("SECRET-FILE-NAME"), false);
      assert.equal(text.includes(TOKEN), false);
      assert.match(text, /paramsSha256/);
      assert.match(text, /shell\.exec/);
    });
  });
});
