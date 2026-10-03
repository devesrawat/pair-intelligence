# OpenClaw plugin spike (Task 1)

Run: 2026-10-03 against the built gateway at `v2026.9.7` (`c074824a27c9`), Node 24.21.0, macOS arm64. The OpenClaw checkout was used read-only (execute and read; no install, rebuild or write).

**Result: pass.** A hook-only out-of-tree plugin loaded into the real gateway, blocked a tool call, requested approval for another, overrode the model, and wrote usage records, with no real model, no API key and no non-loopback traffic observed. Every claim below is backed by a committed file under `adapters/openclaw/spike/evidence/`.

## Artifacts

| Path | Purpose |
|---|---|
| `adapters/openclaw/pair-spike/openclaw.plugin.json` | Manifest, `configSchema`, `activation.onStartup` |
| `adapters/openclaw/pair-spike/package.json` | `openclaw.extensions`, `openclaw.runtimeExtensions`, `openclaw.compat.pluginApi >=2026.9.7` |
| `adapters/openclaw/pair-spike/src/index.ts`, `src/rules.ts` | `definePluginEntry` + four hooks; pure policy in `rules.ts` |
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
| `llm_output` | `{runId, provider, model, resolvedRef, usage{input,output,cacheRead,cacheWrite,total,cost{total}}, assistantTexts, prompt}` | none (observer) | Yes | `usage` is **summed over all model calls in the run** (22/14 for a run with a tool call and a follow-up). One record per run, not per call. |

Implication for the ADR: a per-call budget ledger cannot get token counts from `model_call_ended`; it must use `llm_output` (per run, post hoc, opt-in required) or `onDiagnosticEvent` `model.usage` (not exercised here). Pre-call reservation belongs in `before_model_resolve`, which only sees the prompt text, so estimate from `prompt.length` there.

The tool list sent to the mock (`mock-requests.jsonl`, `tools`) is the default agent surface: `apply_patch, edit, exec, ls, openclaw, process, read, sessions_yield, tool_call, tool_describe, tool_search, write`. Sandbox mode was **off** in this spike, so `exec` would have run on the host if not blocked.

## Divergences from `docs/upstream-audit.md`

1. Conversation-hook gotcha: not silent. A warn-level log line names each blocked hook at start (see above). The audit's table row "Tool gate: no conversation opt-in needed" is confirmed. Add: `model_call_ended` also needs none.
2. `before_model_resolve` is documented as `{providerOverride?, modelOverride?}`; confirmed, with the extra finding that the CLI reports the reroute (`modelRouteChange`).
3. Telemetry row: `model_call_ended` has no usage; `llm_output.usage` is per-run aggregate. The audit listed them side by side as if equivalent.
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

Design: `before_model_resolve` reserves via `POST /v1/budget/reserve` and records the outcome per `ctx.runId`; the override is returned only on success. `before_agent_run` blocks the run unless that run has a successful reservation. A missing record (hook never fired, locked model selection, no run id, any error) means block. `llm_output` reconciles. `ctx.runId` is populated in all three hooks and matches `llm_output.event.runId` (observed live, `evidence/wired/p1-normal-turn/hooks.jsonl`).

Honest limits of the model-call gate:

- It exists only while `allowConversationAccess=true` (all three hooks are conversation hooks). With it off, **model calls are not gated at all** (proven below, phase 8). A startup check on the gateway's WARN lines is still a to-do for PAIR readiness.
- One reservation covers one run. `before_agent_run` is evaluated before the first model submission of an attempt; follow-up model calls after a tool result in the same run are not separately gated (they spend inside the run's reservation). The reservation is an estimate (`budget.ts`: prompt chars / 4 + 16,000 overhead tokens, times 4 assumed calls, plus 2,048 output tokens per call). The overhead constant was calibrated from one live request (24-char prompt produced a 53 KB request). If real usage exceeds the hold, reconcile reports `overrun`; it cannot be stopped mid-run.
- Runs skipped by `attempt.operation === "settled-tool-finalization"` do not pass `before_agent_run`.
- A run whose model call fails before `llm_output` leaves its reservation held (counted in full) until reconciled by hand; there is no `agent_end` reconcile yet.
- Reconcile uses `llm_output.usage` (per-run aggregate) and the plugin's configured prices; cache read/write tokens are priced as input (upper bound). Missing or malformed usage sends `actual_cost_micros: null` (unresolved, never zero). Only one reconcile per run is sent.

### Tool mapping (`mapping.ts`)

`exec -> shell.exec`, `read -> fs.read`, `ls -> fs.read`, `write -> fs.write`, `edit -> fs.edit`, `web_search -> web.search`, `web_fetch -> web.fetch`. Everything else (`apply_patch`, `process`, `openclaw`, `tool_call`, `sessions_*`, `image`, ...) is denied. `exec` is submitted as `executable` + `args` only when the command is a plain, expansion-free word list: any unquoted `; & | < > ( ) ` $ \ { } * ? [ ] ~ # !` or newline, any `$`/backtick inside double quotes, and any option other than title/timing/pty (`workdir`, `env`, `elevated`, `host`, `security`, `ask`, `node`) is denied. The existing local rules run first and a local deny wins without consulting the service; local approval is kept when the service allows. The local allow-list currently denies `write` and `edit` (spike-level), so those mappings are unit-tested but unreachable live.

### Config

Environment only: `PAIR_API_URL` (loopback unless `PAIR_ADAPTER_ALLOW_REMOTE=1`), `PAIR_SERVICE_TOKEN` (>= 16 chars). Plugin config carries no secrets: log paths, override provider/model, `taskKind`, `priceVersion`, per-Mtok micro prices. Bad config does not unload the gate: `register` installs deny-all `before_tool_call` and `before_agent_run` handlers and logs the reason (live: phase 9). Logs record tool name, mapped tool, decision, ids, `paramsSha256` and the service's deny reason; never params, prompts or tokens (closes L12 for the plugin).

### Live harness

```bash
export OPENCLAW_DIR=<pinned checkout>   # read-only; Node 24 on PATH; docker for the Postgres client
cargo build --offline -p pair-api --bin pair-api     # in service/
adapters/openclaw/spike/wired.sh                      # all phases, ~2.5 min; or: wired.sh normal deny ...
scripts/tests/test_openclaw_gate.sh                   # unit tests, plus type-check with the upstream tsc when OPENCLAW_DIR is set
```

`wired.sh` creates `pair_w2_<id>` (never `pair`), lets pair-api apply all 24 migrations (checked against `_sqlx_migrations`), starts the real debug `pair-api` binary on loopback with `config/policy.yaml`, a test registry and budget copies under `spike/wired/`, a loopback mock model server, and the real gateway with a throwaway state dir and `allowConversationAccess=true`; drives turns with `openclaw agent --agent main --session-key ... --json`; asserts; sanitizes evidence into `adapters/openclaw/spike/evidence/wired/`; drops the database. Tokens are random per run. Last run: **43 of 43 checks pass** (`evidence/wired/results.txt`).

Deviation from the brief: the test registry's endpoint is `https://mock-provider.example.com`, not loopback. pair-api's cloud-only guard rejects http and loopback model endpoints at config load with no config bypass (by design), and on the adapter path pair-api never dials a model (only OpenClaw calls the loopback mock). The registry exists to give the budget service a verified price version (`mock-2026-10-03`). `config/models.yaml` is untouched.

| Proof | Phase | What the evidence shows |
|---|---|---|
| (i) real policy engine denies, nothing runs | `p3`, `p4` | Mock model asks for `cat <ws>/prod.tfvars` and `python3 <ws>/write_canary.py`; both pass the local rules. Service decision `deny` (`host credential path denied`; `"python3" can execute arbitrary code and is only permitted inside the sandbox`); the model received `pair policy: ...`; the canary file was never created and the tfvars content never appears in output or mock requests. Control `p2`: `cat <ws>/hello.txt` allowed by the service and executed. |
| (ii) refused reserve means zero model hits | `p7` | Budget with all six caps `0.00` (the kill-switch level 1 file): reserve 402 `budget_exceeded`, run blocked at `before_agent_run`, mock model server saw **0 requests of any kind**, no `budget_reservations` row created. Control `p1`: same prompt with normal caps reaches the mock as `mock-routed`. |
| (iii) pair-api stopped, tools blocked | `p5a`, `p5b` | Reserve succeeds, mock delays its tool call 25 s, pair-api is killed in the window: the tool call is blocked (`deny-on-error`, `unreachable`), content not returned. New run with pair-api down: reserve unreachable, run blocked, mock saw 0 requests. |
| (iv) normal turn leaves a reconciled reservation | `p1` | `budget_reservations`: `settled`, `reserved_micros 17720`, kind `default`, price version `mock-2026-10-03`; `budget_ledger`: settled, 11 in / 7 out, 7 micros, equal to the adapter's computed cost (`db-reservation.txt`, `db-ledger.txt`). Mock usage is a fixed 11/7 per call, so this proves the plumbing, not real token counts. |
| (v) `allowConversationAccess=false` | `p8` | WARN lines for `before_model_resolve`, `before_agent_run` and `llm_output` at gateway start. The model call **happens** on the default `mock-small`, no reservation is created, no override: the budget gate is gone. `before_tool_call` still fires and the real engine still denies. |
| Token rotation (kill-switch level 3) | `p6a`, `p6b` | pair-api restarted with a new `PAIR_SERVICE_TOKEN` while the gateway keeps the old one: mid-run tool call blocked on HTTP 401; new run refused at reserve with 401, mock saw 0 requests. |
| Misconfigured adapter | `p9` | Non-loopback `PAIR_API_URL`: gateway log `refusing to run ungated`, deny-all handlers, run blocked, mock saw 0 requests. |

Harness bugs found on the way (not adapter bugs): the gateway silently restores a "last good" config backup when it finds an unstamped file (it applied `allowConversationAccess=true` over the false config until the harness re-stamped the file with `openclaw telemetry off` and removed stale `.bak`/`.clobbered` files); and `cd && cmd &` backgrounded the wrong pid, leaving a stale pair-api that answered later runs (that first full run's failures were the stale server and were discarded). The harness now refuses to start if its ports are in use.

### NOT proven

- Approval round trip: `requireApproval` is returned with the PAIR payload hash and `allowedDecisions: [allow-once, deny]`, but `openclaw agent` CLI turns have no approval-capable surface (spike finding), so allow-once/deny resolution is untested. Also, an OpenClaw-side allow-once is not a PAIR approval: nothing mints one through `/v1/approvals` (needs the separate approver token) and the tool would then execute without a PAIR approval being consumed.
- Sandbox mode `all`: the harness ran with sandbox off (`exec` really executes on the host in the allowed case; the allowed command was a read-only `cat` of a scratch file). `python3` and other code-exec executables are denied by the policy precisely because no sandbox is declared to the engine.
- Real providers, real token counts and prices: mock model and mock registry only. The 16,000-token overhead constant is calibrated from one mock run.
- Follow-up model calls inside a run are not individually gated (see limits above); mid-run budget exhaustion is not stopped.
- `before_agent_run` semantics across retries, compaction and Codex/Copilot harnesses; host behaviour when `before_model_resolve` times out.
- Concurrent runs sharing the plugin (the per-run map is keyed by `runId`; one run at a time was driven).
- A PAIR-side readiness check for the gateway's `allowConversationAccess` WARN is not implemented.
