-- Core entities. Immutable after merge: add a new migration to change schema.
PRAGMA foreign_keys = ON;

CREATE TABLE agents (
    id TEXT PRIMARY KEY,
    owner_id TEXT NOT NULL,
    profile_version TEXT NOT NULL,
    active_policy_version TEXT,
    created_at TEXT NOT NULL
);

CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL REFERENCES agents (id) ON DELETE CASCADE,
    context_version TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE INDEX idx_sessions_agent ON sessions (agent_id, created_at);

CREATE TABLE tasks (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    objective TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX idx_tasks_session ON tasks (session_id, created_at);
CREATE INDEX idx_tasks_status ON tasks (status);

CREATE TABLE runs (
    id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES tasks (id) ON DELETE CASCADE,
    policy_version TEXT NOT NULL,
    memory_snapshot_version TEXT,
    model_id TEXT NOT NULL,
    status TEXT NOT NULL,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cost_usd REAL NOT NULL DEFAULT 0,
    started_at TEXT,
    finished_at TEXT,
    created_at TEXT NOT NULL
);

CREATE INDEX idx_runs_task ON runs (task_id, created_at);
CREATE INDEX idx_runs_status ON runs (status);

CREATE TABLE run_steps (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs (id) ON DELETE CASCADE,
    seq INTEGER NOT NULL,
    action_type TEXT NOT NULL,
    result_ref TEXT,
    created_at TEXT NOT NULL,
    UNIQUE (run_id, seq)
);
