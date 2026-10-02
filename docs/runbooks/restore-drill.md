# Restore drill record

`scripts/restore-drill` backs up a source database, restores it into a scratch database named `pair_drill_<UTC timestamp>`, compares **exact** `count(*)` of every user table between source and restored copy, drops the scratch database, and exits non-zero on any mismatch (or if the source has no tables to verify).

Procedure and rollback context: [backup-restore](backup-restore.md).

## What this proves, and what it does not

| Claim | Status |
|---|---|
| Backup and restore tooling works end to end (dump, restore, row-count compare, cleanup) | Proven locally, entry 1 below |
| Encrypted (age) path works | **Not exercised**: `age` is not installed on the dev machine. The script's refusal to write an unencrypted backup without `PAIR_BACKUP_UNENCRYPTED=1` was exercised. |
| RPO 24 h | **Owner-verified on the real host only.** Depends on the production schedule and off-host copy, neither of which exists yet. |
| RTO 4 h | **Owner-verified on the real host only.** Timing below is a 4-table, ~7k-row demo database and says nothing about production size. |

## Entry 1: local tooling drill (not a production drill)

- Date: 2026-10-02 (UTC 20:32), dev machine, Postgres 17 container `deploy-postgres-1` via `docker exec` (no local pg client tools).
- Command: `PAIR_BACKUP_UNENCRYPTED=1 scripts/restore-drill --seed-demo`
- Source: throwaway database seeded by `--seed-demo` (the shared `pair` database had no tables). Dropped by the script afterwards; no `pair_drill_*` databases remained.

```
drill: source=pair_drill_src_20261002203248 scratch=pair_drill_20261002203248
backup: WARNING writing UNENCRYPTED backup (PAIR_BACKUP_UNENCRYPTED=1)
backup: wrote $TMP/pair-20261002T203248Z.dump (170838 bytes)
retention: pruned 0 file(s) in $TMP
restore: restored $TMP/pair-20261002T203248Z.dump into 'pair_drill_20261002203248' in 1s
drill: row counts (table|rows)
--- source
public.empty_table|0
public.jobs|321
public.ledger_entries|5678
public.memories|1234
--- restored
public.empty_table|0
public.jobs|321
public.ledger_entries|5678
public.memories|1234
drill: PASS (4 tables identical) backup=1s backup+restore=2s
```

Result: tooling PASS. Gate "restore drill performed" in the [release checklist](../release-checklist.md) stays **unmeasured** until the owner runs it on the real host with encryption on and records RPO/RTO there.

## Owner drill template (real host)

```
date / operator:
host + Postgres version:
source DB size (GiB) / table count:
encrypted (age) backup used: yes/no
backup age at drill start (RPO evidence, hours):
time: backup / decrypt+restore / verify / total (RTO evidence):
row-count result:
application boot against restored DB (/readyz):
findings and fixes:
```
