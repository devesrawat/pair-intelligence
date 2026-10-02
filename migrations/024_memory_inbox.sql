-- Inbox review state: dedupe, contradiction links, review reasons, edits.
ALTER TABLE memory_candidates DROP CONSTRAINT memory_candidates_state_check;
ALTER TABLE memory_candidates ADD CONSTRAINT memory_candidates_state_check
    CHECK (state IN ('pending', 'accepted', 'rejected', 'edited'));
ALTER TABLE memory_candidates
    ADD COLUMN dedupe_key             text,
    ADD COLUMN seen_count             integer NOT NULL DEFAULT 1 CHECK (seen_count >= 1),
    ADD COLUMN needs_review           boolean NOT NULL DEFAULT false,
    ADD COLUMN review_reasons         text[]  NOT NULL DEFAULT '{}',
    ADD COLUMN contradicts_memories   uuid[]  NOT NULL DEFAULT '{}',
    ADD COLUMN contradicts_candidates uuid[]  NOT NULL DEFAULT '{}',
    ADD COLUMN auto_accepted          boolean NOT NULL DEFAULT false,
    ADD COLUMN edited_from            uuid REFERENCES memory_candidates (id),
    ADD COLUMN replaced_by            uuid REFERENCES memory_candidates (id);
UPDATE memory_candidates SET dedupe_key = encode(sha256(convert_to(id::text, 'UTF8')), 'hex') WHERE dedupe_key IS NULL;
ALTER TABLE memory_candidates ALTER COLUMN dedupe_key SET NOT NULL;
CREATE UNIQUE INDEX memory_candidates_dedupe_uniq ON memory_candidates (dedupe_key);

-- Competing memories deliberately kept side by side after review ("keep both").
CREATE TABLE memory_conflicts (
    memory_a    uuid        NOT NULL REFERENCES memories (id),
    memory_b    uuid        NOT NULL REFERENCES memories (id),
    resolved_by text        NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (memory_a, memory_b),
    CHECK (memory_a < memory_b)
);
