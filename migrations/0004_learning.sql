-- Learned experiences and the run evidence that supports them. Append-only.
--
-- An experience starts `provisional` from a single verified run. Confirmation
-- needs at least two distinct evidence runs and at least one of them a success;
-- `unknown` evidence is stored for audit but never counts. Retired rows stay so
-- the same lesson can be extracted again as a new active row later.

PRAGMA foreign_keys = ON;

CREATE TABLE experiences (
    id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL REFERENCES agents (id) ON DELETE CASCADE,
    lesson TEXT NOT NULL,
    normalized_lesson TEXT NOT NULL,
    evidence_status TEXT NOT NULL,  -- provisional | confirmed | rejected | unknown
    confidence REAL NOT NULL,
    source_run_ids TEXT NOT NULL,   -- JSON array
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    last_used_at TEXT,
    status TEXT NOT NULL            -- active | retired
);

CREATE UNIQUE INDEX idx_experiences_active_lesson
    ON experiences (agent_id, normalized_lesson) WHERE status = 'active';

CREATE TABLE experience_evidence (
    experience_id TEXT NOT NULL REFERENCES experiences (id) ON DELETE CASCADE,
    run_id TEXT NOT NULL REFERENCES runs (id) ON DELETE CASCADE,
    outcome TEXT NOT NULL,          -- success | failed | unknown
    evidence_ref TEXT,
    created_at TEXT NOT NULL,
    PRIMARY KEY (experience_id, run_id)
);
