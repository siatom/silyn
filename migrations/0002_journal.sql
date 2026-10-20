-- Event journal, recovery and idempotency. Immutable after merge.

CREATE TABLE event_journal (
    event_id TEXT PRIMARY KEY,
    aggregate_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    event_type TEXT NOT NULL,
    schema_version INTEGER NOT NULL,
    occurred_at TEXT NOT NULL,
    correlation_id TEXT,
    payload_ref TEXT,
    payload_json TEXT,
    UNIQUE (aggregate_id, seq)
);

CREATE INDEX idx_event_journal_occurred ON event_journal (occurred_at);
CREATE INDEX idx_event_journal_type ON event_journal (event_type);

CREATE TABLE checkpoints (
    run_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    state_ref TEXT NOT NULL,
    schema_version INTEGER NOT NULL,
    checksum TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (run_id, seq)
);

CREATE TABLE outbox (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id TEXT NOT NULL REFERENCES event_journal (event_id) ON DELETE CASCADE,
    consumer TEXT NOT NULL,
    available_at TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    processed_at TEXT,
    UNIQUE (consumer, event_id)
);

CREATE INDEX idx_outbox_pending ON outbox (consumer, available_at) WHERE processed_at IS NULL;

CREATE TABLE tool_call_ledger (
    run_id TEXT NOT NULL,
    step_id TEXT NOT NULL,
    tool_call_id TEXT NOT NULL,
    tool_name TEXT NOT NULL,
    state TEXT NOT NULL,
    result_ref TEXT,
    created_at TEXT NOT NULL,
    PRIMARY KEY (run_id, step_id, tool_call_id)
);

CREATE TABLE audit_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    occurred_at TEXT NOT NULL,
    actor TEXT NOT NULL,
    action TEXT NOT NULL,
    subject TEXT,
    decision TEXT,
    details_ref TEXT
);

CREATE INDEX idx_audit_occurred ON audit_events (occurred_at);
