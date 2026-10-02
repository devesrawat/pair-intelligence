# Runbook: queue backlog

**Alerts:** `queue_backlog` warn (>= 50 queued or oldest >= 5 min), critical (>= 200 or >= 30 min; `/readyz` = 503). Thresholds live in `config/alerts.yaml` and `service/crates/telemetry/src/health.rs`.

The gauge is `queue_backlog_depth` in the `/readyz` body: count of rows in `jobs` with `state = 'queued'`. Until the jobs migration exists, the check reports depth 0 ("jobs table not present yet").

## Diagnose
1. Depth vs age: a high depth with a young oldest-age is a burst; a modest depth with an old oldest-age is a stuck worker or lease.
2. Workers alive? `docker compose -f deploy/compose.yaml ps` and worker logs (correlate by `job_id`/`trace_id`).
3. State breakdown (read-only):
   `docker exec deploy-postgres-1 psql -U pair -d pair -c "SELECT state, count(*), min(created_at) FROM jobs GROUP BY state"`
4. Provider outage as root cause? See [provider-outage](provider-outage.md). Budget exhausted? Jobs may be blocked on reservations; see [reservation-reconciliation](reservation-reconciliation.md).

## Act
- Stuck lease: workers reclaim expired leases; if none do, restart the worker (jobs resume from checkpoints; do not hand-edit `state`).
- Burst: wait; do not raise concurrency without checking the budget caps.
- Runaway producer (a routine or integration enqueuing in a loop): stop the producer, then cancel the excess jobs through the jobs interface (`cancelled` is a terminal state). If spend is also climbing use [kill-switch](kill-switch.md).

## Verify
`/readyz` queue_backlog returns to ok and oldest-age falls.
