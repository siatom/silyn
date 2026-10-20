-- Policy versions, evolution candidates, evaluation records and promotions.
--
-- `policy_versions` is write-once: the checksum identifies the canonical JSON
-- content, and triggers reject `UPDATE`/`DELETE` so a promoted version can never
-- be edited or removed. Candidates never write here; a promotion inserts a new
-- version and moves `agents.active_policy_version`.
--
-- Rollback drills are audited in `audit_events` (action
-- `promotion.rollback_drill`) instead of a column: a drill is an event, and the
-- audit log is already append-only.

PRAGMA foreign_keys = ON;

CREATE TABLE policy_versions (
    id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL REFERENCES agents (id) ON DELETE CASCADE,
    parent_version TEXT,
    content_json TEXT NOT NULL,
    checksum TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL
);

CREATE TRIGGER policy_versions_no_update
BEFORE UPDATE ON policy_versions
BEGIN SELECT RAISE(ABORT, 'policy_versions are immutable'); END;

CREATE TRIGGER policy_versions_no_delete
BEFORE DELETE ON policy_versions
BEGIN SELECT RAISE(ABORT, 'policy_versions are immutable'); END;

CREATE TABLE evolution_candidates (
    id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL REFERENCES agents (id) ON DELETE CASCADE,
    base_policy_version TEXT NOT NULL REFERENCES policy_versions (id),
    patch_json TEXT NOT NULL,
    risk_class TEXT NOT NULL,
    state TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX idx_candidates_agent_state ON evolution_candidates (agent_id, state);

CREATE TABLE evaluation_runs (
    id TEXT PRIMARY KEY,
    candidate_id TEXT NOT NULL REFERENCES evolution_candidates (id) ON DELETE CASCADE,
    suite_version TEXT NOT NULL,
    baseline_id TEXT NOT NULL,
    metrics_json TEXT NOT NULL,
    report_path TEXT,
    created_at TEXT NOT NULL
);

CREATE TABLE metric_samples (
    evaluation_id TEXT NOT NULL REFERENCES evaluation_runs (id) ON DELETE CASCADE,
    task_id TEXT NOT NULL,
    arm TEXT NOT NULL,
    success INTEGER NOT NULL,
    PRIMARY KEY (evaluation_id, task_id, arm)
);

CREATE TABLE promotions (
    id TEXT PRIMARY KEY,
    candidate_id TEXT NOT NULL REFERENCES evolution_candidates (id),
    agent_id TEXT NOT NULL REFERENCES agents (id),
    from_version TEXT NOT NULL,
    to_version TEXT NOT NULL,
    approved_by TEXT NOT NULL,
    decision TEXT NOT NULL,
    reason TEXT,
    rolled_back_at TEXT,
    created_at TEXT NOT NULL
);
