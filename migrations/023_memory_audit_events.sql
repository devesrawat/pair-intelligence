-- Memory-domain audit log. Separate from the conversations audit_events table (010), which has a different shape.
CREATE TABLE memory_audit_events (
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
CREATE INDEX memory_audit_events_subject_idx ON memory_audit_events (subject_kind, subject_id, occurred_at);
