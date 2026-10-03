-- Evidence invariant, second half. 021 only checks memories at INSERT; a later DELETE (or a
-- re-pointing UPDATE) of memory_evidence could leave a live memory with no provenance at all.
-- This deferred constraint trigger re-checks the OLD memory at commit. A memory deleted in the
-- same transaction is exempt. Readers treat a memory with no evidence as redacted (read.rs).
CREATE OR REPLACE FUNCTION memory_evidence_keep_one() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM memories WHERE id = OLD.memory_id)
       AND NOT EXISTS (SELECT 1 FROM memory_evidence WHERE memory_id = OLD.memory_id) THEN
        RAISE EXCEPTION 'memory % has no evidence', OLD.memory_id USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;

DROP TRIGGER IF EXISTS memory_evidence_keep_one_trg ON memory_evidence;
CREATE CONSTRAINT TRIGGER memory_evidence_keep_one_trg
    AFTER DELETE OR UPDATE OF memory_id ON memory_evidence
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION memory_evidence_keep_one();
