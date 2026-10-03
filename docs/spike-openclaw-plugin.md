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
- Known gap (L12, deferred): the hook log still records full tool params.
