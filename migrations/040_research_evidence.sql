-- Task 13: source-grounded research evidence tables.
-- Standalone (no FKs to other migrations) so it applies in any order after the base schema.

CREATE TABLE IF NOT EXISTS research_runs (
    id          UUID PRIMARY KEY,
    question    TEXT        NOT NULL,
    scope       JSONB       NOT NULL,
    status      TEXT        NOT NULL DEFAULT 'running'
                CHECK (status IN ('running', 'completed', 'failed')),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- One row per captured source version. Webpage text is untrusted data, stored verbatim.
CREATE TABLE IF NOT EXISTS research_sources (
    id                 UUID PRIMARY KEY,
    run_id             UUID        NOT NULL REFERENCES research_runs (id) ON DELETE CASCADE,
    url                TEXT        NOT NULL,
    normalized_url     TEXT        NOT NULL,
    available          BOOLEAN     NOT NULL,
    unavailable_reason TEXT,
    revision           TEXT,                 -- etag / version string / commit as served
    content_sha256     TEXT,                 -- hash of the captured text
    published_at       DATE,                 -- source date when known
    fetched_at         TIMESTAMPTZ NOT NULL,
    text               TEXT,
    trust              TEXT        NOT NULL DEFAULT 'untrusted' CHECK (trust = 'untrusted'),
    duplicate_of       UUID REFERENCES research_sources (id),
    CHECK ((available AND content_sha256 IS NOT NULL AND text IS NOT NULL)
        OR (NOT available AND unavailable_reason IS NOT NULL)),
    UNIQUE (run_id, normalized_url)
);

CREATE TABLE IF NOT EXISTS research_claims (
    id            UUID PRIMARY KEY,
    run_id        UUID NOT NULL REFERENCES research_runs (id) ON DELETE CASCADE,
    topic         TEXT NOT NULL,
    claim_text    TEXT NOT NULL,
    claim_value   TEXT NOT NULL,
    status        TEXT NOT NULL CHECK (status IN ('validated', 'rejected')),
    reject_reason TEXT,
    CHECK ((status = 'rejected') = (reject_reason IS NOT NULL))
);

-- Exact supporting span per citation; source version pinned by hash.
CREATE TABLE IF NOT EXISTS research_claim_evidence (
    id                  UUID PRIMARY KEY,
    claim_id            UUID NOT NULL REFERENCES research_claims (id) ON DELETE CASCADE,
    source_id           UUID REFERENCES research_sources (id),
    cited_url           TEXT NOT NULL,
    span                TEXT NOT NULL,
    span_start          INTEGER,            -- char offset in sources.text when located
    source_sha256       TEXT,
    source_published_at DATE
);

CREATE TABLE IF NOT EXISTS research_reports (
    run_id     UUID PRIMARY KEY REFERENCES research_runs (id) ON DELETE CASCADE,
    markdown   TEXT        NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS research_claims_run_idx ON research_claims (run_id, topic);
CREATE INDEX IF NOT EXISTS research_claim_evidence_claim_idx ON research_claim_evidence (claim_id);
