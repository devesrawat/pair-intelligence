-- One row per logical turn (conversation + client_message_id), taken BEFORE any paid work so two
-- concurrent identical requests cost one provider call: the loser sees `in_progress` (409) or, once
-- the winner finished, `done` (the stored answer is returned). The content hash lets a replay that
-- carries different text be refused instead of being processed under the old audit row.
--
-- `failed` turns may be claimed again (a client retry shares the task's attempt counter and cap).
-- An `in_progress` claim older than the lease (the process died mid-turn) can be taken over too.
-- Additive: a new table that older builds never read.
CREATE TABLE turn_claims (
    task_id           uuid PRIMARY KEY,
    conversation_id   uuid        NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    client_message_id text        NOT NULL,
    content_sha256    text        NOT NULL,
    state             text        NOT NULL CHECK (state IN ('in_progress', 'done', 'failed')),
    claimed_at        timestamptz NOT NULL DEFAULT now(),
    finished_at       timestamptz,
    CONSTRAINT turn_claims_identity_uniq UNIQUE (conversation_id, client_message_id)
);
