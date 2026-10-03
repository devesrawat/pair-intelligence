# Migration notes

What the migrations actually do to locks and data, and the rules new ones follow. Written to
correct the claims in the header of `migrations/101_foreign_keys_and_audit_columns.sql`, which is
not edited (applied migrations are immutable; sqlx checksums them).

## How sqlx applies a migration

`sqlx migrate run` (and `Migrator::run`) wraps **each migration file in one transaction**. Every lock
a statement takes is held until the end of the file. A `NOT VALID` constraint followed by
`VALIDATE CONSTRAINT` in the *same file* therefore does not shorten any lock: the stronger lock from
`ADD CONSTRAINT` is still held while `VALIDATE` scans.

## What migration 101 really did

| statement | lock held until the file commits | effect |
|---|---|---|
| `ADD CONSTRAINT ... FOREIGN KEY ... NOT VALID` (model_calls, workflow_runs, approvals) | `SHARE ROW EXCLUSIVE` on the table and the referenced table (blocks writes) | new writes checked at once |
| `VALIDATE CONSTRAINT` (same file) | scan under the lock above | validated in line; a violation was caught and left `NOT VALID` with a WARNING |
| `ADD CONSTRAINT approvals_decision_check` and `audit_events_outcome_check` (plain, no `NOT VALID`) | `ACCESS EXCLUSIVE` on `approvals` / `audit_events` for the full scan | validated in line |
| `ADD COLUMN ... DEFAULT 'granted' NOT NULL`, nullable `ADD COLUMN` | brief `ACCESS EXCLUSIVE` | metadata only (PostgreSQL 11+), no rewrite |
| `CREATE INDEX IF NOT EXISTS` (not `CONCURRENTLY`) | `SHARE` on the table (blocks writes) | index built under the lock |

"Re-runnable" in the 101 header is also wrong for the two plain `ADD CONSTRAINT` statements: a second
run fails because the constraint already exists. It never happens in practice because sqlx records
applied migrations.

**Nothing to redo.** Every CHECK that 101 added was validated in line, so there is no unvalidated
constraint to follow up on. A foreign key that hit a legacy violation was left `NOT VALID`; find any
with:

```sql
SELECT conrelid::regclass AS table_name, conname FROM pg_constraint WHERE NOT convalidated;
```

and repair the rows, then `ALTER TABLE ... VALIDATE CONSTRAINT ...;`.

**Large databases.** On a database where these tables hold many rows, an operator applying 101
by hand should split it: add the constraints `NOT VALID` in one transaction, then run each
`VALIDATE CONSTRAINT` in its own transaction (it needs only `SHARE UPDATE EXCLUSIVE` and does not
block writes), and build the indexes with `CREATE INDEX CONCURRENTLY` outside a transaction. sqlx
cannot do that from inside a migration file, so it is a manual step on such a database; the current
single-owner databases are small enough that it does not matter.

## Rules for migrations 120 and later

1. Add every CHECK and foreign key `NOT VALID` in one migration file and `VALIDATE CONSTRAINT` in
   the **next** file (see `120_tool_execution_authorized_outcome.sql` and
   `121_validate_tool_execution_outcome.sql`). The validating file takes only
   `SHARE UPDATE EXCLUSIVE`.
2. Replacing a CHECK is `DROP CONSTRAINT` + `ADD CONSTRAINT ... NOT VALID` in one file, `VALIDATE` in
   the next.
3. Never write `IF NOT EXISTS` on a constraint or column the migration is meant to create: a silent
   skip hides a divergent schema.
4. New columns are nullable or have a constant default (metadata-only); a backfill is its own file.
5. Indexes on a table that may be large: `CREATE INDEX CONCURRENTLY` cannot run inside sqlx's
   transaction, so document it as an operator step instead of hiding a blocking build in a migration.
6. Do not edit an applied migration. Corrections are a new file (see `122_migration_notes.sql`, which
   only attaches comments).
