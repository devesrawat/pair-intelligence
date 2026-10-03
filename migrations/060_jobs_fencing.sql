-- Lease fencing, active-time deadlines and approval binding for jobs (review findings C1, H2, H4).

-- Every claim bumps lease_epoch; all lease-guarded writes match (lease_owner, lease_epoch), so a
-- worker that outlived its lease can never write after another worker has claimed the run.
ALTER TABLE workflow_runs ADD COLUMN lease_epoch bigint NOT NULL DEFAULT 0;

-- Deadline counts ACTIVE running time only: time parked in waiting_approval or queued is excluded.
-- active_base_ms is the running time consumed by earlier claims; claimed_at starts the current one.
ALTER TABLE workflow_runs ADD COLUMN claimed_at timestamptz;
ALTER TABLE workflow_runs ADD COLUMN active_base_ms bigint NOT NULL DEFAULT 0 CHECK (active_base_ms >= 0);

-- 'executing' is a CAS state entered (fenced) immediately before the effect closure is called; it
-- means the effect may have happened and must be reconciled, never re-run blindly.
-- 'not_applied' is the terminal state of an intent a sweeper proved never happened.
ALTER TABLE effect_intents DROP CONSTRAINT effect_intents_status_check;
ALTER TABLE effect_intents ADD CONSTRAINT effect_intents_status_check
    CHECK (status IN ('intended', 'executing', 'unknown', 'completed', 'not_applied'));

-- An approval authorizes exactly one intent.
ALTER TABLE effect_intents ADD COLUMN approval_id uuid UNIQUE REFERENCES approvals (id);

CREATE INDEX effect_intents_unresolved_idx ON effect_intents (run_id)
    WHERE status IN ('intended', 'executing', 'unknown');
