-- Shared with the policy/approval work; IF NOT EXISTS keeps migration order irrelevant.
CREATE TABLE IF NOT EXISTS audit_events (
    id             uuid PRIMARY KEY,
    occurred_at    timestamptz NOT NULL DEFAULT now(),
    actor          text        NOT NULL,
    action         text        NOT NULL,
    subject_kind   text        NOT NULL,
    subject_id     text        NOT NULL,
    outcome        text        NOT NULL DEFAULT 'ok',
    policy_version text,
    metadata       jsonb       NOT NULL DEFAULT '{}'::jsonb
);
CREATE INDEX IF NOT EXISTS audit_events_subject_idx ON audit_events (subject_kind, subject_id, occurred_at);
