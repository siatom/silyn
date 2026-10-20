# W4-01: Memory store, lifecycle, FTS5 retrieval and purge

```yaml
id: W4-01
wave: 4
depends_on: [W1-01, W1-02]
owns:
  - migrations/0003_memory.sql
  - migrations/checksums.txt
  - crates/silyn-growth/src/memory.rs
  - crates/silyn-growth/src/content.rs
  - crates/silyn-growth/src/lib.rs
  - crates/silyn-growth/tests/memory.rs
  - crates/silyn-growth/Cargo.toml
  - eval/acceptance/w4-01.toml
  - docs/gates/w4-01.md
reads:
  - crates/silyn-core/src/contracts.rs
  - crates/silyn-core/src/ids.rs
  - crates/silyn-core/src/state.rs
  - crates/silyn-storage/src/**
  - migrations/0001_core.sql
  - migrations/0002_journal.sql
  - docs/architecture/adr/0002-data-and-events.md
  - docs/architecture/adr/0005-trust-boundaries-and-capabilities.md
forbids:
  - migrations/0001_core.sql
  - migrations/0002_journal.sql
  - crates/silyn-core/**
  - crates/silyn-storage/**
  - crates/silyn-runtime/**
  - crates/silyn-app/**
  - crates/silyn-cli/**
  - eval/suites/**
  - Cargo.toml
contracts: [MemoryRecord v1, MemoryRelation v1, FTS5 query contract]
acceptance_registry:
  - memory.cross_agent_isolation
  - memory.conflict_does_not_overwrite
  - memory.expiry_filters_retrieval
  - memory.fts_search_returns_ranked_hits
  - memory.supersede_preserves_history
  - memory.purge_removes_blob_and_index
  - memory.blob_deduplicates_content
commands:
  - cargo test -p silyn-growth
  - cargo clippy -p silyn-growth --all-targets -- -D warnings
  - cargo run -p xtask -- verify-migrations
  - cargo run -p xtask -- verify-acceptance
```

## Goal

Implement the persistent memory substrate for one agent: content-addressed blobs,
four memory scopes/types, provenance and confidence, validity windows, conflict
relations, supersession, FTS5 retrieval with strict agent/owner isolation, and
delete propagation for user deletion requests.

## Non-goals

- No LLM extraction or reflection (W4-02).
- No context-builder injection into the runtime (W4-03).
- No embedding/vector search; FTS5 only.
- No cross-user sharing UI; shared scope is represented but requires an explicit
  owner/agent match.

## Migration `0003_memory.sql`

Append-only. Suggested schema (adjust names only if necessary, keep the fields):

```sql
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
    content_ref TEXT NOT NULL,        -- content_blobs.id
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
```

Update `migrations/checksums.txt` with the sha256 of the new file; run
`verify-migrations`. Do not edit 0001/0002.

## Public API (`silyn-growth`)

Re-export from `lib.rs`:

```rust
pub mod content;
pub mod memory;

pub use content::ContentStore;
pub use memory::{
    MemoryHitRecord, MemoryRecord, MemoryRelation, MemoryScopeFilter, MemoryStatus,
    MemoryStore, NewMemory, Provenance, PurgeReport, RelationType,
};
```

### ContentStore

```rust
pub struct ContentStore<'a> { /* pool */ }
impl<'a> ContentStore<'a> {
    pub fn new(pool: &'a SqlitePool) -> Self;
    pub async fn put(&self, content: &str) -> Result<String, SilynError>; // sha256 id
    pub async fn get(&self, id: &str) -> Result<Option<String>, SilynError>;
    pub async fn ref_count(&self, id: &str) -> Result<u64, SilynError>;
}
```

`put` is idempotent for identical bytes. Do not store empty content.

### MemoryStore

```rust
pub struct NewMemory {
    pub owner_user_id: String,
    pub agent_id: AgentId,
    pub scope: MemoryScope,
    pub memory_type: MemoryType,
    pub content: String,
    pub summary: String,
    pub source_event_ids: Vec<String>,
    pub provenance: Provenance,
    pub confidence: f32,
    pub importance: f32,
    pub validity_from: Option<DateTime<Utc>>,
    pub validity_to: Option<DateTime<Utc>>,
    pub sensitivity: String,
}

pub struct MemoryScopeFilter {
    pub owner_user_id: String,
    pub agent_id: AgentId,
    pub scopes: Vec<MemoryScope>,     // empty = agent + session only
    pub types: Vec<MemoryType>,       // empty = all
    pub include_expired: bool,
}

impl<'a> MemoryStore<'a> {
    pub fn new(pool: &'a SqlitePool) -> Self;
    pub async fn put(&self, new: NewMemory) -> Result<MemoryRecord, SilynError>;
    pub async fn get(&self, owner: &str, agent: AgentId, id: MemoryId)
        -> Result<Option<MemoryRecord>, SilynError>;
    pub async fn search(&self, filter: &MemoryScopeFilter, query: &str, limit: usize)
        -> Result<Vec<MemoryHitRecord>, SilynError>;
    pub async fn relate(&self, from: MemoryId, to: MemoryId, relation: RelationType)
        -> Result<(), SilynError>;
    pub async fn supersede(&self, old: MemoryId, new: MemoryId)
        -> Result<(), SilynError>;
    pub async fn expire(&self, id: MemoryId, at: DateTime<Utc>)
        -> Result<(), SilynError>;
    pub async fn touch(&self, id: MemoryId, at: DateTime<Utc>)
        -> Result<(), SilynError>;
    pub async fn soft_delete(&self, id: MemoryId, actor: &str)
        -> Result<(), SilynError>;
    pub async fn purge(&self, id: MemoryId, actor: &str)
        -> Result<PurgeReport, SilynError>;
}
```

Rules:

- `put`: validate agent exists, non-empty summary/content, confidence/importance in
  `[0,1]`, `validity_to > validity_from` when both set. Insert blob, memory row,
  source rows, FTS row in one transaction. `source_event_ids` must be JSON.
  Provenance is stored verbatim; model inference is **not** more trusted than
  tool output.
- `search`: filter `owner_user_id`, `agent_id`, `status = active`,
  `validity_from <= now`, `(validity_to IS NULL OR validity_to > now)` unless
  `include_expired`. FTS5 MATCH on summary/content. Quote user tokens to avoid
  FTS syntax errors. Order by `bm25(memory_fts)`, then importance DESC, then
  `created_at` DESC. Return id, summary, content, provenance, confidence,
  importance, status, score. Empty query returns the newest active memories.
- `relate`: insert relation; reject self-relations and duplicates idempotently.
  A `contradicts` relation never changes either memory's status.
- `supersede(old, new)`: transactional; mark old `superseded`, set
  `new.supersedes_id = old`, insert `supersedes` relation. The old blob and rows
  remain (audit history).
- `expire`: set status `expired` and `validity_to`.
- `soft_delete`: status `deleted` (keeps blob for audit until purge).
- `purge`: transactional hard delete of the memory, its FTS row, sources and
  relations; decrement/recompute the blob reference count and delete the blob
  when unreferenced. Insert an `audit_events` row with actor, action
  `memory.purge`, subject memory id, decision `allow`. Return
  `PurgeReport { memory_id, blob_removed, relations_removed, sources_removed }`.
- All reads must include both `agent_id` and `owner_user_id`; a mismatch returns
  `None`/empty, never another agent's row.

## Tests

Use a temp/in-memory `Storage`, create agents via `AgentRepository`, and cover:

- cross-agent isolation: agent A's memory is invisible to agent B and to the
  wrong owner; search returns empty.
- conflict: `contradicts` leaves both active; neither summary changes.
- expiry: a memory with `validity_to` in the past is excluded by default and
  included with `include_expired`.
- FTS: retrieves by a term in summary and in content; ranks a higher-importance
  memory first for equal text; special characters in the query do not error.
- supersession: old status `superseded`, new `supersedes_id` set, relation row
  present, old row still readable.
- purge: memory/FTS/sources/relations gone, blob removed when unreferenced,
  audit row exists; a second memory sharing the blob keeps it alive.
- blob dedupe: two puts of the same content return one blob id / content row.

Add `eval/acceptance/w4-01.toml` with one gate per test, `required = true`.
Add `docs/gates/w4-01.md` with scope, commands, results and deferred work
(embeddings, consolidation, retention policy).

## Commands and commit

```bash
cargo fmt --all
cargo test -p silyn-growth
cargo clippy -p silyn-growth --all-targets -- -D warnings
cargo run -p xtask -- verify-migrations
cargo run -p xtask -- verify-acceptance
```

Commit once on `wave4/memory` with
`feat(growth): add content-addressed memory store and fts retrieval`.
