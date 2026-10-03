import assert from "node:assert/strict";
import { describe, it } from "node:test";

import { allow, deny, needsApproval, unreachableUrl } from "./helpers/mock-pair.ts";
import { ev, isBlock, withGate } from "./helpers/with-gate.ts";

const PUSH = { command: "git push origin main" };
const CAT = { command: "cat /w/hello.txt" };

describe("pair gate: git (C1)", () => {
  it("git_push_is_sent_as_git_push_with_the_remote_as_destination", async () => {
    await withGate(() => needsApproval("h"), async (gate, mock) => {
      assert.ok(isBlock(await gate(ev("exec", { command: "git push https://github.com/a/b.git HEAD:main" }))));
      const body = mock.requests[0]?.body as Record<string, unknown>;
      assert.equal(body["tool"], "git.push");
      assert.equal(body["destination"], "https://github.com/a/b.git");
      assert.equal(body["executable"], "git");
    });
  });

  it("git_push_never_allowed_by_local_rules_alone", async () => {
    // Local rules do not flag `git push`; the verdict must come from the service and an `allow`
    // for a git.push (which a correct policy never returns) is still not honoured.
    await withGate(() => allow(), async (gate, mock) => {
      assert.ok(isBlock(await gate(ev("exec", PUSH))), "service allow for git.push is not honoured");
      assert.equal(mock.requests.length, 1, "the service was consulted");
    });
    await withGate(() => allow(), async (gate) => {
      assert.ok(isBlock(await gate(ev("exec", PUSH))), "unreachable service");
    }, { baseUrl: await unreachableUrl() });
  });

  it("other git subcommands and .git paths never reach the service", async () => {
    await withGate(() => allow(), async (gate, mock) => {
      for (const command of ["git fetch origin", "git -c core.fsmonitor=x status", "git config --list", "cat .git/config"]) {
        assert.ok(isBlock(await gate(ev("exec", { command }))), command);
      }
      assert.ok(isBlock(await gate(ev("read", { path: "/w/.git/config" }))));
      assert.equal(mock.requests.length, 0);
    });
  });
});

describe("pair gate: deny reasons are scrubbed of paths (M7)", () => {
  it("neither the block reason nor the log carries a raw path from the service", async () => {
    await withGate(() => deny("path /Users/devesh/.ssh/id_rsa is outside ~/work and ./x/../y denied"), async (gate, _m, logs) => {
      const r = await gate(ev("exec", CAT));
      assert.ok(r && "blockReason" in r);
      const text = JSON.stringify([r, logs]);
      assert.equal(text.includes("/Users/devesh"), false);
      assert.equal(text.includes("id_rsa"), false);
      assert.ok(text.includes("denied"));
    });
  });
});
