-- A conversation carries the highest data class any of its turns declared (spec global constraint:
-- data never reaches a model that is not allowed to see its class). Messages are stored without a
-- class, so without this column a later turn could re-send earlier, more sensitive history under a
-- lower label. The class only ever rises (public < personal < sensitive < employer); the API refuses
-- a turn declared below the stored class.
--
-- Existing conversations are backfilled to 'personal': their history was written without a recorded
-- class, so the conservative choice is the highest class turns may carry today. The default keeps
-- inserts that omit the column (older builds, rollback) on that same conservative class.
-- Additive: ADD COLUMN with a constant default is metadata-only on PostgreSQL 11+, and a rollback to
-- an older build ignores the column.
CREATE FUNCTION pair_data_class_rank(class text) RETURNS integer
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE AS
$$ SELECT CASE class
        WHEN 'public' THEN 0
        WHEN 'personal' THEN 1
        WHEN 'sensitive' THEN 2
        WHEN 'employer' THEN 3
    END $$;

ALTER TABLE conversations
    ADD COLUMN data_class text NOT NULL DEFAULT 'personal'
        CONSTRAINT conversations_data_class_check
        CHECK (data_class IN ('public', 'personal', 'sensitive', 'employer'));
