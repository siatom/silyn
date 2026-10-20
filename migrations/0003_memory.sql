-- Memory substrate: content-addressed blobs, memory records, provenance,
-- relations, validity windows and FTS5 retrieval. Append-only.

PRAGMA foreign_keys = ON;

CREATE TABLE content_blobs (
    id TEXT PRIMARY KEY,              -- sha256 hex
    sha256 TEXT NOT NULL UNIQUE,
    content TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE memories (
    id TEXT PRIMARY KEY,
    owner_user_id TEXT NOT NULL,
    agent_id TEXT NOT NULL REFERENCES agents (id) ON DELETE CASCADE,
    scope TEXT NOT NULL,              -- session | agent | user | shared
    type TEXT NOT NULL,               -- working | episodic | semantic | procedural
    content_ref TEXT NOT NULL REFERENCES content_blobs (id),
    summary TEXT NOT NULL,
    source_event_ids TEXT NOT NULL,   -- JSON array of strings
    provenance TEXT NOT NULL,         -- user_statement | external_evidence | model_inference | tool_output
    confidence REAL NOT NULL,
    importance REAL NOT NULL,
    validity_from TEXT,
    validity_to TEXT,
    last_used_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    status TEXT NOT NULL,             -- active | superseded | expired | deleted
    supersedes_id TEXT,
    sensitivity TEXT NOT NULL,        -- public | private | secret
    embedding_version TEXT
);

CREATE INDEX idx_memories_agent_scope ON memories (agent_id, owner_user_id, scope, status);
CREATE INDEX idx_memories_type ON memories (agent_id, type, status);
CREATE INDEX idx_memories_validity ON memories (validity_from, validity_to);
CREATE INDEX idx_memories_content_ref ON memories (content_ref);

CREATE TABLE memory_sources (
    memory_id TEXT NOT NULL REFERENCES memories (id) ON DELETE CASCADE,
    source_type TEXT NOT NULL,
    source_ref TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (memory_id, source_type, source_ref)
);

CREATE TABLE memory_relations (
    from_id TEXT NOT NULL REFERENCES memories (id) ON DELETE CASCADE,
    to_id TEXT NOT NULL REFERENCES memories (id) ON DELETE CASCADE,
    relation_type TEXT NOT NULL,      -- supports | contradicts | supersedes
    created_at TEXT NOT NULL,
    PRIMARY KEY (from_id, to_id, relation_type)
);

CREATE VIRTUAL TABLE memory_fts USING fts5(
    memory_id UNINDEXED,
    summary,
    content,
    tokenize = 'unicode61'
);
