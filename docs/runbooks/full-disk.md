# Runbook: full disk

**Alerts:** `disk_free` warn (< 15% free), critical (< 5% free or < 2 GiB; `/readyz` = 503). The check measures the filesystem holding `PAIR_DATA_DIR`. If the Postgres volume is on a different filesystem, monitor it separately (`df -h` on the host).

A full Postgres volume stops writes: budget reservations fail closed (no spend) but jobs and memory writes error.

## Diagnose
1. `df -h` on the host; `docker system df`.
2. Biggest consumers: Postgres data (`docker exec deploy-postgres-1 du -sh /var/lib/postgresql/data`), `PAIR_BACKUP_DIR` (default `~/.local/state/pair/backups`), container logs, build cache, worker workspaces.

## Free space (safest first)
1. `docker builder prune` and `docker image prune` (unused images only; keep the current and previous immutable tags needed for [rollback](rollback.md)).
2. Old backups beyond retention: `scripts/backup --prune-only` (keeps 7 daily + 4 weekly).
3. Rotate or truncate container logs; confirm `logging` limits (`max-size`, `max-file`) in compose for any new service.
4. Retention purge (spec §11): detailed model/tool payloads older than 30 days, unpinned raw imports older than 90 days. Run the retention job; never delete decision records or the billing ledger.
5. If Postgres bloat: `VACUUM (VERBOSE)` on the largest tables; `VACUUM FULL` needs free space equal to the table, so only after step 1-4.

## Last resort
Grow the volume (host provider console), then confirm Postgres recovered: `/readyz` database ok. If Postgres crashed on ENOSPC and will not start, free space first, then start it and check WAL replay in its log before accepting traffic.

## Verify
`/readyz` disk check ok (>= 15% free). Add capacity if free space does not stay above 15% for a week.
