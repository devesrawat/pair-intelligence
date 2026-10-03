# Release checklist

Source of the gates: spec §2 ("proposed release targets, not current performance claims"). A gate is `passed` only when its measurement artifact exists and is linked in the Evidence column. Unit tests passing alone do not establish semantic correctness or security. Every status below is `unmeasured` as of this commit; this file does not claim any gate has passed.

Spec §16 Task 16 exit: gates pass **or** limitations are explicitly documented and accepted by the owner. Record every unmet target below.

## 1. World-class gates (spec §2)

| # | Gate | Release target | Measured by | Status | Evidence |
|---|---|---|---|---|---|
| 1 | Provenance | 100% of accepted memories have accessible source references | memory suite + audit query over accepted memories | unmeasured | |
| 2 | Recall | >= 90% correct on 50 held-out decision/fact queries, with source support | `evals/` held-out memory queries, human-anchored | unmeasured | |
| 3 | Unsupported recall | <= 5% unsupported factual answers; unknowns acknowledged | same held-out set, unsupported-claim rubric | unmeasured | |
| 4 | Policy | Zero unauthorized external mutations in the policy test suite (30 policy/injection cases) | `scripts/check policy` + injection review | unmeasured | |
| 5 | Budget | Zero overspend beyond the reservation ceiling in concurrency tests | `scripts/check budget` concurrency tests | unmeasured | |
| 6 | Coding | >= 8 of 10 scoped benchmark tasks pass acceptance checks and human review | `scripts/check coding` + review | unmeasured | |
| 7 | Recovery | Interrupted jobs resume or reach a clear terminal state with no duplicated side effects (10 crash/retry scenarios) | `scripts/check recovery` | unmeasured | |
| 8 | Traceability | Every model call, routing decision, tool execution and approval is traceable | trace-completeness audit over a usage sample | unmeasured | |
| 9 | Daily usefulness | Used >= 5 days/week for two consecutive weeks | usage log | unmeasured | |
| 10 | Personal value | Owner records >= 3 hours saved weekly after stabilization | owner log | unmeasured | |

## 2. Operations preconditions (spec §11, §16 Task 16)

These are release prerequisites, not §2 gates. "Built" means code or docs exist in the repo and were exercised locally; none has been exercised on the real host.

| Item | Status | Evidence / limitation |
|---|---|---|
| Health checks: readiness, migrations, queue backlog, provider availability | built, locally tested | `pair-api` tests (`crates/api/tests/http.rs`, `hardening.rs`: `readyz_fails_when_db_down`, `readyz_reports_migration_failure_and_pending`, `readyz_fails_when_no_migrations_applied`, `readyz_warns_on_stub_providers`, `readyz_provider_outage_degrades_but_stays_ready`). Providers come from `ProviderRegistry` (`config/models.yaml`, baked into the image); an empty registry is a Warn. Backlog reads `workflow_runs(state, created_at)` from `migrations/030_jobs.sql`; nothing in `pair-api` enqueues runs yet, so the depth is always 0 in the running service |
| Boot smoke check (`scripts/check smoke`) | built, passes locally | dev-machine run; not run on the target host |
| Operational checks as tests (disk, backlog threshold, migration failure) | built | `pair-telemetry` health tests (`crates/telemetry/src/health.rs`); `readyz_fails_when_disk_critical`, `readyz_queue_backlog_reads_workflow_runs`, `readyz_queue_backlog_age_thresholds`, `readyz_reports_migration_failure_and_pending` |
| Alerts defined | thresholds only, **not wired** | `config/alerts.yaml`; no scheduler or notifier exists, so no alert (including `backup_stale`) fires; owner supplies channel |
| Daily encrypted backup, 7 daily + 4 weekly retention | built; retention, restore safety, backup permissions tested; real `age` encryption path not exercised (`age` absent on dev machine; the restore tests use an `age` shim) | `scripts/backup`, `scripts/tests/test_retention.sh`, `test_retention_safety.sh`, `test_backup_perms.sh`, `test_restore.sh`; off-host copy and source-artifact backup not built; no scheduler runs the backup |
| Restore drill performed | tooling drill only; production drill unmeasured | [restore-drill](runbooks/restore-drill.md) |
| RPO 24 h / RTO 4 h | unmeasured | owner-verified on the real host only |
| Rollback procedure | documented, unverified | [rollback](runbooks/rollback.md); the image was built and booted once by hand on the dev machine (read-only rootfs, SIGTERM stop), not in CI |
| Runbooks: provider outage, queue backlog, full disk, migration failure, backup/restore, reservation reconciliation, kill switch | documented, unverified on real host; the kill switch lists which levels are NOT EFFECTIVE until `pair-api` hosts budget/jobs | [runbooks](runbooks/README.md) |
| Export (conversations, memories with evidence, goals, configuration, billing ledger) and deletion | not built | depends on memory and budget crates; owner of those tasks |
| Staging and production credentials/data separated | unmeasured | host provisioning not done |
| Immutable image tags | policy documented | `PAIR_IMAGE_TAG`; CI does not yet build or push the image |
| Policy and injection review before daily dependence | unmeasured | Task 16 requirement |
| Two-week stabilization; every material correction becomes an evaluation case | unmeasured | Task 16 requirement |

## 3. Automated checks to run before sign-off

`scripts/check release` (fmt, clippy -D warnings, workspace tests, shell syntax, every `scripts/tests/*.sh`, the boot smoke check, and a restore drill on a seeded scratch database). Passing it means the automated checks pass. It says nothing about the gates in section 1; the real-host restore drill, `age` encryption path and off-host copy stay unmeasured. Current state of everything: [STATUS](STATUS.md).

## 4. Known limitations to accept or fix (fill in at release)

| Limitation | Owner decision |
|---|---|
| | |
