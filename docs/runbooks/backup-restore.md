# Runbook: backup and restore

Targets (spec §11): **RPO 24 h, RTO 4 h**. These are *targets*. They are proven only by a restore drill on the real host, by the owner. The local drill in [restore-drill](restore-drill.md) proves the tooling, not the targets.

## Backups
- Command: `scripts/backup` (daily from the host scheduler, e.g. cron/systemd timer at 02:30 Asia/Kolkata).
- Format: `pg_dump -Fc`, written to `PAIR_BACKUP_DIR` (default `~/.local/state/pair/backups`, outside the repo) as `pair-<UTC timestamp>.dump.age`.
- Encryption: `age`, recipient public key in `PAIR_BACKUP_AGE_RECIPIENT`. If `age` or the recipient is missing the script **fails loudly**. `PAIR_BACKUP_UNENCRYPTED=1` is an explicit opt-out for local drills only.
- Keep the age *identity* (private key) offline and separate from the backups; a backup you cannot decrypt is not a backup.
- Retention: newest backup of each of the last 7 days, plus the newest of each of 4 older ISO weeks (`scripts/tests/test_retention.sh`).
- Copy `PAIR_BACKUP_DIR` off-host (object storage in a different failure domain). Source artifacts (raw imports) must be added to the same job; the script covers PostgreSQL only. **Unverified:** off-host copy and artifact backup.
- Alert `backup_stale` fires if the newest backup is older than 26 h.

## Restore (full loss)
1. Provision Postgres 17 and the same image tags as production ([rollback](rollback.md) lists them).
2. Fetch the newest backup and the age identity.
3. `PAIR_BACKUP_AGE_IDENTITY=/path/to/key.txt scripts/restore pair-<ts>.dump.age --target-db pair_restored`
4. Verify: `scripts/restore-drill`-style row counts, or run the application against `pair_restored` read-only and check `/readyz`.
5. Cut over: point `DATABASE_URL` at the restored DB, or restore over the live name with `PAIR_RESTORE_CONFIRM_LIVE=<db> ... --target-db <db> --drop-existing` (destructive: only after the live DB is gone or has been backed up).
6. Reconcile reservations ([reservation-reconciliation](reservation-reconciliation.md)); up to 24 h of ledger entries may be missing, so compare with provider usage pages.

Safety rails in `scripts/restore`: refuses a non-empty target unless `--drop-existing`; refuses the live DB name unless `PAIR_RESTORE_CONFIRM_LIVE` equals it.

## Deletion and backups
Source deletion invalidates derived memories in the live DB immediately; backups age out through retention (max ~5 weeks). After any restore, re-apply deletions recorded since the backup date.

## Drill cadence
Before first daily dependence, then quarterly, and after any change to backup tooling or schema tooling. Record results in [restore-drill](restore-drill.md).
