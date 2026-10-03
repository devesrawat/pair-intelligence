-- Spec section 7 tables that did not exist: projects, eval_cases, eval_results.
-- (tool_executions is owned by the jobs/policy migrations.) New tables only: nothing here
-- rewrites or locks existing data, and a build from before this migration keeps working.

CREATE TABLE projects (
    id         uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    name       text        NOT NULL CHECK (length(name) > 0),
    owner      text        NOT NULL CHECK (length(owner) > 0),
    status     text        NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'paused', 'archived')),
    created_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT projects_name_uniq UNIQUE (name)
);

-- One row per (dataset, version, case id). `split` keeps the frozen held-out discipline of spec 6.1
-- visible in the database; `label_status` records whether the owner has reviewed the expected label.
CREATE TABLE eval_cases (
    id           uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    dataset      text        NOT NULL CHECK (length(dataset) > 0),
    version      text        NOT NULL CHECK (length(version) > 0),
    case_id      text        NOT NULL CHECK (length(case_id) > 0),
    input        jsonb       NOT NULL,
    expected     jsonb       NOT NULL,
    split        text        NOT NULL CHECK (split IN ('dev', 'held_out')),
    label_status text        NOT NULL DEFAULT 'draft_unreviewed' CHECK (length(label_status) > 0),
    created_at   timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT eval_cases_identity_uniq UNIQUE (dataset, version, case_id)
);

-- Results are append-only observations; a case cannot be deleted while results reference it.
CREATE TABLE eval_results (
    id             uuid             PRIMARY KEY DEFAULT gen_random_uuid(),
    case_id        uuid             NOT NULL REFERENCES eval_cases (id) ON DELETE RESTRICT,
    config_version text             NOT NULL CHECK (length(config_version) > 0),
    model          text             NOT NULL CHECK (length(model) > 0),
    score          double precision NOT NULL,
    notes          text,
    run_at         timestamptz      NOT NULL DEFAULT now()
);
CREATE INDEX eval_results_case_idx ON eval_results (case_id, run_at);
