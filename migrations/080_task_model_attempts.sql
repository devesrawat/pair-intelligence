-- Persisted per-task model-attempt counter (spec section 6: at most 3 model attempts per task,
-- across generation, repair and escalation). The counter lives in the database so step retries,
-- HTTP retries and restarts cannot multiply the cap. Consumed atomically before each provider call.
CREATE TABLE task_model_attempts (
    task_id    uuid PRIMARY KEY,
    used       integer     NOT NULL CHECK (used >= 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
