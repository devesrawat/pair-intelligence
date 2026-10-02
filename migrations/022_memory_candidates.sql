CREATE TABLE memory_candidates (
    id                 uuid PRIMARY KEY,
    kind               text        NOT NULL CHECK (kind IN
        ('fact', 'preference', 'project', 'person', 'decision', 'procedure', 'goal', 'event', 'commitment', 'open_loop')),
    content            text        NOT NULL CHECK (length(content) > 0),
    normalized_content text        NOT NULL,
    topic_key          text,
    reason             text,
    project            text,
    inferred          boolean     NOT NULL DEFAULT false,
    state              text        NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'accepted', 'rejected')),
    accepted_memory_id uuid REFERENCES memories (id),
    observed_at        timestamptz,
    valid_from         timestamptz,
    valid_to           timestamptz,
    extraction_version text        NOT NULL DEFAULT 'v1',
    reviewed_by        text,
    reviewed_at        timestamptz,
    created_at         timestamptz NOT NULL DEFAULT now(),
    CHECK ((state = 'accepted') = (accepted_memory_id IS NOT NULL))
);
CREATE INDEX memory_candidates_state_idx ON memory_candidates (state, created_at);

CREATE TABLE memory_candidate_evidence (
    id           bigserial PRIMARY KEY,
    candidate_id uuid NOT NULL REFERENCES memory_candidates (id),
    source_id    uuid NOT NULL REFERENCES sources (id),
    span         text
);
CREATE UNIQUE INDEX memory_candidate_evidence_uniq
    ON memory_candidate_evidence (candidate_id, source_id, coalesce(span, ''));
