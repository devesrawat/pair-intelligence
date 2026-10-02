-- Sources: immutable per revision. A new content hash for the same (kind, external_id)
-- creates a new revision row; deletion/visibility apply to every revision of a source.
CREATE TABLE sources (
    id             uuid PRIMARY KEY,
    kind           text        NOT NULL CHECK (length(kind) > 0),
    external_id    text        NOT NULL CHECK (length(external_id) > 0),
    revision       integer     NOT NULL CHECK (revision >= 1),
    content_hash   text        NOT NULL CHECK (length(content_hash) > 0),
    captured_at    timestamptz NOT NULL DEFAULT now(),
    data_class     text        NOT NULL CHECK (data_class IN ('public', 'personal', 'sensitive', 'employer')),
    trust          text        NOT NULL CHECK (trust IN ('owner', 'tool', 'untrusted')),
    uri            text,
    visibility     text        NOT NULL DEFAULT 'visible' CHECK (visibility IN ('visible', 'hidden')),
    deletion_state text        NOT NULL DEFAULT 'active' CHECK (deletion_state IN ('active', 'deleted')),
    deleted_at     timestamptz,
    UNIQUE (kind, external_id, revision),
    CHECK ((deletion_state = 'deleted') = (deleted_at IS NOT NULL))
);
CREATE INDEX sources_identity_idx ON sources (kind, external_id);
