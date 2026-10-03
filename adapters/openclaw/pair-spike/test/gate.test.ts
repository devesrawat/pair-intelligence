// Run with: node --test adapters/openclaw/pair-spike/test/   (Node >= 22.18 strips types natively)
import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, it } from "node:test";

import { createToolGate, FAIL_CLOSED_REASON, type HookLog } from "../src/gate.ts";
import { appendJsonl } from "../src/jsonl.ts";
import { decideToolCall } from "../src/rules.ts";

const APPROVAL_TIMEOUT_MS = 3_000;
const noopLog: HookLog = () => {};

function execCall(command: unknown) {
  return decideToolCall("exec", { command });
}

describe("rules: tool-name allow-list with default deny", () => {
  it("allows only listed read-only tools", () => {
    for (const tool of ["read", "sessions_list", "sessions_history"]) {
      assert.equal(decideToolCall(tool, {}).kind, "allow", tool);
    }
  });

  it("denies every unlisted tool by default", () => {
    for (const tool of [
      "write",
      "edit",
      "apply_patch",
      "gateway",
      "cron",
      "message",
      "nodes",
      "process",
      "browser",
      "sessions_send",
      "sessions_spawn",
      "",
      "some_future_tool",
    ]) {
      assert.equal(decideToolCall(tool, {}).kind, "deny", `tool '${tool}' must be denied`);
    }
  });

  it("requires approval for network tools", () => {
    assert.equal(decideToolCall("web_search", {}).kind, "approve");
    assert.equal(decideToolCall("web_fetch", {}).kind, "approve");
  });
});

describe("rules: exec command inspection", () => {
  const destructive = [
    "rm -rf /tmp/x",
    "rm -fr /tmp/x",
    "rm -r -f /tmp/x",
    "rm -Rf x",
    "rm --recursive --force x",
    "rm -rf",
    "find / -delete",
    "find . -name '*.log' -delete",
    "dd if=/dev/zero of=/dev/sda",
    "dd of=disk.img if=/dev/urandom",
    "mkfs /dev/sda1",
    "mkfs.ext4 /dev/sda1",
    "sudo -u root rm -rf /",
    "ls && rm -rf x",
    "echo ok; rm -fr x",
    "echo $(rm -rf x)",
    "bash -c \"rm -fr x\"",
    "ls | xargs rm -rf",
    "/bin/rm -rf x",
    "\\rm -rf x",
    "rm /*",
    "rm --no-preserve-root /",
    "echo hi > /dev/sda",
    "shred -n 3 file",
    "wipefs -a /dev/sdb",
  ];
  for (const command of destructive) {
    it(`denies: ${command}`, () => {
      assert.equal(execCall(command).kind, "deny");
    });
  }

  it("denies exec with a missing, empty or non-string command", () => {
    assert.equal(decideToolCall("exec", {}).kind, "deny");
    assert.equal(execCall("   ").kind, "deny");
    assert.equal(execCall(42).kind, "deny");
    assert.equal(execCall(["rm", "-rf", "x"]).kind, "deny");
  });

  it("allows ordinary commands", () => {
    for (const command of [
      "ls -la",
      "echo hello",
      "rm file.txt",
      "grep -r foo .",
      "find . -name '*.ts'",
      "dd if=disk.img bs=1m count=1",
      "git status",
    ]) {
      assert.equal(execCall(command).kind, "allow", command);
    }
  });

  it("requires approval for network commands", () => {
    assert.equal(execCall("curl http://127.0.0.1:1/never").kind, "approve");
    assert.equal(execCall("ls && wget x").kind, "approve");
  });
});

describe("gate: fail closed", () => {
  const gate = (log: HookLog, injectPolicyError?: boolean) =>
    createToolGate({
      log,
      approvalTimeoutMs: APPROVAL_TIMEOUT_MS,
      ...(injectPolicyError === undefined ? {} : { injectPolicyError }),
    });
  const event = (toolName: string, params: Record<string, unknown> = {}) => ({ toolName, params });

  it("passes allowed calls, blocks denied ones, and asks approval for network", () => {
    const g = gate(noopLog);
    assert.equal(g(event("read")), undefined);
    const denied = g(event("exec", { command: "rm -rf x" }));
    assert.ok(denied && "block" in denied && denied.block === true);
    const approval = g(event("exec", { command: "curl x" }));
    assert.ok(approval && "requireApproval" in approval);
  });

  it("blocks on injected policy error", () => {
    const result = gate(noopLog, true)(event("read"));
    assert.deepEqual(result, { block: true, blockReason: FAIL_CLOSED_REASON });
  });

  it("blocks when the log path is unwritable, for allowed and denied calls alike", () => {
    const dir = mkdtempSync(join(tmpdir(), "pair-gate-"));
    const regularFile = join(dir, "a-file");
    writeFileSync(regularFile, "x");
    // A directory cannot be created below a regular file: appendJsonl throws ENOTDIR.
    const unwritable = join(regularFile, "sub", "hooks.jsonl");
    assert.throws(() => appendJsonl(unwritable, { probe: true }));
    const g = gate((hook, detail) => appendJsonl(unwritable, { hook, ...detail }));
    for (const e of [event("read"), event("exec", { command: "rm -rf x" }), event("web_fetch")]) {
      const result = g(e);
      assert.ok(result && "block" in result && result.block === true, `${e.toolName} must be blocked`);
    }
  });

  it("never throws and always blocks when logging throws on every call", () => {
    const alwaysThrows: HookLog = () => {
      throw new Error("disk on fire");
    };
    for (const injected of [false, true]) {
      const result = gate(alwaysThrows, injected)(event("read"));
      assert.deepEqual(result, { block: true, blockReason: FAIL_CLOSED_REASON });
    }
  });

  it("never throws when the error itself cannot be stringified", () => {
    const hostile = {
      toolName: "read",
      get params(): Record<string, unknown> {
        throw {
          toString() {
            throw new Error("toString exploded");
          },
        };
      },
    };
    const result = gate(noopLog)(hostile);
    assert.deepEqual(result, { block: true, blockReason: FAIL_CLOSED_REASON });
  });
});
