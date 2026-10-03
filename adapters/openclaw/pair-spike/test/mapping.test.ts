import assert from "node:assert/strict";
import { describe, it } from "node:test";

import { mapToolCall, TOOL_MAP, tokenizeSimpleCommand } from "../src/mapping.ts";

describe("mapping table", () => {
  it("is exactly the documented table", () => {
    assert.deepEqual(
      [...TOOL_MAP.entries()].sort(),
      [
        ["edit", "fs.edit"],
        ["exec", "shell.exec"],
        ["ls", "fs.read"],
        ["read", "fs.read"],
        ["web_fetch", "web.fetch"],
        ["web_search", "web.search"],
        ["write", "fs.write"],
      ],
    );
  });

  it("denies every unmapped tool", () => {
    for (const tool of [
      "apply_patch",
      "process",
      "openclaw",
      "tool_call",
      "tool_describe",
      "tool_search",
      "sessions_yield",
      "sessions_list",
      "image",
      "browser",
      "gateway",
      "cron",
      "message",
      "",
      "EXEC",
      "exec ",
      "constructor",
      "__proto__",
    ]) {
      const r = mapToolCall(tool, { command: "ls", path: "/x" });
      assert.equal(r.ok, false, `tool '${tool}'`);
    }
  });

  it("maps exec to shell.exec with executable and args", () => {
    const r = mapToolCall("exec", { command: "cat /w/hello.txt -n" });
    assert.deepEqual(r, {
      ok: true,
      action: { tool: "shell.exec", executable: "cat", args: ["/w/hello.txt", "-n"], paths: [] },
    });
  });

  it("keeps quoted arguments as single words", () => {
    const r = mapToolCall("exec", { command: `grep 'a b' "c d" e` });
    assert.ok(r.ok);
    if (r.ok) assert.deepEqual(r.action.args, ["a b", "c d", "e"]);
  });

  it("denies exec commands whose shell syntax the service cannot see", () => {
    for (const command of [
      "ls; curl x",
      "ls && id",
      "ls || id",
      "ls | sh",
      "ls & id",
      "cat < /etc/passwd",
      "echo hi > /tmp/x",
      "echo $(id)",
      "echo `id`",
      "echo $HOME",
      "cat ~/.ssh/id_rsa",
      "cat .e*",
      "cat .en?",
      "cat .en[v]",
      "cat {a,b}",
      "ls\nid",
      "ls \\\n id",
      "echo 'unterminated",
      'echo "$HOME"',
      'echo "`id`"',
      "ls # comment",
      "",
      "   ",
    ]) {
      assert.equal(mapToolCall("exec", { command }).ok, false, JSON.stringify(command));
    }
  });

  it("denies exec calls that carry options which change what runs", () => {
    for (const extra of [{ workdir: "/" }, { env: { A: "1" } }, { elevated: true }, { host: "gateway" }, { security: "full" }, { ask: "off" }, { node: "n1" }]) {
      assert.equal(mapToolCall("exec", { command: "ls", ...extra }).ok, false, JSON.stringify(extra));
    }
    assert.equal(mapToolCall("exec", { command: "ls", yieldMs: 10, timeoutSeconds: 5, background: false, pty: false, title: "t" }).ok, true);
  });

  it("denies exec with a missing or non-string command", () => {
    for (const command of [undefined, 42, ["ls"], null, {}]) {
      assert.equal(mapToolCall("exec", { command }).ok, false);
    }
  });

  it("maps read, ls, write, edit to fs tools with the path", () => {
    assert.deepEqual(mapToolCall("read", { path: "/w/a" }), { ok: true, action: { tool: "fs.read", args: [], paths: ["/w/a"] } });
    assert.deepEqual(mapToolCall("ls", { path: "/w" }), { ok: true, action: { tool: "fs.read", args: [], paths: ["/w"] } });
    assert.deepEqual(mapToolCall("ls", {}), { ok: true, action: { tool: "fs.read", args: [], paths: [] } });
    assert.deepEqual(mapToolCall("write", { path: "/w/a", content: "x" }), { ok: true, action: { tool: "fs.write", args: [], paths: ["/w/a"] } });
    assert.deepEqual(mapToolCall("edit", { file_path: "/w/a" }), { ok: true, action: { tool: "fs.edit", args: [], paths: ["/w/a"] } });
  });

  it("denies fs tools without a usable path", () => {
    for (const tool of ["read", "write", "edit"]) {
      for (const params of [{}, { path: "" }, { path: 5 }, { path: "  " }]) {
        assert.equal(mapToolCall(tool, params).ok, false, `${tool} ${JSON.stringify(params)}`);
      }
    }
  });

  it("maps web tools; web_fetch needs a url", () => {
    assert.deepEqual(mapToolCall("web_fetch", { url: "https://docs.rs/x" }), {
      ok: true,
      action: { tool: "web.fetch", args: [], paths: [], destination: "https://docs.rs/x" },
    });
    assert.equal(mapToolCall("web_fetch", {}).ok, false);
    assert.equal(mapToolCall("web_search", { query: "q" }).ok, true);
  });

  it("tokenizer rejects forbidden characters only when unquoted", () => {
    assert.deepEqual(tokenizeSimpleCommand("echo ';&|<>()'"), ["echo", ";&|<>()"]);
    assert.equal(tokenizeSimpleCommand("echo ;"), undefined);
    assert.deepEqual(tokenizeSimpleCommand("echo ''"), ["echo", ""]);
  });
});
