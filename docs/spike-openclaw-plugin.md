# OpenClaw plugin spike (Task 1)

Run: 2026-10-03 against the built gateway at `v2026.9.7` (`c074824a27c9`), Node 24.21.0, macOS arm64. The OpenClaw checkout was used read-only (execute and read; no install, rebuild or write).

**Result: pass.** A hook-only out-of-tree plugin loaded into the real gateway, blocked a tool call, requested approval for another, overrode the model, and wrote usage records, with no real model, no API key and no non-loopback traffic observed. Every claim below is backed by a committed file under `adapters/openclaw/spike/evidence/`.

## Artifacts

| Path | Purpose |
|---|---|
| `adapters/openclaw/pair-spike/openclaw.plugin.json` | Manifest, `configSchema`, `activation.onStartup` |
| `adapters/openclaw/pair-spike/package.json` | `openclaw.extensions`, `openclaw.runtimeExtensions`, `openclaw.compat.pluginApi >=2026.9.7` |
| `adapters/openclaw/pair-spike/src/index.ts`, `src/register.ts`, `src/rules.ts` | `definePluginEntry` entry; hook registration (SDK-free, testable with a fake api); pure policy in `rules.ts` |
| `adapters/openclaw/spike/mock-openai-server.ts` | Loopback OpenAI-compatible mock (SSE streaming, scripted tool calls) |
| `adapters/openclaw/spike/openclaw.spike.template.json5` | Sanitized gateway config template |
| `adapters/openclaw/spike/{env.sh,spike.sh,setup-plugin.sh}` | Throwaway-state env, driver, plugin build |
| `adapters/openclaw/spike/evidence/{final,noconv,errprobe}/` | Captured JSONL from the three runs |

Strict TypeScript (`strict`, `noUncheckedIndexedAccess`), ESM, no `any`. Built with the checkout's own `tsc`; types resolve through a `node_modules/openclaw` symlink to the checkout (created by `setup-plugin.sh`, git-ignored). At runtime the host resolved `openclaw/plugin-sdk/plugin-entry` **without** that symlink (verified by removing it; `plugins inspect` still `status: loaded, hookCount: 4`), so the plugin needs no `node_modules`.

## Isolation

`env.sh` sets, all pointing into a scratch directory:

```bash
OPENCLAW_HOME=$SPIKE_DIR/home
OPENCLAW_STATE_DIR=$SPIKE_DIR/state          # src/config/paths.ts:69,169
OPENCLAW_CONFIG_PATH=$SPIKE_DIR/state/openclaw.json   # paths.ts:226
OPENCLAW_NO_AUTO_UPDATE=1
CLAWHUB_DISABLE_TELEMETRY=1
OPENCLAW_DISABLE_BONJOUR=1
```

Config (template): `gateway {mode: local, bind: loopback, port: 18911, auth: {mode: token}}`, `plugins.deny: [ollama, lmstudio, vllm, sglang, llama-cpp]`, `plugins.load.paths: [<plugin dir>]`, `plugins.entries.pair-spike.hooks.allowConversationAccess: true`, `models.providers.mock {baseUrl: http://127.0.0.1:18901/v1, api: openai-completions}` with models `mock-small` (agent default) and `mock-routed` (hook override), `agents.defaults.model.primary: mock/mock-small`. `openclaw telemetry off` was run against the throwaway config (`Anonymous feature stats disabled.`). `~/.openclaw` mtime remained 7 Jun (untouched).

Network check: `lsof -a -p <gateway pid> -i` after three turns showed only the two loopback LISTEN sockets (`127.0.0.1:18911`, `[::1]:18911`) and no outbound sockets. This is a point-in-time snapshot, not a packet capture.

Side effects to know about, none inside the state dir: the gateway also logs to `/tmp/openclaw/openclaw-<date>.log` (`[gateway] log file:` line). Bonjour/mDNS advertising is on by default (first run logged `bonjour: advertised gateway ... state=announcing`, a LAN multicast); `OPENCLAW_DISABLE_BONJOUR=1` removed that, so keep it in PAIR deployment env.

## Commands (exact)

```bash
export PATH=$HOME/.nvm/versions/node/v24.21.0/bin:$PATH     # nvm use 24
export SPIKE_DIR=<scratch>  OPENCLAW_DIR=<checkout>
adapters/openclaw/spike/setup-plugin.sh                     # tsc -> pair-spike/dist
adapters/openclaw/spike/spike.sh init true false            # render config: allowConversationAccess=true, injectPolicyError=false
adapters/openclaw/spike/spike.sh oc plugins inspect pair-spike --runtime --json
adapters/openclaw/spike/spike.sh mock-start                 # node mock-openai-server.ts (Node 24 type stripping)
adapters/openclaw/spike/spike.sh gw-start                   # openclaw gateway run --port 18911 --bind loopback
# non-interactive turn through the running Gateway (docs/cli/agent.md):
spike.sh oc agent --agent main --session-key agent:main:f-plain   --message "hello SCENARIO:plain"   --json
spike.sh oc agent --agent main --session-key agent:main:f-deny    --message "do it SCENARIO:deny"    --json
spike.sh oc agent --agent main --session-key agent:main:f-approve --message "do it SCENARIO:approve" --json
```

`spike.sh oc ...` is `node $OPENCLAW_DIR/openclaw.mjs ...` under the env above. Use a fresh `--session-key` per scenario; the mock keys off conversation history. Gateway cold start is about 45 s (19 bundled plugins); the first turn took about 17 s, later turns 1-2 s.

`openclaw plugins inspect` result: `shape: hook-only`, `typedHooks: [before_model_resolve, before_tool_call (priority 100), llm_output, model_call_ended]`, `origin: config`, `policy.allowConversationAccess: true`. It also warns "OpenClaw can't verify where this plugin came from" (no trust record for config-path plugins). It does not stop loading.

## Evidence

### 1. Model override (`evidence/final/`)

`hooks.jsonl` line 1: `before_model_resolve` returned `{providerOverride: "mock", modelOverride: "mock-routed"}`. `mock-requests.jsonl`: every request carries `"model":"mock-routed"`; `mock-small` (the configured default) never appears. The CLI envelope also reports `winnerModel: mock-routed` and `modelRouteChange: "Model route changed: mock/mock-small -> mock/mock-routed."`. The mock saw `Authorization: Bearer <redacted>` from the dummy key only.

### 2. Tool gate

Block (`SCENARIO:deny`): mock replied with an `exec` tool call `rm -rf /tmp/pair-spike-denied`; hook logged `decision: deny`; the tool result the model received, and the final assistant text, was `pair-spike: destructive command denied by policy` (the `blockReason` verbatim). `/tmp/pair-spike-denied` was never created (it did not exist before either, so this is a no-side-effect check rather than a proof the command would have run).

Approval (`SCENARIO:approve`): hook logged `decision: approve` for `curl http://127.0.0.1:1/never` and returned `requireApproval {severity: warning, timeoutMs: 3000}`. The tool did not run. The tool result was:

```json
{"status":"error","tool":"exec","error":"Plugin approval unavailable: non-interactive CLI runs have no approval-capable initiating surface."}
```

So in a CLI-driven turn `requireApproval` **fails closed to deny** immediately (source: `src/agents/agent-tools.before-tool-call.approval.ts:172-193`: `trigger==="user"` with no `turnSourceChannel` and no `approvalReviewerDeviceId`). **Not demonstrated:** a real allow-once/deny round trip. That needs an approval-capable surface (a channel with native approvals, or a paired operator device). Task 3 must test it with the real surface PAIR will use.

Fail-closed on plugin error (`evidence/errprobe/`): with `injectPolicyError: true` the handler throws inside the plugin's own `try`, logs `decision: deny-on-error`, and returns `{block: true}`; the model received `pair-spike: policy error, failing closed`. **Not demonstrated:** the host's own behavior when a handler throws or exceeds the documented 15 s timeout (the plugin catches before the host sees it).

### 3. Usage JSONL (`evidence/final/usage.jsonl`)

One `model_call_ended` record per provider call and one `llm_output` record per run. Example `llm_output`: `{"provider":"mock","model":"mock-routed","resolvedRef":"mock/mock-routed","usage":{"input":11,"output":7,"cacheRead":0,"cacheWrite":0,"total":18,"cost":{"total":0}}}`. Mock emits fixed 11 in / 7 out per call.

### 4. allowConversationAccess gotcha (`evidence/noconv/`)

Same plugin, `allowConversationAccess: false`:

- Load-time warnings (`noconv/load-warnings.txt`): `typed hook "before_model_resolve" blocked because non-bundled plugins must set plugins.entries.pair-spike.hooks.allowConversationAccess=true` and the same for `"llm_output"`. The audit called this "silently dropped"; it is dropped but **logged at warn level at gateway start**, so it is detectable.
- `before_model_resolve` and `llm_output` never fired. The mock saw `"model":"mock-small"` (override not applied) and no `llm_output` records exist.
- `before_tool_call` **still fired and still blocked** (`decision: deny`, same blockReason). `model_call_ended` **still fired**.

Consequence: tool gating and call telemetry do not need the opt-in; routing and token usage do.

## Observed hook contracts

| Hook | Event fields observed in use | Result accepted | Opt-in needed | Notes |
|---|---|---|---|---|
| `before_model_resolve` | `{prompt, attachments?}` only (no messages, no current model) | `{providerOverride, modelOverride}` | Yes | Fired once per run, before any model call. Both fields must name a configured provider/model. |
| `before_tool_call` | `{toolName, params, toolCallId?, runId?, ...}`; tool is `exec`, params `{command}` | `{block, blockReason}` or `{requireApproval{title,description,severity,timeoutMs}}` | No | Fires per tool call. `blockReason` is returned to the model as the tool result. After a block the model may retry (my mock looped until it was made to stop), so budget/loop limits are PAIR's job. |
| `model_call_ended` | `{runId, callId, provider, model, outcome, durationMs, ...}`; **no token counts** | none (observer) | No | `callId` is `<runId>:model:<n>`; `model` is the post-override model. |
| `llm_output` | `{runId, provider, model, resolvedRef, usage{input,output,cacheRead,cacheWrite,total,cost{total}}, assistantTexts, prompt}` | none (observer) | Yes | `usage` is **summed over all model calls of one attempt** (22/14 for an attempt with a tool call and a follow-up). One record per **attempt**, not per run and not per call: a run that retries (compaction, empty-response retry, auth rotation) emits one per attempt (`src/agents/embedded-agent-runner/run/attempt-result.ts:277`, `usage = attemptUsage`). A failover attempt reports the fallback model's `provider`/`model`. |

Implication for the ADR: a per-call budget ledger cannot get token counts from `model_call_ended`; it must use `llm_output` (per attempt, post hoc, opt-in required) or `onDiagnosticEvent` `model.usage` (not exercised here). Pre-call reservation belongs in `before_model_resolve`, which only sees the prompt text, so estimate from `prompt.length` there.

The tool list sent to the mock (`mock-requests.jsonl`, `tools`) is the default agent surface: `apply_patch, edit, exec, ls, openclaw, process, read, sessions_yield, tool_call, tool_describe, tool_search, write`. Sandbox mode was **off** in this spike, so `exec` would have run on the host if not blocked.

## Divergences from `docs/upstream-audit.md`

1. Conversation-hook gotcha: not silent. A warn-level log line names each blocked hook at start (see above). The audit's table row "Tool gate: no conversation opt-in needed" is confirmed. Add: `model_call_ended` also needs none.
2. `before_model_resolve` is documented as `{providerOverride?, modelOverride?}`; confirmed, with the extra finding that the CLI reports the reroute (`modelRouteChange`).
3. Telemetry row: `model_call_ended` has no usage; `llm_output.usage` is a per-attempt aggregate. The audit listed them side by side as if equivalent.
4. `requireApproval` is not usable from `openclaw agent` CLI turns (immediate deny). The audit did not mention this surface dependency.
5. Operational: `openclaw telemetry off` rewrites the config file and strips JSON5 comments (creates `.bak` files); mDNS advertising is on by default; logs go to `/tmp/openclaw/`. None of this is in the audit's telemetry row.
6. License facts changed, see `docs/license-scan.md`.

## What did not get tested

- Real allow-once / allow-always / deny resolution of `requireApproval`.
- Host handling of a throwing or timed-out handler.
- `before_prompt_build` and the memory capability (ADR lists them; Tasks 7-9).
- `onDiagnosticEvent` `model.usage` cost fields.
- Sandbox on (`agents.defaults.sandbox.mode: all`); the ADR requires it for deployment. The spike ran with the default `off`.
- Real provider streaming quirks (mock only), and Anthropic auth (open item from the audit, needs the owner's capped key).
- Plugin install via `openclaw plugins install` (used `plugins.load.paths` instead).

## Post-review hardening (review finding M5)

The evidence above was captured with the first version of the gate. After review the plugin changed, and the evidence directories were **not** re-captured against a live gateway:

- The `before_tool_call` handler lives in `src/gate.ts`. Its error path never throws (logging is wrapped in its own `try`) and always returns `{block: true}`, including when the hook log path is unwritable.
- `src/rules.ts` is an allow-list of tool names with default deny (`read`, `sessions_list`, `sessions_history`, `sessions_search`, `image`); `web_search` / `web_fetch` need approval; `exec` is allowed only after command inspection (recursive `rm` in any flag order, `find -delete`, `dd of=`, `mkfs*`, `wipefs`, `shred`, block-device redirects are denied; `curl`/`wget`/`ssh` etc. need approval). The allow-list is a spike-level placeholder, not the spec section 9 policy.
- Tests: `scripts/tests/test_openclaw_gate.sh` (`node --test`, Node 22.18+). They exercise the pure policy and the gate, not the real OpenClaw host.
- L12 (hook log recorded full tool params) is closed: logs now carry a params hash only. The evidence under `evidence/{final,noconv,errprobe}` predates both the hardening and the pair-api wiring; `evidence/wired/` is the current, live-captured set (see "Wired to pair-api" below).

## Wired to pair-api

The adapter now calls pair-api. Source: `adapters/openclaw/pair-spike/src/` (`pair-gate.ts`, `mapping.ts`, `budget.ts`, `client.ts`, `config.ts`). Tests were written alongside each module (not strictly red-first for every case); the live harness was written last and its first runs exposed two harness bugs (below), not adapter bugs.

### How OpenClaw can and cannot block a model call (read from the pinned source)

| Mechanism | Can it stop the model call? | Evidence |
|---|---|---|
| `before_model_resolve` | **No.** It returns `{providerOverride?, modelOverride?}` only. A throw is caught, logged at warn, and the run continues on the default model (fail OPEN). It is skipped entirely when `modelSelectionLocked === true`. | `src/agents/embedded-agent-runner/run/setup.ts:106-141`, `src/plugins/hook-types.ts:971-980` |
| `before_agent_run` | **Yes, the supported gate.** Returns `{outcome:"block", reason, message?, category?}`; runs after prompt build and before model submission; fail CLOSED on throw or the 15 s timeout. Implemented by the embedded and CLI runners only (not Codex/Copilot harnesses). It is a conversation hook: needs `allowConversationAccess`. | `docs/plugins/hooks/reference.md:93,130`, `docs/plugins/hooks/prompt-and-session.md:149-156`, `src/plugins/hook-runner-global.ts:26`, `src/agents/embedded-agent-runner/run/attempt-before-agent-run.ts`, `attempt-prompt-phase.ts:216-232` |
| `before_agent_reply` `{handled:true}` | Replies without a model call; not used. | `docs/plugins/hooks.md:161` |

Design: `before_model_resolve` reserves via `POST /v1/budget/reserve` and records the outcome per `ctx.runId`; the override is returned only on success. `before_agent_run` blocks the run unless that run holds a live reservation. A missing record (hook never fired, locked model selection, no run id, any error) means block, and so does a reservation that is **already reconciled**. `llm_output` reconciles. Cadence matters: `before_model_resolve` fires once per RUN, `before_agent_run` and `llm_output` once per ATTEMPT (`run-loop.ts:254`, `attempt-result.ts:277`). One reservation therefore covers exactly one attempt; the second attempt of a run (compaction, empty-response retry, auth rotation) finds the reservation reconciled and is blocked rather than run with no hold (tests `second_attempt_without_reservation_is_blocked`, `registered hooks: blocks the second attempt ...`). A retry that OpenClaw would have made is thus a blocked run: fail closed, at the cost of availability. `ctx.runId` is populated in all three hooks and matches `llm_output.event.runId` (observed live, `evidence/wired/p1-normal-turn/hooks.jsonl`).

Honest limits of the model-call gate:

- It exists only while `allowConversationAccess=true` (all three hooks are conversation hooks; for a non-bundled plugin OpenClaw requires exactly `true`, `src/plugins/hook-policy-decisions.ts`). With it off OpenClaw drops the hooks and **model calls cannot be gated by this adapter**. The adapter now detects that at registration (`conversationAccessGranted(api.config, "pair-spike")` reads `plugins.entries.pair-spike.hooks.allowConversationAccess`), logs an ERROR, and installs a deny-all tool gate instead of half-gating (tests `access_off_...`, `access_unset_...`; live phase 8). Model calls themselves still run ungated in that state; the ERROR line and the dead tool gate are the signal.
- **The budget is a per-run ESTIMATE, not a bound.** The cap is enforced on holds, not on realised spend. The hold is `4 x (prompt/4 + 16,000 overhead tokens x input price + 2,048 x output price)` (`budget-estimate.ts`), fixed at `before_model_resolve`, where only the prompt text and attachment kinds are visible (no history, no tool output, no attachment sizes). A long tool loop makes more model calls than the 4 assumed and can **overspend the hold many times over**; reconcile then reports `overrun` after the fact. Mitigations that exist: prompt size is clamped (400,000 chars) and each attachment adds 2,000 assumed tokens (cap 16), and a **per-run tool-call guard** blocks any tool call beyond `maxToolCallsPerRun` (plugin config, default 10, counts attempted calls whether allowed or not) so a tool loop cannot run unbounded. The guard bounds tool round trips, not model calls that follow no tool call, and history growth across a long session is not modelled. Do not read "zero overspend" (spec gate 5) as established for this path.
- `before_agent_run` is evaluated before the first model submission of an attempt; follow-up model calls after a tool result in the same attempt are not separately gated (they spend inside the attempt's hold).
- Runs skipped by `attempt.operation === "settled-tool-finalization"` do not pass `before_agent_run`.
- A run whose model call fails before `llm_output` leaves its reservation held (counted in full) until reconciled by hand; there is no `agent_end` reconcile yet.
- Reconcile uses `llm_output.usage` (a per-attempt aggregate) and the plugin's configured prices; cache read/write tokens are priced as input (upper bound). Missing or malformed usage, and **usage from a model other than the override (failover)**, send `actual_cost_micros: null` (unresolved, never zero or a guessed price; test `failover_model_reconciles_with_unknown_cost`). One reconcile per reservation.

### Tool mapping (`mapping.ts`)

`exec -> shell.exec`, `exec git push <remote> ... -> git.push`, `read -> fs.read`, `ls -> fs.read`, `web_fetch -> web.fetch`. Everything else (`apply_patch`, `process`, `openclaw`, `tool_call`, `sessions_*`, `image`, `web_search`, ...) is denied. `write -> fs.write` and `edit -> fs.edit` are still in the table but **unreachable**: `rules.ts` (`ALLOWED_TOOLS`) denies both before mapping, so they exist only as unit-tested defence in depth (both also refuse `.git` paths). `web_search` is denied by the adapter: its backend is chosen by OpenClaw, so there is no destination for the egress allowlist to check.

`exec` is submitted as `executable` + `args` only when the command is a plain, expansion-free word list: any unquoted `; & | < > ( ) ` $ \ { } * ? [ ] ~ # !` or newline, an unquoted word starting with `=` (zsh `=word` expands to a command path), any `$`/backtick inside double quotes, and any option other than title/timing/pty (`workdir`, `env`, `elevated`, `host`, `security`, `ask`, `node`) is denied; `background` (anything but `false`) and a `timeoutSeconds` that is not a positive number (`0` removes the limit) are denied too.

`git` (the C1 fix): pair-api derives the action class from the PAIR tool name alone, so `git push` sent as `shell.exec` (class `local_edit`) was allowed with no approval or egress check. The adapter now parses git itself. `push` is sent as `git.push` with the first positional (the remote, a name or URL) as `destination`, so the engine returns `needs_approval` and egress-checks it; a push with no explicit remote, or with a flag outside `-u/--set-upstream, -n/--dry-run, -q, -v, --tags, --follow-tags, --no-verify, --atomic` (force, delete, `--receive-pack`, ...), is denied locally. Any other subcommand outside `status, diff, log, show, add, commit, rev-parse, ls-files` is denied, as is any option before the subcommand (`-c`, `-C` unconditionally, `--exec-path`, `--git-dir`, ...) and config/exec flags (`--upload-pack`, `--config-env`, `--ext-diff`, `--output`, ...) anywhere. Any `exec` word or fs path containing a `.git` path component (case-insensitive) is denied, so the `.git/config` (`core.fsmonitor`) route to code execution is closed in the adapter as well as by `ALLOWED_TOOLS`. A remote **name** such as `origin` cannot be resolved by the adapter, so the engine sees host `origin` and denies it at egress; only a URL to an allow-listed host reaches `needs_approval`. Either way the adapter blocks the call (below), and it never honours an `allow` for `git.push`.

PAIR `needs_approval` **blocks**. The adapter cannot mint a PAIR approval (that needs the separate approver token and a human), and an OpenClaw allow-once is not one, so returning `requireApproval` would let the tool run with no PAIR approval consumed. Until the approval flow is wired, anything PAIR holds for approval is denied (`pair: this action needs a PAIR approval ...`; test `needs_approval_from_pair_blocks_until_the_approval_flow_is_wired`). Local rules that ask for approval (network commands, `web_fetch`) are still OpenClaw-only approvals and are kept only when PAIR allowed the call.

The existing local rules run first and a local deny wins without consulting the service.

Other plugins: OpenClaw merges `params` returned by every plugin's `before_tool_call` (`src/plugins/hooks.ts:1093-1100`), so another plugin running after `pair-spike` can change what executes after PAIR approved something else. `pair-spike` must be the **only** plugin that returns `params` from `before_tool_call`; the adapter cannot enforce that. It records the params hash PAIR allowed and compares it at `after_tool_call` (`params-watch.ts`): a mismatch logs an ERROR and an audit line (`params-changed-after-approval`). This is detection after the fact, best effort, and only as good as the `params` OpenClaw reports in that hook (test `other_plugin_params_rewrite_is_detected_and_logged`; not exercised live).

Path checks happen on pair-api's filesystem, against `PAIR_WORKSPACE_ROOT` of the service, not on the OpenClaw host. If the two roots differ, or `PAIR_WORKSPACE_ROOT` is unset (the pair-api compose file and Dockerfile do not set it, only the worker compose does, so every authorize is denied as deployed), the adapter does not work as intended; a symlink inside the OpenClaw workspace is judged on the service's view of the tree. Run pair-api next to the gateway with the same workspace path for now.

### Config

Environment only: `PAIR_API_URL` (a bare origin: a path, query or fragment is rejected rather than dropped; loopback http, or a non-loopback **https** URL only with `PAIR_ADAPTER_ALLOW_REMOTE=1`, because the bearer token travels in the request), `PAIR_SERVICE_TOKEN` (>= 16 chars). Plugin config carries no secrets: log paths, override provider/model, `taskKind`, `priceVersion`, per-Mtok micro prices, optional `maxToolCallsPerRun`. Responses from pair-api are read with a 1 MiB cap. Bad config does not unload the gate: `register` installs deny-all `before_tool_call` and `before_agent_run` handlers and logs the reason (live: phase 9). Logs record tool name, mapped tool, decision, ids, `paramsSha256` and the service's deny reason with path-like tokens replaced by `<path>` (`scrub.ts`; the service's reason can echo resolved filesystem paths, and the same scrubbed text is what the model sees). Params, prompts and tokens are never logged (closes L12 for the plugin). The adapter's own unmappable-call reasons are fixed strings or tool/option names, not values.

### Live harness

```bash
export OPENCLAW_DIR=<pinned checkout>   # read-only; Node 24 on PATH; docker for the Postgres client
cargo build --offline -p pair-api --bin pair-api     # in service/
adapters/openclaw/spike/wired.sh                      # all phases, ~2.5 min; or: wired.sh normal deny ...
scripts/tests/test_openclaw_gate.sh                   # unit tests, plus type-check with the upstream tsc when OPENCLAW_DIR is set
```

`wired.sh` creates `pair_w2_<id>` (never `pair`), lets pair-api apply all 31 migrations (checked against `_sqlx_migrations`), starts the real debug `pair-api` binary on loopback with `config/policy.yaml`, a test registry and budget copies under `spike/wired/`, a loopback mock model server, and the real gateway with a throwaway state dir and `allowConversationAccess=true`; drives turns with `openclaw agent --agent main --session-key ... --json`; asserts; sanitizes evidence into `adapters/openclaw/spike/evidence/wired/`; drops the database. Tokens are random per run. Last run: **48 of 48 checks pass** (`evidence/wired/results.txt`, which records the `git rev-parse HEAD` the plugin was built from; the harness runs `setup-plugin.sh` itself, fails if the plugin tree is dirty, replaces the committed evidence only after a fully passing run, scans the evidence for tokens and host paths, and deletes its temp dir at exit).

Deviation from the brief: the test registry's endpoint is `https://mock-provider.example.com`, not loopback. pair-api's cloud-only guard rejects http and loopback model endpoints at config load with no config bypass (by design), and on the adapter path pair-api never dials a model (only OpenClaw calls the loopback mock). The registry exists to give the budget service a verified price version (`mock-2026-10-03`). `config/models.yaml` is untouched.

| Proof | Phase | What the evidence shows |
|---|---|---|
| (i) real policy engine denies, nothing runs | `p3`, `p4` | Mock model asks for `cat <ws>/prod.tfvars` and `python3 <ws>/write_canary.py`; both pass the local rules. Service decision `deny` (`host credential path denied`; `"python3" can execute arbitrary code and is only permitted inside the sandbox`); the model received `pair policy: ...`; the canary file was never created and the tfvars content never appears in output or mock requests. Control `p2`: `cat <ws>/hello.txt` allowed by the service and executed. |
| (ii) refused reserve means zero model hits | `p7` | Budget with all six caps `0.00` (the kill-switch level 1 file): reserve 402 `budget_exceeded`, run blocked at `before_agent_run`, mock model server saw **0 requests of any kind**, no `budget_reservations` row created. Control `p1`: same prompt with normal caps reaches the mock as `mock-routed`. |
| (iii) pair-api stopped, tools blocked | `p5a`, `p5b` | Reserve succeeds, mock delays its tool call 25 s, pair-api is killed in the window: the tool call is blocked (`deny-on-error`, `unreachable`), content not returned. New run with pair-api down: reserve unreachable, run blocked, mock saw 0 requests. |
| (iv) normal turn leaves a reconciled reservation | `p1` | `budget_reservations`: `settled`, `reserved_micros 17720`, kind `default`, price version `mock-2026-10-03`; `budget_ledger`: settled, 11 in / 7 out, 7 micros, equal to the adapter's computed cost (`db-reservation.txt`, `db-ledger.txt`). Mock usage is a fixed 11/7 per call, so this proves the plumbing, not real token counts. |
| (v) `allowConversationAccess=false` | `p8` | The adapter logs an ERROR at gateway start naming the missing opt-in and registers no model hooks. The model call **still happens** on the default `mock-small` (nothing can gate it), no reservation, no override. The tool gate is deny-all: even a `cat` the policy would allow is blocked (`misconfigured, tool calls are denied`) and does not run. (Before the H4 fix the tool gate kept working and only a gateway WARN line announced the lost model gate.) |
| `git push` (C1) | `p4b` | The mock model asks for `git push origin main`; the adapter sends it to pair-api as tool `git.push` (never `shell.exec`), the decision is not `allow`, and the call is blocked. |
| Token rotation (kill-switch level 3) | `p6a`, `p6b` | pair-api restarted with a new `PAIR_SERVICE_TOKEN` while the gateway keeps the old one: mid-run tool call blocked on HTTP 401; new run refused at reserve with 401, mock saw 0 requests. |
| Misconfigured adapter | `p9` | Non-loopback `PAIR_API_URL`: gateway log `refusing to run ungated`, deny-all handlers, run blocked, mock saw 0 requests. |

Harness bugs found on the way (not adapter bugs): the gateway silently restores a "last good" config backup when it finds an unstamped file (it applied `allowConversationAccess=true` over the false config until the harness re-stamped the file with `openclaw telemetry off` and removed stale `.bak`/`.clobbered` files); and `cd && cmd &` backgrounded the wrong pid, leaving a stale pair-api that answered later runs (that first full run's failures were the stale server and were discarded). The harness now refuses to start if its ports are in use.

### NOT proven

- Approval round trip: PAIR `needs_approval` now blocks (the adapter cannot mint a PAIR approval), so nothing that needs approval can run through this adapter yet. The local-rule `requireApproval` (network commands, `web_fetch`) still goes to OpenClaw's approval surface, which `openclaw agent` CLI turns do not have (spike finding), so allow-once/deny resolution is untested.
- Sandbox mode `all`: the harness ran with sandbox off (`exec` really executes on the host in the allowed case; the allowed command was a read-only `cat` of a scratch file). `python3` and other code-exec executables are denied by the policy precisely because no sandbox is declared to the engine.
- Real providers, real token counts and prices: mock model and mock registry only. The 16,000-token overhead constant is calibrated from one mock run.
- Follow-up model calls inside an attempt are not individually gated (see limits above); mid-run budget exhaustion is not stopped; the budget for this path is an estimate and can be overspent (see limits). The per-run tool-call guard is unit-tested only, not driven live.
- A real multi-attempt run (compaction, retry, auth rotation, failover) against the gateway: the second-attempt block and the failover unknown-cost reconcile are proven with a fake api and a mock pair-api, not by provoking OpenClaw into a second attempt.
- The after-hook params-rewrite detection against a real second plugin.
- `before_agent_run` semantics on Codex/Copilot harnesses; host behaviour when `before_model_resolve` times out.
- Concurrent runs sharing the plugin (the per-run map is keyed by `runId`; one run at a time was driven).
- A PAIR-side readiness check for the gateway's `allowConversationAccess` state is not implemented (the adapter detects and logs it itself).
