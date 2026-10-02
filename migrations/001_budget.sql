-- Budget ledger (spec sections 6, 7). All money is integer micro-USD.
CREATE TABLE budget_reservations (
    id              UUID PRIMARY KEY,
    task_id         UUID        NOT NULL,
    category        TEXT        NOT NULL CHECK (category IN ('metered', 'classifier')),
    task_kind       TEXT        NOT NULL CHECK (task_kind IN ('default', 'research', 'coding')),
    price_version   TEXT        NOT NULL CHECK (price_version <> ''),
    -- Period keys are Asia/Kolkata calendar day / first-of-month, derived from created_at (UTC).
    period_day      DATE        NOT NULL,
    period_month    DATE        NOT NULL,
    reserved_micros BIGINT      NOT NULL CHECK (reserved_micros > 0),
    -- Amount currently counted against caps: reserved while held/unresolved, actual once settled.
    counted_micros  BIGINT      NOT NULL CHECK (counted_micros >= 0),
    state           TEXT        NOT NULL CHECK (state IN ('held', 'settled', 'unresolved')),
    created_at      TIMESTAMPTZ NOT NULL,
    resolved_at     TIMESTAMPTZ
);
CREATE INDEX budget_reservations_task_idx  ON budget_reservations (task_id);
CREATE INDEX budget_reservations_day_idx   ON budget_reservations (period_day);
CREATE INDEX budget_reservations_month_idx ON budget_reservations (period_month, category);

CREATE TABLE budget_ledger (
    id                 UUID PRIMARY KEY,
    reservation_id     UUID        NOT NULL REFERENCES budget_reservations (id),
    -- Reconciliation identity: canonical form of the usage report; a replay maps to the same row.
    reconciliation_key TEXT        NOT NULL,
    amount_micros      BIGINT      NOT NULL CHECK (amount_micros >= 0),
    settled            BOOLEAN     NOT NULL,
    input_tokens       BIGINT      NOT NULL CHECK (input_tokens >= 0),
    output_tokens      BIGINT      NOT NULL CHECK (output_tokens >= 0),
    price_version      TEXT        NOT NULL,
    created_at         TIMESTAMPTZ NOT NULL,
    UNIQUE (reservation_id, reconciliation_key)
);
