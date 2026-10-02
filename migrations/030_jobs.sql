-- Durable job state, step checkpoints, hash-bound approvals, side-effect intents (spec sections 9, 10).

CREATE TABLE workflow_runs (
    id                  uuid PRIMARY KEY,
    idempotency_key     uuid NOT NULL UNIQUE,
    kind                text NOT NULL,
    input               jsonb NOT NULL,
    input_hash          text NOT NULL,
    run_class           text NOT NULL CHECK (run_class IN ('interactive', 'research')),
    state               text NOT NULL CHECK (state IN
        ('queued', 'running', 'waiting_approval', 'succeeded', 'failed', 'cancelled', 'interrupted')),
    next_step           integer NOT NULL DEFAULT 0,
    tool_calls          integer NOT NULL DEFAULT 0,
    lease_owner         text,
    lease_expires_at    timestamptz,
    deadline_at         timestamptz NOT NULL,
    pending_action_hash text,
    approval_id         uuid,
    output              jsonb,
    failure             text,
    created_at          timestamptz NOT NULL DEFAULT now(),
    started_at          timestamptz,
    updated_at          timestamptz NOT NULL DEFAULT now(),
    finished_at         timestamptz
);
CREATE INDEX workflow_runs_claim_idx ON workflow_runs (created_at) WHERE state = 'queued';
CREATE INDEX workflow_runs_lease_idx ON workflow_runs (lease_expires_at) WHERE state = 'running';

CREATE TABLE workflow_steps (
    run_id       uuid NOT NULL REFERENCES workflow_runs (id) ON DELETE CASCADE,
    idx          integer NOT NULL,
    name         text NOT NULL,
    output       jsonb NOT NULL,
    completed_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (run_id, idx)
);

CREATE TABLE approvals (
    id          uuid PRIMARY KEY,
    action_hash text NOT NULL,
    actor       text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    expires_at  timestamptz NOT NULL,
    consumed_at timestamptz,
    consumed_by uuid
);
CREATE INDEX approvals_hash_idx ON approvals (action_hash);

-- Written BEFORE an external effect runs; completed after. 'intended' or 'unknown' on resume => reconcile.
CREATE TABLE effect_intents (
    id           uuid PRIMARY KEY,
    run_id       uuid NOT NULL REFERENCES workflow_runs (id) ON DELETE CASCADE,
    effect_key   text NOT NULL,
    status       text NOT NULL CHECK (status IN ('intended', 'unknown', 'completed')),
    payload      jsonb NOT NULL,
    result       jsonb,
    created_at   timestamptz NOT NULL DEFAULT now(),
    completed_at timestamptz,
    UNIQUE (run_id, effect_key)
);
