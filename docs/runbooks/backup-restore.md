# Runbook: backup and restore

Targets (spec §11): **RPO 24 h, RTO 4 h**. These are *targets*. They are proven only by a restore drill on the real host, by the owner. The local drill in [restore-drill](restore-drill.md) proves the tooling, not the targets.

## Backups
- Command: `scripts/backup` (daily from the host scheduler, e.g. cron/systemd timer at 02:30 Asia/Kolkata).
- Format: `pg_dump -Fc`, written to `PAIR_BACKUP_DIR` (default `~/.local/state/pair/backups`, outside the repo) as `pair-<UTC timestamp>.dump.age`.
- Encryption: `age`, recipient public key in `PAIR_BACKUP_AGE_RECIPIENT`. If `age` or the recipient is missing the script **fails loudly**. `PAIR_BACKUP_UNENCRYPTED=1` is an explicit opt-out for local drills only.
- Keep the age *identity* (private key) offline and separate from the backups; a backup you cannot decrypt is not a backup.
- Retention: newest backup of each of the last 7 days, plus the newest of each of 4 older ISO weeks (`scripts/tests/test_retention.sh`).
- Copy `PAIR_BACKUP_DIR` off-host (object storage in a different failure domain). Source artifacts (raw imports) must be added to the same job; the script covers PostgreSQL only. **Unverified:** off-host copy and artifact backup.
- Staleness alert: `backup_stale` is defined in `config/alerts.yaml` (warn above 26 h) but **not wired**: nothing reads `PAIR_BACKUP_DIR` or fires it. Until a job exists, check the age of the newest backup by hand.
- `PAIR_BACKUP_DIR` is created `0700` and backups `0600` regardless of the caller's umask.

## Restore (full loss)
1. Provision Postgres 17 and the same image tags as production ([rollback](rollback.md) lists them).
2. Fetch the newest backup and the age identity.
3. `PAIR_BACKUP_AGE_IDENTITY=/path/to/key.txt scripts/restore pair-<ts>.dump.age --target-db pair_restored`
4. Verify: `scripts/restore-drill`-style row counts, or run the application against `pair_restored` read-only and check `/readyz`.
5. Cut over: point `DATABASE_URL` at the restored DB, or replace the live database in place: `PAIR_RESTORE_CONFIRM_LIVE=<db> PAIR_RESTORE_CONFIRM_DROP=<db> ... --target-db <db> --drop-existing`. The restore goes into `<db>_incoming` and is renamed over `<db>` only after it fully succeeded, so a failed restore leaves the existing database untouched. Still destructive on success: back up the live DB first.
6. Reconcile reservations ([reservation-reconciliation](reservation-reconciliation.md)); up to 24 h of ledger entries may be missing, so compare with provider usage pages.

Safety rails in `scripts/restore`: all prerequisites (age present, identity set and readable, dump readable, `age -d | pg_restore --list` succeeds) are checked before anything is dropped; the restore runs into `<target>_incoming` and is renamed over the target on success only; a non-empty target is refused unless `--drop-existing`; `--drop-existing` always needs `PAIR_RESTORE_CONFIRM_DROP=<target>`; the live DB is identified by `PAIR_LIVE_DB` (default `pair`, independent of `DATABASE_URL`) and needs `PAIR_RESTORE_CONFIRM_LIVE=<db>`. Tested by `scripts/tests/test_restore.sh` (`restore_failure_leaves_target_intact`).

## Deletion and backups
Source deletion invalidates derived memories in the live DB immediately; backups age out through retention (max ~5 weeks). After any restore, re-apply deletions recorded since the backup date.

## Drill cadence
Before first daily dependence, then quarterly, and after any change to backup tooling or schema tooling. Record results in [restore-drill](restore-drill.md).
