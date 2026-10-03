-- Budget review findings M1/M2: persisted task kind per task and overrun flag on ledger rows.

-- The task kind is fixed by the first reservation for a task; later reservations of a different
-- kind are rejected, so a task cannot dodge its cap by switching kind.
CREATE TABLE budget_tasks (
    task_id    UUID PRIMARY KEY,
    task_kind  TEXT        NOT NULL CHECK (task_kind IN ('default', 'research', 'coding')),
    created_at TIMESTAMPTZ NOT NULL
);
INSERT INTO budget_tasks (task_id, task_kind, created_at)
SELECT DISTINCT ON (task_id) task_id, task_kind, created_at
FROM budget_reservations ORDER BY task_id, created_at;

-- actual > reserved at settlement.
ALTER TABLE budget_ledger ADD COLUMN overrun BOOLEAN NOT NULL DEFAULT FALSE;
