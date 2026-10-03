# Provider billing and authentication verification

Checked: 2026-10-03. Method: official docs fetched via summarizing web tool (figures paraphrased, not verbatim).
Status legend: **verified** (official page read), **partial**, **UNVERIFIED** (not found or snippet-only).
Account-level rows (`owner account`) require the owner to confirm in each console; they are not done.

Rule from spec §5: unknown pricing ⇒ automatic paid execution disabled for that provider.

## 1. TypeSafe Jev (classifier)

| Fact | Value | Source | Status |
|---|---|---|---|
| Endpoint | `POST https://api.typesafe.ai/v1/systemone` | https://docs.typesafe.ai/api | verified |
| Auth | `Authorization: Bearer <API_KEY>` | https://docs.typesafe.ai/api | verified |
| Request shape | `{state, model, questions: {id: Question}}` | https://docs.typesafe.ai/api | verified |
| Question types | Noul (yes/no 0–1), Choice (≤255 options, chosen option + full distribution), Score (2–10 level rubric) | https://docs.typesafe.ai/api | verified |
| Response | `{model, answers, usage{input, output}}`; `model` = resolved version | https://docs.typesafe.ai/api | verified |
| Errors | 401, 422, 429, 529 | https://docs.typesafe.ai/api | verified |
| Price | $0.042 / M input tokens; output free | https://docs.typesafe.ai/models | verified |
| Limits | 64k total/request; 32k state + longest question; text input only | https://docs.typesafe.ai/models | verified |
| Rate limits | 100K tok/s, 40 req/s; "adjust dynamically" without notice | https://docs.typesafe.ai/models | verified |
| Version pinning | Aliases `jev-latest`, `jev-preview` (both 1.13.0 today). Sending `jev-1.13.0` directly | https://docs.typesafe.ai/models | **UNVERIFIED** |
| SDKs | TS `@typesafe-ai/sdk`, Python `typesafe_sdk`; no Rust SDK | docs.typesafe.ai/sdk | partial |
| Training / retention | No training on input; ZDR enterprise-only; default retention period | typesafe.ai/legal/privacy-policy | **UNVERIFIED** |
| Owner account: key, prepaid balance, spend cap | — | owner account | **pending owner** |

Spec corrections:
- "Intent routing" is not a named vendor feature; PAIR implements it with a Choice question.
- "Confidence = top option probability" is a PAIR convention over the Choice distribution, not a vendor statistic. Spec §6.1 wording should be read that way.
- Vendor recommends retrying 429/529; spec §6.1 says no synchronous retry on the interactive path. Spec wins: fall back to baseline.

## 2. Ollama Cloud (routine/strong generation)

| Fact | Value | Source | Status |
|---|---|---|---|
| Server API | `https://ollama.com/api/chat`, `Authorization: Bearer <key>` | https://docs.ollama.com/cloud | verified |
| Model catalog | `GET https://ollama.com/api/tags` (API IDs e.g. `gemma4:31b`) | https://docs.ollama.com/cloud | verified |
| OpenAI/Anthropic-compatible base URL | exists per docs; exact URL | https://docs.ollama.com/cloud | **UNVERIFIED** |
| Plans | Free $0, Pro $20/mo, Max $100/mo, Team $500/mo | https://ollama.com/pricing | verified |
| Included credits | Pro $60/mo, Max $300/mo; reset on anniversary, no rollover | https://ollama.com/pricing | verified |
| Metering | Credit-based, per-token by model | https://ollama.com/pricing | verified (summary) |
| Per-model token prices | table not extracted | https://ollama.com/pricing | **UNVERIFIED** |
| Concurrency | Free 1, Pro 3, Max 10; excess queued | https://ollama.com/pricing | verified |
| Data | Prompts/responses not logged or trained on; US primary, EU/SG routing | https://ollama.com/pricing | verified |
| Server/automation use permitted by ToS | not stated | ollama.com terms | **UNVERIFIED** |
| Overage behavior after credits exhausted | — | ollama.com/settings/usage | **pending owner** |

Implication: Ollama Cloud usage is a fixed subscription with included credits, not pure metering. Dashboard must show fixed ($20) and credit consumption separately (spec §5).

## 3. Anthropic Claude (premium generation)

| Fact | Value | Source | Status |
|---|---|---|---|
| Pro/Max include API access | **No** | support.claude.com/en/articles/9876003 | verified |
| Agent SDK / `claude -p` on subscription | Currently draws from subscription limits; policy explicitly under revision | support.claude.com/en/articles/15036540 | verified, unstable |
| Third-party subscription login | Not allowed unless approved; "use the API key authentication methods" | code.claude.com/docs/en/agent-sdk/overview | verified |
| Auth | `x-api-key` or Bearer; `anthropic-version: 2023-06-01`; `https://api.anthropic.com` | platform.claude.com/docs/en/api/overview | verified |
| Prices (in/out per MTok) | Opus 5.5 $4/$20; Sonnet 5.5 $2/$10; Haiku 4.5 $1/$5; Fable 5.1 $10/$50; batch −50% | platform.claude.com/docs/en/about-claude/pricing | verified |
| Model IDs | `claude-opus-5-5` seen in docs; others resolve via `GET /v1/models` | platform.claude.com | partial |
| Spend cap | Per-workspace monthly limit + alerts; not on Default Workspace | platform.claude.com/docs/en/manage-claude/workspaces | verified |
| Retention | 30-day default, no training; Fable/"covered models" excluded from ZDR | api-and-data-retention page | **UNVERIFIED** (snippet) |
| Owner account: Console org, dedicated `pair` workspace, cap set | — | owner account | **pending owner** |

## Subscription terms (verified 2026-10-03 against primary sources)

Sources: Consumer Terms of Service (effective 8 Oct 2025) https://www.anthropic.com/legal/consumer-terms ; Claude Code legal and compliance https://code.claude.com/docs/en/legal-and-compliance ; authentication https://code.claude.com/docs/en/authentication ; headless https://code.claude.com/docs/en/headless.

| Question | Finding |
|---|---|
| Is scripted/automated use of a subscription allowed? | Consumer Terms s.3 bars automated or non-human access "except when you are accessing our Services via an Anthropic API Key or where we otherwise explicitly permit it". Anthropic's docs explicitly document `claude -p` (non-interactive) and `claude setup-token` / `CLAUDE_CODE_OAUTH_TOKEN` "for CI pipelines and scripts where browser login isn't available", authenticating "with your Claude subscription". That is the explicit permission, for the **unmodified Claude Code binary**. |
| Using it from our own service code? | Not permitted: developers building products or services that interact with Claude "should use API key authentication"; no routing requests through Free/Pro/Max credentials on behalf of users; no collecting, storing or intermediating Claude.ai credentials. PAIR therefore never reads or forwards the credential: only the CLI does, and PAIR spawns it. |
| Limits | OAuth is "designed to support ordinary use"; advertised Pro/Max limits "assume ordinary, individual usage of Claude Code and the Agent SDK". The adapter stops at 95% of any provider-reported window and never spends paid overage. |
| Enforcement | Anthropic "may do so without prior notice". |

**Conclusion:** the `claude_code` provider is within the published terms for **one owner running their own subscription on their own host through the unmodified CLI**. It is **not** permitted if PAIR ever serves another person, runs a hosted multi-user service, or modifies/wraps the CLI's authentication (use the API-key mode there). Interpretation of "ordinary, individual usage" for a 24/7 personal automation is Anthropic's; if in doubt, ask via https://www.anthropic.com/contact-sales. Re-verify when these pages change.

## Decision (provisional, pending owner rows)

- PAIR supports **two** Anthropic auth modes: (a) an API key in a dedicated `pair` workspace with a monthly spend limit ≤ the PAIR metered cap (metered, dollar-budgeted); (b) the owner's own subscription through the unmodified `claude` CLI (`claude_code` provider, see "Subscription terms" below). Never treat a subscription as an API entitlement: the OpenClaw `claude-cli/*` route and any use of subscription tokens outside the unmodified CLI remain **not** used.
- Ollama Cloud: candidate only after owner confirms plan, ToS automation permission and per-model prices. Until then, automatic paid execution via Ollama is disabled.
- Jev: shadow mode only; pin by recording returned `model` field on every call until direct version pinning is verified.
- Assumed SDK credit: **$0** (earlier "$20 SDK credit / $40 total" claims are unsupported).

## Owner checklist

- [ ] Anthropic Console: create workspace `pair`, set monthly spend limit, create key → `.env` `ANTHROPIC_API_KEY`
- [ ] Ollama: confirm plan, overage behavior, ToS for server use, copy per-model prices
- [ ] TypeSafe: create key, confirm billing mode (prepaid/postpaid), read default retention
- [ ] Record each with date checked in this file
