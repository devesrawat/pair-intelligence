-- Accepted memories and their provenance. Candidates live in memory_candidates (022).
CREATE TABLE memories (
    id                 uuid PRIMARY KEY,
    kind               text        NOT NULL CHECK (kind IN
        ('fact', 'preference', 'project', 'person', 'decision', 'procedure', 'goal', 'event', 'commitment', 'open_loop')),
    status             text        NOT NULL CHECK (status IN ('accepted', 'superseded', 'expired')),
    content            text        NOT NULL CHECK (length(content) > 0),
    normalized_content text        NOT NULL,
    topic_key          text,
    project            text,
    valid_from         timestamptz NOT NULL,
    valid_to           timestamptz,
    observed_at        timestamptz NOT NULL,
    confidence         text        NOT NULL CHECK (confidence IN ('observed', 'inferred')),
    importance         smallint    NOT NULL DEFAULT 1 CHECK (importance BETWEEN 1 AND 10),
    supersedes_id      uuid REFERENCES memories (id),
    invalidated_reason text,
    accepted_by        text        NOT NULL,
    created_at         timestamptz NOT NULL DEFAULT now(),
    CHECK (valid_to IS NULL OR valid_to >= valid_from)
);
-- A memory can be superseded at most once, so supersession forms linear chains.
CREATE UNIQUE INDEX memories_supersedes_uniq ON memories (supersedes_id) WHERE supersedes_id IS NOT NULL;
CREATE INDEX memories_project_idx ON memories (project, status);
CREATE INDEX memories_topic_idx ON memories (kind, project, topic_key) WHERE topic_key IS NOT NULL;

CREATE TABLE memory_evidence (
    id                 bigserial PRIMARY KEY,
    memory_id          uuid NOT NULL REFERENCES memories (id),
    source_id          uuid NOT NULL REFERENCES sources (id),
    span               text,
    extraction_version text NOT NULL,
    created_at         timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX memory_evidence_uniq ON memory_evidence (memory_id, source_id, coalesce(span, ''));
CREATE INDEX memory_evidence_source_idx ON memory_evidence (source_id);

-- Invariant enforced in the database as well as the service: a memory row cannot be
-- committed without at least one evidence row.
CREATE FUNCTION memories_require_evidence() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM memory_evidence WHERE memory_id = NEW.id) THEN
        RAISE EXCEPTION 'memory % has no evidence', NEW.id USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER memories_require_evidence_trg
    AFTER INSERT ON memories
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION memories_require_evidence();

-- Derived search index. Embedding columns are a seam only; they stay NULL until an
-- embedding provider is enabled (see pair_memory::embedding).
CREATE TABLE memory_chunks (
    id                uuid PRIMARY KEY,
    memory_id         uuid NOT NULL REFERENCES memories (id) ON DELETE CASCADE,
    source_id         uuid REFERENCES sources (id),
    text              text NOT NULL CHECK (length(text) > 0),
    embedding_model   text,
    embedding_version text,
    dimensions        integer,
    search_vector     tsvector GENERATED ALWAYS AS (to_tsvector('english', text)) STORED,
    created_at        timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX memory_chunks_memory_idx ON memory_chunks (memory_id);
