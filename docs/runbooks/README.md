# PAIR runbooks

Alert thresholds: `config/alerts.yaml`. Health endpoint: `GET /readyz` (needs `Authorization: Bearer $PAIR_SERVICE_TOKEN` and `X-Actor`).

| Symptom / alert | Runbook |
|---|---|
| Provider failing or timing out | [provider-outage](provider-outage.md) |
| `queue_backlog` warn/critical | [queue-backlog](queue-backlog.md) |
| `disk` warn/critical | [full-disk](full-disk.md) |
| `migrations` critical, service will not start | [migration-failure](migration-failure.md) |
| Need a backup, or data loss | [backup-restore](backup-restore.md), [restore-drill](restore-drill.md) |
| Bad release | [rollback](rollback.md) |
| Reservations stuck in `reserved` | [reservation-reconciliation](reservation-reconciliation.md) |
| Runaway spend, suspected injection, compromise | [kill-switch](kill-switch.md) |

Conventions: commands run from the repo root on the host. `DB` below means the PAIR database; the compose Postgres container is `deploy-postgres-1` in the dev stack. Procedures marked **unverified** have not been exercised on the real host.

Readiness levels: `ok`, `warn` (alert, still ready), `critical` (alert, `/readyz` returns 503). Provider outage and a stalled background task (`background` check: lease sweeper, orphan reconciler, jobs worker) are deliberately warn-only; `services` is critical when the budget/policy/routing configuration did not load.
