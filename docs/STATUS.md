# PAIR status

Honest, tracked snapshot of what exists, what is tested, and what is not. Written for branch `feat/phase-0-audit` after the four-reviewer pass in [review-findings.md](review-findings.md). It replaces ad hoc claims in the working ledger (which is git-ignored and therefore not reproducible). If this file and a runbook or commit message disagree, this file was written last; if it disagrees with the code, the code wins and this file is a bug.

"Tested" means the automated suite passes. Per spec section 2, passing tests do not establish semantic correctness or security, and **none of the section 2 release gates has been measured** (see below).

## 1. Implemented and tested (automated)

| Area | What exists | Where it is tested |
|---|---|---|
| `pair-api` HTTP surface | `/healthz` (open), `/readyz`, `/v1/whoami`; bearer service token + `X-Actor` (`[A-Za-z0-9._:@-]{1,64}`); single `X-Trace-Id` header requiring a canonical UUID (otherwise regenerated); request timeout (30 s) and in-flight cap (64) returning 504 / 503; SIGTERM and Ctrl-C graceful shutdown; config secrets wrapped so `Debug` never prints them | `crates/api/tests/{http,hardening,limits}.rs`, unit tests in `crates/api/src/{auth,config,shutdown}.rs`, `scripts/check smoke` (boots the real binary against a scratch database) |
| Readiness strictness | Explicitly configured `PAIR_MODELS_CONFIG` / `PAIR_MIGRATIONS_DIR` that do not exist fail startup; zero migrations on disk or applied is critical in the service binary; empty provider registry is a warning; error details carry no paths or OS errors | `startup_fails_when_configured_models_config_missing`, `readyz_fails_when_no_migrations_applied`, `readyz_warns_on_stub_providers`, `readyz_error_details_do_not_leak_paths_or_os_errors` |
| Telemetry | Trace-id codec (`x-trace-id`, UUID only), structured tracing setup, secret redaction, disk and backlog health evaluation | `crates/telemetry` unit tests |
| Container image | `deploy/api/Dockerfile` bakes migrations and `config/models.yaml`, pinned base tags, `.dockerignore`. Built and booted once by hand on the dev machine (read-only root filesystem, all capabilities dropped, `docker stop` took 1 s via SIGTERM). Not built in CI | manual, 2026-10-03 |
| Compose | `deploy/compose.yaml` refuses to render without `POSTGRES_PASSWORD`; pair-api has `read_only`, `cap_drop: [ALL]`, `no-new-privileges`, tmpfs; dev-only database in `deploy/compose.dev.yaml` | `scripts/tests/test_compose.sh` |
| Backup / restore tooling | Encrypted (age) dump, 7 daily + 4 weekly retention with exact-name matching and loud failure; restore verifies everything first, restores into `<db>_incoming`, renames on success; live-db guard on `PAIR_LIVE_DB`; `--drop-existing` always confirmed; private file modes; docker mode keeps the password out of argv and refuses non-loopback hosts | `scripts/tests/test_{restore,retention,retention_safety,backup_perms,pgtools}.sh`; `scripts/restore-drill --seed-demo` (CI) |
| OpenClaw spike plugin | Tool-name allow-list with default deny, extended destructive-command checks, fail-closed gate that cannot throw (including an unwritable log path) | `scripts/tests/test_openclaw_gate.sh` (policy and gate in isolation; **not** against a live gateway since the hardening) |
| Library crates | `core`, `policy`, `budget`, `models`, `memory`, `context`, `jobs`, `workflows` have their own suites | `cargo test --workspace`, `scripts/check <suite>` |

CI (`.github/workflows/ci.yml`) runs fmt, clippy `-D warnings`, the workspace tests, the smoke check, every `scripts/tests/*.sh`, and a seeded restore drill, against the dev compose database. GitHub Actions are pinned to commit SHAs.

## 2. Library-only: not wired into the running `pair-api`

`pair-api` depends on `pair-core`, `pair-telemetry` and `pair-models` (for provider readiness) only. These crates compile and have tests but nothing in the running service or the OpenClaw adapter calls them:

- `budget` (reservations, ledger, caps)
- `jobs` (durable runs, leases, approvals, effect intents)
- `workflows` (coding, research, daily)
- the router in `models` (provider calls exist as a library; `pair-api` only reads the registry for `/readyz`)
- `memory` (store, inbox, retrieval, export)
- `context` (compiler)
- `policy` (the gate); the OpenClaw spike plugin has its own small allow-list, which is not the section 9 policy

Consequences: no budget cap, job interruption or policy gate is enforced by the deployed service. The kill-switch runbook marks those levels NOT EFFECTIVE; the only effective spend control is the provider console. `/readyz` queue depth reads `workflow_runs`, which nothing in the service populates.

## 3. Owner-gated (cannot be done by the repo alone)

| Item | Why it blocks |
|---|---|
| Anthropic API key and a spend-capped workspace | No real provider call has been made; prices and model ids in `config/models.yaml` are unverified against a live catalog |
| Ollama Cloud plan and terms-of-service review | Data policy for that provider is an assumption (`retention_30d_no_training_unverified`) |
| TypeSafe API key | Jev classifier path cannot run live |
| A real coding repository and issue | The coding workflow and the 8-of-10 benchmark need real tasks |
| Gmail / Calendar OAuth grants | Personal integrations are untested against real accounts |
| 7-day shadow pilot | Gates 9 and 10 need real use |
| Restore drill on the real host | RPO 24 h / RTO 4 h are unmeasured; only a tooling drill on a demo database has run; the real `age` encryption path has not run (age is not installed on the dev machine) |
| Two weeks of use (5 days per week) | Gate 9 |
| Manual claim audit | Claims in docs and commit messages have not been audited by the owner |

## 4. Spec section 2 gates

All ten are **unmeasured**. The detailed table with the missing artifact per gate is in [release-checklist.md](release-checklist.md).

| Gate | Measured? | What is missing |
|---|---|---|
| Provenance (100% accepted memories have a source) | no | No audit query run over real accepted memories; memory is not wired into the service |
| Recall (>= 90% on 50 held-out queries) | no | Held-out memory query set not authored; no answer generation wired |
| Unsupported recall (<= 5%) | no | Same set and rubric |
| Policy (zero unauthorized mutations; 30 policy/injection cases) | no | 0 of 30 injection cases exist; policy not wired into the service |
| Budget (zero overspend in concurrency tests) | no | Concurrency tests exist in the crate, but the budget is not on the live call path, and open findings from reviewer A remain until fixed |
| Coding (>= 8 of 10 benchmark tasks) | no | No benchmark tasks run; workflow runs code on the host (reviewer B C1) until fixed |
| Recovery (10 crash/retry scenarios) | no | Scenario set incomplete; jobs not wired; reviewer A C1 (lease/effect fencing) until fixed |
| Traceability (every call, route, tool run, approval) | no | Approvals and tool runs carry no correlation id; no trace-completeness audit |
| Daily usefulness (5 days/week for 2 weeks) | no | Owner-gated |
| Personal value (>= 3 h/week saved) | no | Owner-gated |

## 5. Conformance against the spec (reviewer D, kept current)

| Spec item | Status | Notes |
|---|---|---|
| Section 2 release gates | MISSING (unmeasured) | Section 4 above |
| Standing approvals | MISSING | Approvals are per-payload only |
| Source-artifact backup | MISSING | Backup covers PostgreSQL only |
| Off-host backup copy | MISSING | `PAIR_BACKUP_DIR` is local; copying off-host is an owner/host task |
| Backup scheduler | MISSING | Nothing runs `scripts/backup` daily |
| RPO 24 h / RTO 4 h proven | MISSING (unmeasured) | Needs the real-host restore drill |
| Export | PARTIAL | Memory export exists as a library; conversations, goals, configuration and billing ledger export do not |
| Retention: payloads 30 days, raw sources 90 days | MISSING | No purge job |
| Separate staging and production credentials/data | MISSING | `deploy/compose.yaml` has no default password now, but no staging/production separation exists |
| Dashboard (usage, reservations, escalations, queue health, ...) | MISSING | |
| Policy/injection evaluation set (30 cases) | MISSING | 0 of 30 |
| Correlation ids on approvals and tool runs | MISSING | Request/trace ids exist only on `pair-api` requests; no `tool_executions` table |
| Alerts firing | MISSING | `config/alerts.yaml` is definitions only; nothing evaluates it (including `backup_stale`) |
| Health checks (readiness, migrations, backlog, provider availability) | IMPLEMENTED | Backlog is always 0 in the running service (no producer); provider availability reads the registry's static health flag, not a live probe |
| Immutable image tags and rollback procedure | PARTIAL | Policy and runbook exist; CI does not build or push an image; base images pinned by tag, digests to be pinned at release |
| Database private, authenticated ingress only | PARTIAL | Compose binds loopback; no reverse proxy is defined |
| Isolated workers | PARTIAL | `deploy/worker/compose.worker.yaml` defines the profile; reviewer B C1 notes the coding runner does not use it yet |
| Kill switch | DOCUMENTED, mostly NOT EFFECTIVE | See [kill-switch](runbooks/kill-switch.md) |
| Library-only components (section 2 above) | NOT WIRED | |

## 6. Deferred

### LOW findings not fixed in this change set (reviewer D, owned here)

- **L12** the spike plugin logs full tool params, and `spike.sh` / `setup-plugin.sh` interpolate values into `sed` unescaped.
- Image base digests are not pinned (tags are; pin digests at release). `cargo install cargo-audit` / `cargo-deny` in the non-blocking supply-chain job are not version-pinned.
- The OpenClaw spike evidence under `adapters/openclaw/spike/evidence/` was captured with the pre-hardening gate and was not re-captured against a live gateway.

### LOW and other findings in crates not covered here

Reviewers A, B and C list LOW items in `budget`, `jobs`, `workflows`, `policy`, `models`, `context`, `memory` and the migrations (see [review-findings.md](review-findings.md)). They are owned by separate fix tracks. This file does not assert their status; where a fix has landed it is in `git log`, and anything not mentioned there should be treated as deferred. Reviewer C LOW examples: normalization collisions, markdown export injection, supersession not scoped by project/kind, retrieval `LIMIT 200` before coverage filter, N+1 in `list_inbox`.

### Spec section 7 tables that do not exist

`projects`, `tool_executions`, `eval_cases`, `eval_results`.

### Missing foreign keys, columns, constraints and indexes (reviewer C)

- `approvals` lacks scope and decision; `audit_events` lacks policy version, outcome and approval reference.
- `goals` / `open_loops` evidence is `jsonb` with no foreign key; parallel source registries `research_sources` / `integration_sources` alongside `sources`.
- Missing FKs: `model_calls.reservation_id`, `workflow_runs.approval_id`, `approvals.consumed_by`.
- Missing CHECKs: superseded/expired imply `valid_to`; `invalidated_reason` implies expired; edited implies `replaced_by`; audit outcome values.
- Missing indexes: `memory_chunks(source_id)`, `memory_candidate_evidence(source_id)`, pending candidates `(kind, project, topic_key)`.
- Migration hazards: `IF NOT EXISTS` in 040 (silent skip); 051 `NOT NULL` without default breaks a rollback to a pre-051 build; some test fixtures load single migrations.

### Declined by the reviewers (recorded, not planned)

IST day-boundary burst policy; `sweep_expired`/`resume` have no production callers yet; jsonb payload round trip; step retries (4) times router attempts (3) could exceed the section 6 limit of 3 attempts once wired; extractor topic quality; external id retention on deleted evidence; bitemporal `as_of`; `SourceFetcher` redirects (not implemented); integrations data-class tagging.
