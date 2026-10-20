# W4-02: Outcome verification and provisional experience extraction

```yaml
id: W4-02
wave: 4
depends_on: [W1-01, W1-02, W4-01]
owns:
  - migrations/0004_learning.sql
  - migrations/checksums.txt
  - crates/silyn-growth/src/verifier.rs
  - crates/silyn-growth/src/experience.rs
  - crates/silyn-growth/src/lib.rs
  - crates/silyn-growth/tests/verifier.rs
  - crates/silyn-growth/tests/experience.rs
  - crates/silyn-growth/Cargo.toml
  - eval/acceptance/w4-02.toml
  - docs/gates/w4-02.md
reads:
  - crates/silyn-core/src/contracts.rs
  - crates/silyn-core/src/event.rs
  - crates/silyn-core/src/state.rs
  - crates/silyn-storage/src/**
  - crates/silyn-growth/src/memory.rs
  - docs/architecture/adr/0002-data-and-events.md
forbids:
  - migrations/0001_core.sql
  - migrations/0002_journal.sql
  - migrations/0003_memory.sql
  - crates/silyn-core/**
  - crates/silyn-storage/**
  - crates/silyn-runtime/**
  - crates/silyn-app/**
  - crates/silyn-cli/**
  - eval/suites/**
  - Cargo.toml
contracts: [OutcomeVerifier v1, CheckResult v1, ExperienceRecord v1]
acceptance_registry:
  - verify.success_requires_independent_evidence
  - verify.failed_dominates_unknown
  - verify.no_checker_is_unknown
  - verify.expected_answer_json_subset
  - experience.failed_run_creates_provisional
  - experience.unknown_creates_no_success
  - experience.deduplicates_lesson
  - experience.confirm_requires_two_runs
  - experience.cross_agent_isolation
commands:
  - cargo test -p silyn-growth
  - cargo clippy -p silyn-growth --all-targets -- -D warnings
  - cargo run -p xtask -- verify-migrations
  - cargo run -p xtask -- verify-acceptance
```

## Goal

Provide a pluggable, evidence-based `OutcomeVerifier` and a deterministic
experience extractor. A run is only `Success` when independent machine checks
agree; `Unknown` never becomes a remembered success. Failed runs can produce a
**provisional** lesson tied to run evidence, which is deduplicated and must be
confirmed by later, independent evidence.

## Non-goals

- No model self-assessment as evidence. No LLM call is allowed in a checker.
- No reflection, policy candidates or promotion (W5).
- No context injection (W4-03).
- No automatic confirmation from a single run.

## Migration `0004_learning.sql`

```sql
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
```

Update `migrations/checksums.txt` with the 0004 sha256. Do not edit 0003.

## Verification API (`silyn-growth/src/verifier.rs`)

Re-export from `lib.rs`:

```rust
pub enum CheckResult {
    Success { evidence: Vec<String> },
    Failed { code: String, reason: String, evidence: Vec<String> },
    Unknown { reason: String },
}

#[async_trait]
pub trait Checker: Send + Sync {
    async fn check(&self, run: &RunSnapshot, artifacts: &ArtifactSet)
        -> Result<CheckResult, SilynError>;
}

pub struct ArtifactSet { /* loads inline/file refs once */ }
impl ArtifactSet {
    pub fn new(root: Option<PathBuf>, refs: &[String]) -> Self;
    pub fn get(&self, reference: &str) -> Option<&str>;
    pub fn all(&self) -> &[String];
}

pub struct CompositeVerifier { /* checkers, policy */ }
pub enum VerificationPolicy { AnyFailureFails, AllMustSucceed }
impl CompositeVerifier {
    pub fn new(checkers: Vec<Arc<dyn Checker>>) -> Self;
    pub fn with_policy(self, policy: VerificationPolicy) -> Self;
}
#[async_trait]
impl OutcomeVerifier for CompositeVerifier { ... }
```

Rules:

- Run `RunSnapshot.artifact_refs` entries may be:
  - `inline:<text>` — inline content;
  - `file:<path>` — file relative to `ArtifactSet.root`, path-sandboxed;
  - a plain string — treated as a file path when it exists, otherwise as
    inline content (W3 compatibility).
- `ExpectedAnswerChecker::new(expected: serde_json::Value)` parses the first
  inline/JSON artifact and requires every expected key to match exactly
  (objects are subset-matched recursively; arrays and scalars exact).
  Missing/extra top-level keys are allowed; malformed JSON is `Failed`.
- `FileContainsChecker::new(needle)` checks any artifact containing the needle.
- `CompositeVerifier`:
  - any `Failed` -> `Failed` (first failure, all evidence merged);
  - all checkers `Success` -> `Success` with merged evidence;
  - otherwise `Unknown` (including zero checkers).
- Checkers must not call a model, network or shell. They may read the given
  files only.
- `RunSnapshot` comes from `silyn-core`; do not change it.

## Experience API (`silyn-growth/src/experience.rs`)

```rust
pub enum EvidenceStatus { Provisional, Confirmed, Rejected, Unknown }
pub enum ExperienceStatus { Active, Retired }

pub struct ExperienceRecord {
    pub id: ExperienceId,
    pub agent_id: AgentId,
    pub lesson: String,
    pub evidence_status: EvidenceStatus,
    pub confidence: f32,
    pub source_run_ids: Vec<RunId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub status: ExperienceStatus,
}

pub struct ExperienceStore<'a> { /* pool */ }
impl<'a> ExperienceStore<'a> {
    pub fn new(pool: &'a SqlitePool) -> Self;
    pub async fn extract(&self, agent_id: AgentId, run_id: RunId, result: &CheckResult)
        -> Result<Option<ExperienceRecord>, SilynError>;
    pub async fn list(&self, agent_id: AgentId)
        -> Result<Vec<ExperienceRecord>, SilynError>;
    pub async fn confirm(&self, id: ExperienceId) -> Result<ExperienceRecord, SilynError>;
    pub async fn retire(&self, id: ExperienceId) -> Result<(), SilynError>;
}
```

Deterministic lesson map (no model call):

| Failure code | Lesson |
|---|---|
| `unit_mismatch` | Check units before computing aggregates. |
| `missing_values` | Check for missing values before counting. |
| `duplicate_rows` | Check for duplicate rows before counting. |
| `wrong_count` | Recompute row counts from distinct rows. |
| `tool_error` | Inspect the tool result before assuming success. |
| other code | `Verify the result before reporting it.` |

Rules:

- `CheckResult::Unknown` -> `Ok(None)`; never create a success experience.
- `CheckResult::Success` -> create a **provisional** positive lesson
  `Verified approach: <reason/evidence summary>`? Keep it deterministic: use
  `"Verified result using independent checks."` with confidence `0.4`.
- `CheckResult::Failed` -> provisional lesson from the map, confidence `0.3`.
- Normalize lesson: trim, collapse whitespace, lowercase. Deduplicate by
  `(agent_id, normalized_lesson)` among active rows; add a new
  `experience_evidence` row when the run is new, do not duplicate.
- `source_run_ids` stays a JSON array in sync with evidence rows.
- `confirm`: requires at least 2 distinct evidence runs and at least one
  `success` evidence. `Unknown` evidence never counts. Otherwise permanent
  error. Sets `confirmed`, confidence `0.7`.
- `retire`: status `retired`; the partial unique index allows a new active
  lesson with the same text later.
- All reads/writes filter by `agent_id`; an experience from agent A is invisible
  to agent B.

## Tests

- verifier: Success only when all checkers succeed; any Failed dominates
  Unknown; no checker -> Unknown; expected-answer subset matches/mismatches;
  malformed artifact -> Failed.
- experience: a failed run creates one provisional lesson; `Unknown` creates
  none; two failed runs with the same code produce one row and two evidence
  rows; `confirm` fails with one run/only failures and succeeds with two runs
  including a success; unknown evidence never confirms; A/B isolation; retire
  then re-extract creates a new active row.

Add `eval/acceptance/w4-02.toml` with one gate per test, `required = true`.
Add `docs/gates/w4-02.md` with scope, commands, results and deferred work
(reflection, policy candidates, context injection).

## Commands and commit

```bash
cargo fmt --all
cargo test -p silyn-growth
cargo clippy -p silyn-growth --all-targets -- -D warnings
cargo run -p xtask -- verify-migrations
cargo run -p xtask -- verify-acceptance
```

Commit once on `wave4-verify` with
`feat(growth): add evidence-based verifier and provisional experiences`.
