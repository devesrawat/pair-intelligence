-- Conversations, messages, model calls and audit events (spec section 12, Task 5).

CREATE TABLE conversations (
    id         uuid PRIMARY KEY,
    title      text NOT NULL DEFAULT '',
    trace_id   uuid NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE messages (
    id                uuid PRIMARY KEY,
    conversation_id   uuid NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    -- Caller-supplied identity; makes appends idempotent across retries and restarts.
    client_message_id text NOT NULL,
    seq               integer NOT NULL,
    role              text NOT NULL CHECK (role IN ('system', 'user', 'assistant', 'tool')),
    content           text NOT NULL,
    trust             text NOT NULL CHECK (trust IN ('owner', 'tool', 'untrusted')),
    trace_id          uuid NOT NULL,
    created_at        timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT messages_identity_uniq UNIQUE (conversation_id, client_message_id),
    CONSTRAINT messages_seq_uniq UNIQUE (conversation_id, seq)
);

CREATE TABLE model_calls (
    id                  uuid PRIMARY KEY,
    conversation_id     uuid REFERENCES conversations (id) ON DELETE SET NULL,
    response_message_id uuid REFERENCES messages (id) ON DELETE SET NULL,
    task_id             uuid NOT NULL,
    trace_id            uuid NOT NULL,
    reservation_id      uuid,
    provider            text NOT NULL,
    requested_model     text NOT NULL,
    -- Model id actually returned by the provider; routing never trusts aliases.
    resolved_model      text,
    provider_request_id text,
    input_tokens        bigint NOT NULL DEFAULT 0 CHECK (input_tokens >= 0),
    output_tokens       bigint NOT NULL DEFAULT 0 CHECK (output_tokens >= 0),
    cost_micros         bigint CHECK (cost_micros IS NULL OR cost_micros >= 0),
    price_version       text,
    -- unpriced: no price known; priced: computed from usage; reconciled: settled in the budget ledger.
    cost_state          text NOT NULL CHECK (cost_state IN ('unpriced', 'priced', 'reconciled')),
    route_reason        text NOT NULL,
    latency_ms          bigint NOT NULL CHECK (latency_ms >= 0),
    status              text NOT NULL CHECK (status IN ('ok', 'error')),
    error_code          text,
    verification        text NOT NULL DEFAULT 'none',
    created_at          timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX model_calls_conversation_idx ON model_calls (conversation_id, created_at);
CREATE INDEX model_calls_task_idx ON model_calls (task_id);
CREATE INDEX model_calls_trace_idx ON model_calls (trace_id);

CREATE TABLE audit_events (
    id         uuid PRIMARY KEY,
    trace_id   uuid NOT NULL,
    actor      text NOT NULL,
    kind       text NOT NULL,
    subject    text NOT NULL,
    detail     jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX audit_events_trace_idx ON audit_events (trace_id);
CREATE INDEX audit_events_kind_idx ON audit_events (kind, created_at);
