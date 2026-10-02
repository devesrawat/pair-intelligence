# Runbook: migration failure

**Alert:** `migrations` critical; `/readyz` = 503 with detail `migration <version> failed (dirty)` or `pending migrations: [...]`. `pair-api` also exits at boot if a migration errors (fail fast), so a crash-looping container with a migration error in its log is the same incident.

How the check works: versions present in `PAIR_MIGRATIONS_DIR` (default `migrations/`) are compared with `_sqlx_migrations`. A row with `success = false` means a migration started and did not finish. Test: `readyz_reports_migration_failure_and_pending`.

## Do first
1. Stop the rollout. Do not retry in a loop; the failed row blocks further runs.
2. Take a backup **before touching anything**: `scripts/backup` (or confirm the pre-deploy backup from [release-checklist](../release-checklist.md)).

## Diagnose
`docker logs <pair-api container> 2>&1 | grep -i migrat` for the SQL error. Read the migration file for the failed version.
`docker exec deploy-postgres-1 psql -U pair -d pair -c "SELECT version, description, success, installed_on FROM _sqlx_migrations ORDER BY version DESC LIMIT 5"`

## Recover
Postgres DDL is transactional, so a failed migration normally rolled back fully and only left the dirty marker.
1. Confirm no partial change landed (schema matches the previous version).
2. Remove the dirty row: `DELETE FROM _sqlx_migrations WHERE version = <v> AND success = false;`
3. Fix forward: correct the migration in a new commit (never edit an already-applied migration; sqlx checksums will reject it), or
4. Roll back the release per [rollback](rollback.md) (previous image tag; the schema is still at the prior version).
5. If a migration was non-transactional (`CREATE INDEX CONCURRENTLY`, etc.) and left objects behind, drop them by hand before re-running, or restore from backup ([backup-restore](backup-restore.md)) into a scratch DB and compare.

## Verify
`/readyz` migrations check ok; `SELECT count(*) FROM _sqlx_migrations WHERE NOT success` = 0.

## Prevention
Rehearse every migration against a restored copy of production data (restore drill DB) before release; destructive migrations need a separate expand/contract release.
