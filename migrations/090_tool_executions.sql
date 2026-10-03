-- Audit row for every gate call (spec sections 2 and 12): every tool execution, refusal and
-- approval-gated run is traceable by trace_id. Raw arguments are NEVER stored, only their sha256.

CREATE TABLE tool_executions (
    id             uuid PRIMARY KEY,
    task_id        uuid NOT NULL,
    trace_id       uuid NOT NULL,
    tool           text NOT NULL,
    executable     text,
    args_hash      text NOT NULL CHECK (args_hash ~ '^[0-9a-f]{64}$'),
    destination    text,
    data_class     text NOT NULL CHECK (data_class IN ('public', 'personal', 'sensitive', 'employer')),
    policy_version text NOT NULL,
    decision       text NOT NULL CHECK (decision IN ('allow', 'deny', 'needs_approval')),
    approval_id    uuid REFERENCES approvals (id),
    -- 'started' is the only non-terminal outcome; a row stuck there means the process died mid-call.
    outcome        text NOT NULL CHECK (outcome IN ('started', 'ok', 'error', 'denied', 'approval_required')),
    error_code     text,
    started_at     timestamptz NOT NULL DEFAULT now(),
    finished_at    timestamptz,
    CONSTRAINT tool_executions_finished_matches_outcome
        CHECK ((outcome = 'started') = (finished_at IS NULL)),
    -- A refusal never ran, so it carries no approval; a denied row is always decision 'deny' or an
    -- approval that was required but missing.
    CONSTRAINT tool_executions_refusal_shape
        CHECK (outcome <> 'denied' OR decision = 'deny'),
    CONSTRAINT tool_executions_approval_required_shape
        CHECK (outcome <> 'approval_required' OR (decision = 'needs_approval' AND approval_id IS NULL))
);
CREATE INDEX tool_executions_trace_idx ON tool_executions (trace_id);
CREATE INDEX tool_executions_task_idx ON tool_executions (task_id);
CREATE INDEX tool_executions_unfinished_idx ON tool_executions (started_at) WHERE outcome = 'started';
