# OpenClaw upstream audit (Task 1)

Audited: 2026-10-03. Method: source read at the pinned tag plus unauthenticated GitHub API. Paths are relative to the OpenClaw checkout.

## Pin

| Item | Value |
|---|---|
| Repo | https://github.com/openclaw/openclaw |
| Tag | `v2026.9.7` (committed 2026-09-29) |
| SHA | `c074824a27c96d3983043f9eeb33823cd1772d8c` |
| Newer line | `v2026.8.35` (extended-stable maintenance) published 2026-10-02; `main` is far ahead |
| Runtime | Node `>=24.16.0 <25 \|\| >=26.1.0` (installed 24.21.0), pnpm 12.5.1 (`package.json` engines, packageManager) |
| Schema versions | state 19, agent 24 (`package.json:6-9`, `src/state/openclaw-state-db-contract.ts:22`). The spec-era note of "state 20" was wrong. |

## Capability matrix

Status: verified = read in source at the pinned SHA. All hook APIs are labelled **experimental** (`docs/plugins/sdk-overview.md:27`).

| Capability | Status | Contract | Evidence |
|---|---|---|---|
| Out-of-tree plugins | verified | `openclaw.plugin.json` manifest + `definePluginEntry({id,name,description,kind,configSchema,register})`; version gate `openclaw.compat.pluginApi`; install via `openclaw plugins install` or `plugins.load.paths` | `docs/plugins/manifest.md:9-24`, `src/plugin-sdk/plugin-entry.ts:236-260`, `docs/plugins/manifest/package-json.md:36-63` |
| Context injection | verified | `before_prompt_build` → `{systemPrompt, prependContext, appendContext, prependSystemContext, appendSystemContext, toolsAllow}`; exclusive `ContextEngine.assemble({messages,tokenBudget,...})` slot | `src/plugins/hook-types.ts:189-316`, `src/context-engine/types.ts:353,455-475` |
| Memory slot | verified | `registerMemoryCapability`; `MemorySearchManager` needs `search, readFile, status, probe*`; slot `plugins.slots.memory` | `src/plugins/registry-contribution-types.ts:219-292`, `packages/memory-host-sdk/src/host/types.ts:383-426` |
| Model override | verified | `before_model_resolve` → `{providerOverride?, modelOverride?}`; or `models.providers.<id>{baseUrl,api,apiKey,models[]}` for an OpenAI-compatible PAIR router | `src/plugins/hook-types.ts:971-980`, `src/config/zod-schema.core.ts:552-577` |
| Tool gate | verified | `before_tool_call` → `{params?, block?, blockReason?, requireApproval?}`; throw or 15 s timeout blocks (fail closed); no conversation opt-in needed | `src/plugins/hook-before-tool-call-result.ts:14-35`, `docs/plugins/hooks/tool-policy.md:65-86` |
| Telemetry | verified | `llm_output` (usage, resolvedRef), `model_call_ended`, `onDiagnosticEvent` (`model.usage` incl. `costUsd`); OTel via bundled `diagnostics-otel` | `src/plugins/hook-types.ts:334-401`, `src/infra/diagnostic-events.ts` |
| Durable state | partial | SQLite (`node:sqlite`+Kysely); plugin `openKeyedStore`/`openBlobStore`, `registerService`. `scheduleSessionTurn` is bundled-only; plugin Tasks/TaskFlow removed | `docs/plugins/sdk-runtime.md:380`, `src/plugins/runtime/types-core.ts:514-522` |
| Sandbox | verified, **off by default** | `agents.defaults.sandbox{mode: off\|non-main\|all, backend, workspaceAccess, docker{network default "none", capDrop, readOnlyRoot}}` | `src/config/zod-schema.sandbox.ts:38-110`, `docs/gateway/sandboxing.md:9-29` |
| Gateway auth | verified | `gateway.auth.mode none\|token\|password\|trusted-proxy`; default bind loopback :18789; non-loopback needs auth | `src/config/zod-schema.gateway.ts:105-110`, `docs/gateway/config-gateway.md:20-26` |
| Data / export | verified | `~/.openclaw/state/openclaw.sqlite`, per-agent sqlite, Markdown memory; `openclaw backup create\|verify\|restore`, `sessions export-trajectory` | `docs/cli/backup.md:14-30`, `docs/concepts/memory.md:20-22` |
| Disable local providers | verified | `plugins.deny: [ollama,lmstudio,vllm,sglang,llama-cpp]` (deny wins); `agents.defaults.modelPolicy.allow` | `docs/tools/plugin.md:197-216`, `extensions/ollama/openclaw.plugin.json:10` |
| Upgrades | verified | `openclaw update` with channels, rollback, doctor; schema versions checked on candidate | `docs/cli/update.md:284-289`, `src/infra/update-candidate-state.ts:77-110` |
| Tests | partial | Vitest 5; `pnpm test`, `pnpm test:contracts`. Plugin contract helpers are repo-local, not published | `package.json:1991-2005`, `docs/plugins/sdk-testing.md:20-24` |

**Gotcha:** conversation hooks (`before_model_resolve`, `before_prompt_build`, `llm_input`, `llm_output`) are silently dropped for non-bundled plugins unless `plugins.entries.<id>.hooks.allowConversationAccess=true` (`src/plugins/hook-policy-decisions.ts:10-17`).

## License and dependencies

| Area | Finding | Risk |
|---|---|---|
| License | MIT, © 2026 OpenClaw Foundation (`LICENSE`, `package.json`). GitHub API shows `NOASSERTION` (detector quirk). | low |
| Notices | `THIRD_PARTY_NOTICES.md`: Pi/pi-mono (MIT), GitHub Octicons (MIT). Icon bundles MIT/CC0. | low |
| Trademark | No policy file. Name/logo reuse needs Foundation consent if commercialized (`docs/start/lore.md:18`). | low-med |
| Copyleft | `extensions/whatsapp` → `baileys` → `libsignal` (**GPL-3.0, confirmed** in `docs/license-scan.md`; also behind `@openclaw/qa-lab`); `mpg123-decoder` is MIT (earlier note of LGPL-2.1 was wrong); GPL-2.0/LGPL `@audio/decode-*` packages and `codec-parser` exist behind `@openclaw/whatsapp`; `jszip` dual MIT/GPLv3. Lockfile has no license data. | med, only if WhatsApp shipped |
| Action | Run `pnpm licenses list` after install and attach output; do not enable WhatsApp plugin. | |

## Security posture

| Area | Finding | Risk |
|---|---|---|
| Policy | `SECURITY.md`: single trusted operator, not multi-tenant; prompt injection alone out of scope; no supported-versions table | med |
| Advisories | 722 published (14 critical, 249 high); 75 on 2026-09-11. Range check found none covering 2026.9.7; highest patched is 2026.9.3. Unauthenticated, re-check with `gh auth login`. | med |
| Sandbox | default `off`, "not a perfect security boundary" (`docs/gateway/sandboxing.md`). Past sandbox-escape criticals (GHSA-g5cg-8x5w-7jpm, GHSA-9p3r-hh9g-5cmg). | **high for PAIR** |
| Telemetry | Daily `GET telemetry.openclaw.ai/api/latest-version` on by default; opt-in feature stats; ClawHub install telemetry. Disable: `openclaw telemetry off`, `OPENCLAW_NO_AUTO_UPDATE=1`, `CLAWHUB_DISABLE_TELEMETRY=1`. | med |
| Maintenance | 17k commits/30 days, 423 authors, ~65% from one person; 23 tags in 60 days | med (churn, bus factor) |

## Build

| Step | Result |
|---|---|
| `pnpm install --frozen-lockfile` | pass, 2m34s (Node 24.21.0) |
| `pnpm build` | pass, 14m55s on Apple Silicon (slowest: d.ts generation 8m45s). `node openclaw.mjs --version` → `OpenClaw 2026.9.7 (c074824)` |
| `pnpm test` baseline | not yet run (deferred; full suite is large, run `pnpm test:contracts` for the plugin spike) |

## Not yet demonstrated (Task 1 exit criteria still open)

0. DONE: plugin spike (`docs/spike-openclaw-plugin.md`): hooks verified except approval round trip, sandbox=all, before_prompt_build, memory capability, onDiagnosticEvent. License scan done (`docs/license-scan.md`).
1. Authenticated cloud inference through the runtime: needs `ANTHROPIC_API_KEY` for a spend-capped `pair` workspace (owner action, see `provider-billing.md`).
2. One extension point exercised live: a spike plugin using `before_tool_call` (block) and `before_model_resolve` against the built gateway.
3. License scan output and authenticated advisory re-check.
