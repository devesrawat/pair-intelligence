# ADR 001: Foundation and integration boundary

Status: **Accepted with conditions** (2026-10-03). Plugin spike passed against the built gateway (`docs/spike-openclaw-plugin.md`). Still open: authenticated cloud inference, real approval round trip, sandbox mode `all`, advisory re-check.
Date: 2026-10-03

## Decision

1. Adopt OpenClaw pinned at `v2026.9.7` (`c074824a27c9`) as the runtime, conditional on the open spike items.
2. **Hybrid boundary:** one thin external OpenClaw plugin (TypeScript, the adapter) plus a **companion PAIR service** for budget, policy, memory and routing state.
   - The plugin only wires hooks: `before_model_resolve`, `before_prompt_build`, `before_tool_call`, `llm_output`/`model_call_ended`, and registers the memory capability. Each hook calls the service; the plugin holds no business logic.
   - The service owns PostgreSQL state (budget ledger, approvals, memory, audit). Rust/Axum is justified by this: atomic budget reservation and the durable job table need a real transactional store, but OpenClaw's plugin state is schema-less JSON in SQLite, plugin scheduling is bundled-only, and the API is experimental and churns weekly. Keeping authoritative state outside the upstream process limits upgrade blast radius.
3. Plugin-only is **rejected** for V1: spec §6 needs serialized cross-process reservations and §10 needs durable jobs, neither of which the plugin runtime supports (`scheduleSessionTurn` bundled-only, no TaskFlow).
4. Upstream patches: none planned. Fallback if a hook proves insufficient: smallest isolated patch with a contract test (spec §3 order).

## Required deployment config (enforces spec global constraints)

- `plugins.deny: [ollama, lmstudio, vllm, sglang, llama-cpp]` (no local inference)
- `agents.defaults.sandbox.mode: all`, `network: none`, `workspaceAccess` per task
- `gateway.auth.mode: token`, bind loopback behind reverse proxy
- `plugins.entries.pair.hooks.allowConversationAccess: true` (only for PAIR)
- `OPENCLAW_NO_AUTO_UPDATE=1`, `openclaw telemetry off`, `CLAWHUB_DISABLE_TELEMETRY=1`, `OPENCLAW_DISABLE_BONJOUR=1` (mDNS advertising is on by default)
- Deny the bundled `acpx` plugin (pulls `@anthropic-ai/claude-agent-sdk`, no OSS license). Do not ship `@openclaw/whatsapp` or `@openclaw/qa-lab` (libsignal GPL-3.0 confirmed in `docs/license-scan.md`)
- Anthropic via API key in a capped workspace; never `claude-cli/*` subscription routes (see `provider-billing.md`)

## Risks accepted

- All plugin APIs experimental: pin version, add adapter contract tests, rehearse upgrades in staging.
- Single-maintainer concentration and 700+ advisories: track the advisory feed; treat OpenClaw sandbox as defense in depth, not the only boundary. The PAIR policy check runs in `before_tool_call` and fails closed.
- Fixed host cost of two processes (gateway + service) vs plugin-only.

## Fallback

If `before_model_resolve` or `before_tool_call` do not behave as documented in the spike, run PAIR as an OpenAI-compatible endpoint registered via `models.providers.pair` and enforce tool policy in the sandbox backend instead; if neither works, revisit foundation (spec §3, step 4).

## Amendments from the spike (2026-10-03)

- **Budget accounting cannot use `model_call_ended`:** it carries no token counts. Reserve in `before_model_resolve` from prompt-length estimates; settle from `llm_output.usage` (per run, post hoc, needs `allowConversationAccess`) or diagnostics events.
- **Approvals:** `requireApproval` from a CLI turn denies immediately (no approval-capable surface). Fail-closed, but the real allow/deny round trip must be tested on the actual approval surface before Task 3 is considered closed against OpenClaw.
- **Conversation hooks are not silent when dropped:** the gateway logs a warn line at start naming each blocked hook; add a startup check that fails PAIR's own readiness if that warning appears.
- **`openclaw telemetry off` rewrites config and strips JSON5 comments;** manage config from a template, not in place.
