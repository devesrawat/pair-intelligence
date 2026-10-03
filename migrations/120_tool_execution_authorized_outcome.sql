-- `POST /v1/policy/authorize` records every decision in tool_executions. An Allow executes
-- nothing inside PAIR (the caller runs the tool), so it is recorded as a terminal decision row
-- with the outcome 'authorized' instead of pretending a tool ran ('ok').
--
-- Migration style from 120 on (see docs/migrations-notes.md): sqlx wraps each migration file in
-- ONE transaction, so a constraint added here and validated here would hold its lock for the whole
-- scan. Constraints are therefore added NOT VALID (enforced for every new write at once, no scan)
-- and VALIDATEd in the NEXT migration file (121), which only needs SHARE UPDATE EXCLUSIVE.
ALTER TABLE tool_executions DROP CONSTRAINT tool_executions_outcome_check;
ALTER TABLE tool_executions
    ADD CONSTRAINT tool_executions_outcome_check
    CHECK (outcome IN ('started', 'ok', 'error', 'denied', 'approval_required', 'authorized'))
    NOT VALID;
