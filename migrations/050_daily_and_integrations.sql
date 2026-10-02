-- Task 14 (goals, commitments, daily routines) and Task 15 (optional integrations).
-- Self-contained: depends on no other migration. All timestamps are UTC.

CREATE TABLE goals (
    id            uuid PRIMARY KEY,
    title         text NOT NULL CHECK (length(title) > 0),
    owner         text NOT NULL,
    status        text NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'achieved', 'dropped')),
    parent_goal   uuid REFERENCES goals (id),
    due_at        timestamptz,
    evidence      jsonb NOT NULL DEFAULT '[]'::jsonb,
    created_at    timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE open_loops (
    id            uuid PRIMARY KEY,
    goal_id       uuid REFERENCES goals (id),
    related_loop  uuid REFERENCES open_loops (id),
    relationship  text,
    title         text NOT NULL CHECK (length(title) > 0),
    owner         text NOT NULL,
    kind          text NOT NULL CHECK (kind IN ('commitment', 'inference')),
    status        text NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'blocked', 'done', 'dropped')),
    delegable     boolean NOT NULL DEFAULT false,
    due_at        timestamptz,
    source_ref    text NOT NULL CHECK (length(source_ref) > 0),
    evidence      jsonb NOT NULL DEFAULT '[]'::jsonb,
    completed_at  timestamptz,
    created_at    timestamptz NOT NULL DEFAULT now(),
    CHECK (status <> 'done' OR completed_at IS NOT NULL)
);
CREATE INDEX open_loops_status_due_idx ON open_loops (status, due_at);

-- Recorded or observed events. 'intent' rows never change loop state.
CREATE TABLE daily_events (
    id            uuid PRIMARY KEY,
    occurred_at   timestamptz NOT NULL,
    kind          text NOT NULL CHECK (kind IN
                    ('intent', 'observed_work', 'observed_completion', 'meeting', 'decision')),
    summary       text NOT NULL CHECK (length(summary) > 0),
    loop_id       uuid REFERENCES open_loops (id),
    source_ref    text NOT NULL CHECK (length(source_ref) > 0)
);
CREATE INDEX daily_events_time_idx ON daily_events (occurred_at);

CREATE TABLE scheduled_routines (
    name                 text PRIMARY KEY,
    manual_checks_passed boolean NOT NULL DEFAULT false,
    next_run_at          timestamptz,
    last_run_at          timestamptz
);

CREATE TABLE integration_accounts (
    id             uuid PRIMARY KEY,
    provider       text NOT NULL CHECK (provider IN ('gmail', 'calendar')),
    state          text NOT NULL DEFAULT 'connected' CHECK (state IN ('connected', 'revoked', 'disconnected')),
    allowlist      jsonb NOT NULL DEFAULT '[]'::jsonb,
    writes_enabled boolean NOT NULL DEFAULT false,
    created_at     timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE integration_cursors (
    account_id uuid NOT NULL REFERENCES integration_accounts (id),
    scope      text NOT NULL,
    cursor     text NOT NULL,
    PRIMARY KEY (account_id, scope)
);

CREATE TABLE integration_sources (
    account_id  uuid NOT NULL REFERENCES integration_accounts (id),
    external_id text NOT NULL,
    scope       text NOT NULL,
    kind        text NOT NULL CHECK (kind IN ('message', 'event')),
    revision    text NOT NULL,
    content     jsonb,
    state       text NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'tombstoned')),
    captured_at timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, external_id)
);
