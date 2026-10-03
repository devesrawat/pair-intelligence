# PAIR status

Honest, tracked snapshot of what exists, what is tested, and what is not. Written for branch `feat/phase-0-audit` after the four-reviewer pass in [review-findings.md](review-findings.md). It replaces ad hoc claims in the working ledger (which is git-ignored and therefore not reproducible). If this file and a runbook or commit message disagree, this file was written last; if it disagrees with the code, the code wins and this file is a bug.

"Tested" means the automated suite passes (Rust: 455 tests across 10 crates, 2 live-network tests ignored; 7 shell script suites; the full migration chain 001-080 applies in order, checked by `crates/api/tests/migration_chain.rs`). Per spec section 2, passing tests do not establish semantic correctness or security, and **none of the section 2 release gates has been measured** (see below).

## 1. Implemented and tested (automated)

| Area | What exists | Where it is tested |
|---|---|---|
| `pair-api` HTTP surface | `/healthz` (open), `/readyz`, `/v1/whoami`, plus the hosted `/v1` endpoints of section 2; bearer service token + `X-Actor` (`[A-Za-z0-9._:@-]{1,64}`); single `X-Trace-Id` header requiring a canonical UUID (otherwise regenerated); request timeout (30 s) and in-flight cap (64) returning 504 / 503; SIGTERM and Ctrl-C graceful shutdown; config secrets wrapped so `Debug` never prints them | `crates/api/tests/{http,hardening,limits}.rs`, unit tests in `crates/api/src/{auth,config,shutdown}.rs`, `scripts/check smoke` (boots the real binary against a scratch database) |
| Readiness strictness | Explicitly configured `PAIR_MODELS_CONFIG` / `PAIR_MIGRATIONS_DIR` that do not exist fail startup; zero migrations on disk or applied is critical in the service binary; empty provider registry is a warning; error details carry no paths or OS errors | `startup_fails_when_configured_models_config_missing`, `readyz_fails_when_no_migrations_applied`, `readyz_warns_on_stub_providers`, `readyz_error_details_do_not_leak_paths_or_os_errors` |
| Telemetry | Trace-id codec (`x-trace-id`, UUID only), structured tracing setup, secret redaction, disk and backlog health evaluation | `crates/telemetry` unit tests |
| Container image | `deploy/api/Dockerfile` bakes migrations and `config/models.yaml`, pinned base tags, `.dockerignore`. Built and booted once by hand on the dev machine (read-only root filesystem, all capabilities dropped, `docker stop` took 1 s via SIGTERM). Not built in CI | manual, 2026-10-03 |
| Compose | `deploy/compose.yaml` refuses to render without `POSTGRES_PASSWORD`; pair-api has `read_only`, `cap_drop: [ALL]`, `no-new-privileges`, tmpfs; dev-only database in `deploy/compose.dev.yaml` | `scripts/tests/test_compose.sh` |
| Backup / restore tooling | Encrypted (age) dump, 7 daily + 4 weekly retention with exact-name matching and loud failure; restore verifies everything first, restores into `<db>_incoming`, renames on success; live-db guard on `PAIR_LIVE_DB`; `--drop-existing` always confirmed; private file modes; docker mode keeps the password out of argv and refuses non-loopback hosts | `scripts/tests/test_{restore,retention,retention_safety,backup_perms,pgtools}.sh`; `scripts/restore-drill --seed-demo` (CI) |
| OpenClaw adapter (`pair-spike`) | `before_tool_call` maps tools through an explicit table (unmapped denied, shell syntax denied) to `POST /v1/policy/authorize`; local deny-list kept and wins; every failure blocks. `before_model_resolve` reserves via `/v1/budget/reserve`, `before_agent_run` (the only fail-closed run gate; `before_model_resolve` cannot abort) blocks runs without a reservation, `llm_output` reconciles (unknown usage stays unresolved). Env-only config, loopback-only URL, no params or tokens in logs | `scripts/tests/test_openclaw_gate.sh` (107 unit tests against a loopback mock pair-api, plus upstream `tsc` type-check when `OPENCLAW_DIR` is set); live: `adapters/openclaw/spike/wired.sh` (real pair-api binary + real gateway v2026.9.7, 43 checks, evidence in `adapters/openclaw/spike/evidence/wired/`), see [spike doc](spike-openclaw-plugin.md) |
| Library crates | `core`, `policy`, `budget`, `models`, `memory`, `context`, `jobs`, `workflows` have their own suites | `cargo test --workspace`, `scripts/check <suite>` |

CI (`.github/workflows/ci.yml`) runs fmt, clippy `-D warnings`, the workspace tests, the smoke check, every `scripts/tests/*.sh`, and a seeded restore drill, against the dev compose database. GitHub Actions are pinned to commit SHAs.

## 2. What the running `pair-api` hosts, and what it does not

`pair-api` now hosts the budget, policy, routing, context, conversation and (partly) jobs crates. Each row names the test that proves it against real Postgres, a real `PgBudget`, the real `config/policy.yaml` and a loopback mock provider (`crates/api/tests/`).

| Endpoint / task | What is enforced | Proven by |
|---|---|---|
| `POST /v1/turn` | Employer, sensitive and unknown data classes refused before any I/O; NUL and other control characters (except newline and tab) in `message` and `project_type` are a 422; `kind` must be `default` unless `PAIR_TURN_ALLOW_KIND_OVERRIDE=1`; a conversation stores the highest data class any of its turns declared (migration 110, existing rows `personal`), a turn below it is `policy_denied` before any write and routing, the classifier summary and the provider call run under that highest class; the turn is claimed before paid work (migration 112: two identical concurrent turns cost one provider call, the loser gets 409; a replay with different content is 409; a replay of a finished turn returns the stored answer with `replayed: true`); Jev classification in shadow mode (failure or timeout falls back to the baseline, never an error); router plan within the task kind's own cap; reserve with explicit kind and price version; context compile over a LIMIT-ed window of the last 12 messages; provider call through `ModelCaller`; reconcile (unknown cost stays unresolved); message and `model_calls` rows with a cost state; unverified model ids refused up front without escalation; internal errors answer a generic `internal error` body (detail logged with the trace id). The turn runs detached from its HTTP request but keeps the request's in-flight permit and runs on a tracker the shutdown drain waits for; a turn dropped between reserve and reconcile settles its reservation as unresolved, never `held` | `turn.rs`: `budget_denied_request_makes_zero_provider_hits`, `turn_persists_and_survives_pool_reopen_with_reconciled_cost`, `employer_data_refused_before_any_network_call`, `jev_failure_falls_back_to_baseline_and_still_answers`, `unverified_model_id_stops_the_call_and_does_not_escalate`, `provider_failure_leaves_reservation_unresolved`, `turn_requires_service_token`; `turn_class.rs`: `new_conversation_records_declared_class`, `turn_cannot_lower_conversation_data_class`, `history_sent_under_highest_class_in_conversation`; `turn_replay.rs`: `concurrent_identical_turns_pay_once`, `replay_with_changed_content_is_refused`, `replay_of_finished_turn_returns_stored_answer`; `turn_input.rs`: `nul_in_message_is_422_not_500`, `internal_errors_do_not_leak_database_text`, `turn_kind_other_than_default_is_refused`; `turn_lifecycle.rs`: `disconnect_mid_turn_does_not_exceed_in_flight_limit`, `shutdown_waits_for_detached_turns`, `cancelled_turn_leaves_unresolved_reservation_and_a_retryable_claim`; `store_reads.rs` |
| Persisted model-attempt cap (spec 6: 3 per task) | Migration 080 counter consumed atomically before every metered reservation of a turn. Retries share one counter and one task budget only when the request carries BOTH `conversation_id` and `client_message_id` (the task id is derived from them; any other turn gets a fresh random task id and a fresh counter) | `attempts.rs`: `attempt_cap_is_persisted_across_step_retries`, `fourth_attempt_for_same_task_refused_even_after_restart`, `concurrent_consumers_cannot_exceed_the_attempt_cap` |
| `POST /v1/policy/authorize` | Decides only (allow, deny, needs approval with payload hash); never executes, never consumes an approval; workspace root comes from server config; stale policy version denied | `policy_api.rs`: `deny_returns_deny_and_nothing_is_executed`, `needs_approval_returns_payload_hash`, `stale_policy_version_denied` |
| `POST /v1/approvals` | Hash-bound, at most 24 h, created through `PgApprovals` and only with a second credential (`PAIR_APPROVER_TOKEN`), so the holder of the service token cannot mint its own approvals | `policy_api.rs`: `approvals_need_the_separate_approver_credential` |
| `POST /v1/budget/reserve`, `/reconcile` | Explicit task kind, category and price version; `task_id` required on both; reserve takes `model_id` and holds `max(client figure, registry worst case)` (the client can only raise the hold; unknown model or price is `budget_unknown_price`); reconcile counts `max(reported, tokens x registry price)`, refuses anything above reserved x `PAIR_BUDGET_OVERRUN_FACTOR` (4) or the hard ceiling (`MAX_COST_MICROS`) and refuses a different `task_id`; over-cap reserves nothing; unknown cost is unresolved. Limit: token counts are adapter-reported, so the server can bound a settlement from above and floor it against the reported tokens, not verify them (migration 111 stores the reserved model) | `budget_api.rs`: `adapter_reserve_then_reconcile_roundtrip`, `reserve_over_cap_returns_budget_exceeded_and_reserves_nothing`, `reconcile_unknown_cost_is_unresolved`; `budget_safety.rs`: `reserve_hold_is_at_least_registry_worst_case`, `reconcile_cannot_underreport_cost`, `reconcile_rejects_i64_max_actual_cost_and_budget_still_works`, `reserve_requires_task_id`, `reconcile_with_wrong_task_is_refused`, `reserve_unknown_model_refused` |
| Background tasks | Lease sweeper (marks expired leases interrupted and requeues them, failing a run after 5 interruptions; a run an operator interrupted on purpose is left alone, migration 113) and orphaned-intent reconciler run inside the binary, stop on SIGTERM/Ctrl-C with a bounded drain, and `/readyz` reports their liveness (`background`: a stalled task is a warning) | `background.rs`: `shutdown_drains_background_tasks`, `drain_is_bounded_and_aborts_a_stuck_task`, `sweeper_requeues_expired_lease_in_running_service`, `readyz_reports_background_liveness_and_warns_on_a_stalled_task`; jobs `requeue.rs`: `requeue_fails_a_run_that_keeps_being_interrupted`, `operator_interrupt_is_not_requeued_by_the_sweeper` |
| Kill switch levels 1 and 3 | Zeroing every cap refuses new turns and reserves; rotating the service token locks out the old one | `kill_switch.rs` (level 2 has no mechanism and stays NOT EFFECTIVE) |

### Still NOT wired

- **The OpenClaw adapter is wired for the CLI/gateway path, with limits** (see the adapter row and [spike doc](spike-openclaw-plugin.md#not-proven)): tool calls are authorized by the real policy engine and model runs are reserved, gated and reconciled, proven live with a mock model and `allowConversationAccess=true`. NOT proven: the approval round trip (no approval-capable surface; an OpenClaw allow-once is not a PAIR approval), sandbox mode `all`, any real provider or real token counts. With `allowConversationAccess=false` the model-call gate silently disappears (only a gateway WARN line); follow-up model calls inside a run share the run's one estimated reservation; the local allow-list still denies `write`/`edit`. The policy is the shipped `config/policy.yaml`, not yet reviewed against OpenClaw's real tool surface. Gates 4, 5 and 8 remain unmeasured for the OpenClaw path.
- `workflows` (coding, research, daily): nothing starts them. No step handler is registered, so no jobs worker loop runs (a handler-less worker would claim and fail every queued run, because `JobStore::claim` is not filtered by kind). `Gate::execute` has no caller in the service: there is no tool-execution path in `pair-api` for the gate to guard, and the approval that `/v1/approvals` creates is consumed only by a Gate path that does not exist here yet.
- `memory` (store, inbox, retrieval, export): turns compile context with no retrieved memories; no memory endpoint exists.
- Orphaned effect intents: the reconciler returns an explicit "undecided" error for every intent (it never guesses), so they stay unresolved and are logged on every sweep until a real reconciler exists.
- The classifier is shadow mode only and runs when `TYPESAFE_API_KEY` is set. No real provider call has been made (section 3), so none of this has run against a live provider.

### Deviations and limits to know

- Order: the spec lists reserve before context compile; the reservation is derived from the compiled request, so a turn compiles first and then reserves. A refused turn still never reaches the provider.
- A failed provider attempt keeps its full worst-case reservation unresolved and counted, so it can use up the task cap and stop a fallback (the response then reports the provider failure, not the budget). Attempts are taken before the reservation because the ledger has no release.
- The turn budget (`PAIR_TURN_BUDGET_SECS`, default 25, at most 28) is a deadline for the whole turn: classifier and database time use it up, and the provider attempts get what is left. The HTTP request timeout is 30 s, but a turn that outlives it is not cut off (it runs detached); the client sees a 504 and a retry with the same `client_message_id` returns the stored answer.
- The attempt counter covers the turn pipeline (and any `ModelCaller` given the counting budget). Adapter reserve calls are not counted: an agent loop makes many legitimate calls per run.
- The classifier reserves under its own derived task id: `ReserveRequest::classifier` registers its task as `default`, which would make a later `coding` or `research` reservation of the same task a conflict.
- `/readyz`: `services` is critical when the budget, policy and routing configuration did not load; a stalled background task is a warning. `/readyz` queue depth reads `workflow_runs`, which nothing in the service populates yet.

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
| Policy (zero unauthorized mutations; 30 policy/injection cases) | no | 0 of 30 injection cases exist; `/v1/policy/authorize` is hosted, but nothing in `pair-api` executes tools and the OpenClaw adapter does not call it (section 2) |
| Budget (zero overspend in concurrency tests) | no | Concurrency tests exist and reviewer A's findings are fixed; the budget is now on the `pair-api` turn and adapter-reserve path, but not on OpenClaw's own provider calls (section 2) |
| Coding (>= 8 of 10 benchmark tasks) | no | No benchmark tasks run. The runner now defaults to a container sandbox, but that path has only been tested with a recording executor, never against a docker daemon and the worker image |
| Recovery (10 crash/retry scenarios) | no | Scenario set incomplete; the lease sweeper and orphan reconciler now run in the service, but no workflow handler does. Lease and effect fencing (reviewer A C1) is fixed and tested with a live first worker |
| Traceability (every call, route, tool run, approval) | no | Turns carry one trace id through messages, model calls and audit events; approvals and tool runs still carry none; OpenClaw's own calls never reach these tables; no trace-completeness audit |
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
| Isolated workers | PARTIAL | The coding runner defaults to `ContainerSandbox` (no network, read-only root, one worktree mount, `.git` of the original repo never mounted) and refuses host execution unless `PAIR_ALLOW_HOST_EXEC=1`. Tested with a recording executor only; no docker daemon was exercised, and approved pushes cannot run through it (no network or credentials in the container) |
| Kill switch | PARTIAL | Levels 1 (zero caps) and 3 (rotate token) are proven for `pair-api` calls and, in the live harness, for the OpenClaw adapter path (zero caps: reserve refused, mock model saw 0 requests; rotated token: adapter tool calls and reserves get 401 and are blocked); level 2 (interrupt jobs) is NOT EFFECTIVE; the adapter must be configured with `allowConversationAccess=true` for the budget gate to exist at all. See [kill-switch](runbooks/kill-switch.md) |
| Components still not hosted (section 2 above) | NOT WIRED | workflows, memory |

## 6. Deferred

### LOW findings not fixed in this change set (reviewer D, owned here)

- **L12** the spike plugin logs full tool params, and `spike.sh` / `setup-plugin.sh` interpolate values into `sed` unescaped.
- Image base digests are not pinned (tags are; pin digests at release). `cargo install cargo-audit` / `cargo-deny` in the non-blocking supply-chain job are not version-pinned.
- The older OpenClaw spike evidence under `adapters/openclaw/spike/evidence/{final,noconv,errprobe}` predates the hardening and wiring; the current live evidence is `evidence/wired/`.

### Review findings: resolution (reviewers A, B, C; D is above)

All findings graded Critical or High were fixed test-first except where noted; Medium findings were fixed except where listed below. Fix commits are in `git log`; the findings themselves are in [review-findings.md](review-findings.md).

**Fixed.** A: C1 lease/effect fencing (heartbeat, epoch fencing, `executing` CAS, tests keep the first worker alive), H1 re-entrant approval expiry, H2 approval bound to the executed payload and to one effect, H3/B-H1 failed provider calls no longer booked at zero, H4 deadline counts active time only, M1 reconcile lock plus overrun flag, M2 task kind persisted per task, M4 orphaned intents swept, M5 tool-call counter under the lease guard. B: C1 sandbox abstraction (see section 5), H2 executables that run code are denied on the host and argument fragments are path/egress checked (including scp-style remotes, clustered flags, scheme and port), H3 Jev client guarded and endpoint validated at load, H4 data class required on repos and research scopes and the employer/sensitive provider rows removed, M1-M7. C: H1 trust only from verified spans within an allowed source set, H2 concurrent contradictory accepts serialized, M1-M7, new constraints and indexes (migrations 070-072).

**Fixed after merging, found by integration tests, not by the reviewers:** the hardened policy read a push refspec (`<sha>:refs/heads/x`) as an scp host and denied every push; routing candidates were placeholders that no priced registry model matched; the router was given the $0.10 default task cap instead of the coding ($1.00) and research ($0.50) caps, so no model was affordable at a realistic context. Jobs tests were load-sensitive (real-time sleeps against a 600 ms lease) and now expire leases deterministically.

**Known gaps in the fixes (not hidden):**

- The container sandbox is untested against docker. Approved `git push` cannot run through it.
- `claude-sonnet-5-5` and `claude-haiku-4-5` stay marked `id_verified: false` in `config/models.yaml`, so `generate` refuses them unless `PAIR_ALLOW_UNVERIFIED_MODEL_IDS=1`, and routing now points at them. Run the ignored `live_anthropic_model_ids_exist` test once an API key exists. Ollama models are not routed until their prices are verified.
- Research is blocked by default: `config/policy.yaml` has no research or search hosts allow-listed. Repositories and research scopes without a `data_class` are refused.
- Budget M3: the classifier eval tool calls the classifier without reserving budget (do not point it at the real Jev). `overrun` is exposed through `reconcile_detailed`, not on `LedgerEntry`. Plain `Budget::reserve` uses the task's registered kind or Default.
- Policy: path checks are time-of-check (a symlink swapped before execution is not caught; the read-only single-volume worker is the second layer). `sed`, `git` and other tools are validated by rule, not proven safe. Standing approvals are not implemented.
- Memory: a manual accept of a preference or permission now needs verified owner evidence, so callers must supply source text. Stored normalized/dedupe keys use a new format and will not match rows written by earlier builds. Unassigned LOWs: untrusted text labelled `kind: fact` still passes `check_accept` (prompt-level only), `list_inbox` is N+1, export is unbounded.
- Jobs: the sweeper and orphan reconciler now run in `pair-api`; `resume` has no caller. Step retries (4) times router attempts (3) can no longer exceed the section 6 limit where the persisted counter is used (the turn pipeline); the workflows' own `ModelCaller` is not given it because the workflows are not hosted.
- `SourceFetcher` and the research `SupportJudge` have no real implementation; the lexical support check is disclosed as lexical in every report.

### Spec section 7 tables that do not exist

`projects`, `tool_executions`, `eval_cases`, `eval_results`.

### Missing foreign keys, columns, constraints and indexes (reviewer C)

- `approvals` lacks scope and decision; `audit_events` lacks policy version, outcome and approval reference.
- `goals` / `open_loops` evidence is `jsonb` with no foreign key; parallel source registries `research_sources` / `integration_sources` alongside `sources`.
- Missing FKs: `model_calls.reservation_id`, `workflow_runs.approval_id`, `approvals.consumed_by`.
- Migration hazards: `IF NOT EXISTS` in 040 (silent skip); 051 `NOT NULL` without default breaks a rollback to a pre-051 build; some test fixtures load single migrations (the full chain is covered by `migration_chain`). The CHECK constraints in 071 are added `NOT VALID` then validated, and are left `NOT VALID` with a warning if legacy rows violate them.

### Declined by the reviewers (recorded, not planned)

IST day-boundary burst policy; `resume` has no production caller; jsonb payload round trip; extractor topic quality; external id retention on deleted evidence; bitemporal `as_of`; `SourceFetcher` redirects (not implemented); integrations data-class tagging.
