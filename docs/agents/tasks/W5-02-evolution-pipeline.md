# W5-02: Policy versions, candidate pipeline and promotion/rollback

```yaml
id: W5-02
wave: 5
depends_on: [W1-01, W1-02, W4-02]
owns:
  - migrations/0005_evolution.sql
  - migrations/checksums.txt
  - crates/silyn-growth/src/policy.rs
  - crates/silyn-growth/src/candidate.rs
  - crates/silyn-growth/src/promotion.rs
  - crates/silyn-growth/src/lib.rs
  - crates/silyn-growth/tests/evolution.rs
  - crates/silyn-growth/Cargo.toml
  - eval/acceptance/w5-02.toml
  - docs/gates/w5-02.md
reads:
  - schemas/policy_artifact.schema.json
  - schemas/policy_patch.schema.json
  - docs/architecture/adr/0003-policy-artifact-and-patch.md
  - docs/architecture/adr/0008-low-risk-auto-promotion.md
  - crates/silyn-core/src/{contracts.rs,state.rs,ids.rs,error.rs}
  - crates/silyn-storage/src/**
  - crates/silyn-growth/src/{memory.rs,experience.rs}
forbids:
  - migrations/0001_core.sql
  - migrations/0002_journal.sql
  - migrations/0003_memory.sql
  - migrations/0004_learning.sql
  - crates/silyn-core/**
  - crates/silyn-storage/**
  - crates/silyn-runtime/**
  - crates/silyn-app/**
  - crates/silyn-cli/**
  - eval/suites/**
  - Cargo.toml
contracts: [PolicyArtifact v1, PolicyPatch v1, CandidateState v1, PromotionDecision v1]
acceptance_registry:
  - policy.canonical_checksum_is_stable
  - policy.version_is_immutable
  - candidate.path_whitelist_rejects_tools
  - candidate.bounds_rejected
  - candidate.evidence_must_exist
  - candidate.state_machine_enforced
  - promotion.low_risk_auto_promotes
  - promotion.default_is_deny
  - promotion.rollback_restores_previous
  - promotion.audit_chain_complete
commands:
  - cargo test -p silyn-growth
  - cargo clippy -p silyn-growth --all-targets -- -D warnings
  - cargo run -p xtask -- verify-migrations
  - cargo run -p xtask -- verify-schemas
  - cargo run -p xtask -- verify-acceptance
```

## Goal

Implement the L2 evolution control plane: immutable policy versions, editable
policy patches with static checks, the candidate state machine, an
evaluation-report promotion gate (low-risk auto, default deny), and audited
rollback. A candidate can never widen tools, verification, budgets, thresholds,
approval settings or executable code.

## Non-goals

- No evaluation execution (W5-01); accept a validated report struct/hash.
- No CLI commands (W5-03/W7).
- No L3 code/tool changes.
- No sealed-suite access.

## Migration `0005_evolution.sql`

```sql
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
```

Update `migrations/checksums.txt` with the 0005 sha256.

## Policy API (`silyn-growth/src/policy.rs`)

```rust
pub struct PolicyArtifact {
    pub schema_version: u32,
    pub context: ContextPolicy,
    pub planner: PlannerPolicy,
    pub tools: ToolPolicy,
    pub verification: VerificationPolicy,
}

pub struct PolicyVersionRecord {
    pub id: PolicyVersionId,
    pub agent_id: AgentId,
    pub parent_version: Option<PolicyVersionId>,
    pub content: PolicyArtifact,
    pub checksum: String,
    pub created_at: DateTime<Utc>,
}

pub struct PolicyStore<'a> { /* pool */ }
impl<'a> PolicyStore<'a> {
    pub fn new(pool: &'a SqlitePool) -> Self;
    pub async fn create(&self, agent_id: AgentId, parent: Option<PolicyVersionId>,
        policy: &PolicyArtifact) -> Result<PolicyVersionRecord, SilynError>;
    pub async fn get(&self, agent_id: AgentId, id: PolicyVersionId)
        -> Result<Option<PolicyVersionRecord>, SilynError>;
    pub async fn latest(&self, agent_id: AgentId) -> Result<Option<PolicyVersionRecord>, SilynError>;
}
```

- Canonical serialization: deterministic key order (build serde structs in
  declaration order; no maps with nondeterministic iteration), compact JSON.
- Checksum = sha256 of canonical bytes; identical policy -> identical checksum;
  duplicate checksum is idempotent or rejected with the existing id.
- Validate bounds: `memory_budget_ratio` 0–0.6, `bm25_k` 1–200,
  `max_evidence` 1–50, tool allowlist non-empty and unique, strings non-empty.
- Trigger prevents updates; test it.

## Candidate API (`silyn-growth/src/candidate.rs`)

```rust
pub struct PolicyPatch {
    pub schema_version: u32,
    pub candidate_id: CandidateId,
    pub base_policy_version: PolicyVersionId,
    pub hypothesis: String,
    pub evidence_run_ids: Vec<RunId>,
    pub expected_gain: ExpectedGain,
    pub risk_class: RiskClass, // Low | Medium | High
    pub operations: Vec<PatchOperation>,
}

pub struct CandidateRecord { /* id, agent, base, patch, risk, state, timestamps */ }

pub struct CandidateStore<'a> { /* pool + PolicyStore */ }
impl<'a> CandidateStore<'a> {
    pub fn new(pool: &'a SqlitePool) -> Self;
    pub async fn submit(&self, agent_id: AgentId, patch: PolicyPatch)
        -> Result<CandidateRecord, SilynError>;
    pub async fn get(&self, agent_id: AgentId, id: CandidateId)
        -> Result<Option<CandidateRecord>, SilynError>;
    pub async fn transition(&self, id: CandidateId, to: CandidateState)
        -> Result<CandidateRecord, SilynError>;
    pub async fn list(&self, agent_id: AgentId) -> Result<Vec<CandidateRecord>, SilynError>;
}
```

Static checks (all permanent errors, no evaluation run created):

- `schema_version == 1`, non-empty hypothesis (<= 2000), 1–20 operations.
- Paths match `^/(planner/(prompt_fragments|rules)|context/retrieval)(/.*)?$`;
  reject `/tools`, `/verification`, budgets, thresholds, approval, code.
- `add`/`replace` require `value`; `remove` must not carry one.
- Text values cannot contain capability grants (`fs.`, `capability`, `tool`,
  `approval`, `threshold`, `oracle`, `budget`, `system prompt`) in an
  escalatory pattern; reject control characters and strings > 4000 bytes.
- Numeric values for retrieval stay in bounds; `memory_budget_ratio` is not
  patchable by this schema (context/retrieval only).
- Every `evidence_run_id` must exist, be terminal (`Completed`/`Failed`/
  `Cancelled`), belong to the agent's task/session, and have evidence
  (`RunRecord` + at least one step or verified event).
- `base_policy_version` must exist for the agent and be the active version.
- Candidate starts `Proposed`, immediately becomes `StaticChecked` on success.
- `transition` uses `LifecycleState::can_transition_to`; illegal transitions
  fail permanently.

## Promotion API (`silyn-growth/src/promotion.rs`)

```rust
pub struct PromotionReport {
    pub candidate_id: CandidateId,
    pub verdict: String,            // promote | reject
    pub gain_pp: f64,
    pub ci_low_pp: f64,
    pub unauthorized_actions: u64,
    pub evidence_run_ids: Vec<RunId>,
    pub suite_version: String,
    pub report_checksum: String,
}

pub struct PromotionStore<'a> { /* pool */ }
impl<'a> PromotionStore<'a> {
    pub fn new(pool: &'a SqlitePool) -> Self;
    pub async fn decide(&self, agent_id: AgentId, report: &PromotionReport)
        -> Result<PromotionDecision, SilynError>;
    pub async fn promote(&self, agent_id: AgentId, candidate_id: CandidateId,
        approved_by: &str) -> Result<PromotionRecord, SilynError>;
    pub async fn rollback(&self, promotion_id: PromotionId, actor: &str)
        -> Result<PolicyVersionRecord, SilynError>;
}
```

Rules (ADR 0008):

- Auto-promote only if risk `low`, all patch operations in the allowlist,
  `verdict == promote`, `gain_pp >= min_gain_pp`, `ci_low_pp > 0`,
  `unauthorized_actions == 0`, every evidence run is terminal/verified, the
  report checksum matches, and a successful rollback drill is recorded.
- Otherwise `AwaitingApproval`; no operator means it stays there. `Reject` only
  for hard failures (invalid report, evidence, state, checksum).
- `promote`: move candidate to `Promoted`, create a new immutable policy
  version from the patched artifact, switch `agents.active_policy_version`,
  record `promotions` and an `audit_events` row.
- `rollback`: restore the previous version pointer, mark promotion
  `rolled_back_at`, candidate `RolledBack`, audit row. Rollback of a rolled-back
  promotion is a permanent error.
- All transitions and audit rows in one transaction.

## Tests

- canonical checksum stable across two builds; duplicate policy idempotent.
- update trigger rejects mutation.
- patch path traversal/tools/verification/budget rejected.
- retrieval bounds rejected.
- evidence run unknown/non-terminal rejected.
- state machine skips rejected.
- low-risk auto promotion succeeds; medium/high waits; invalid report rejects.
- rollback restores pointer and writes audit; double rollback fails.
- promotion audit chain has candidate, from/to versions, actor and decision.

Add `eval/acceptance/w5-02.toml` with one gate per test, `required = true`.
Add `docs/gates/w5-02.md`.

## Commands and commit

```bash
cargo fmt --all
cargo test -p silyn-growth
cargo clippy -p silyn-growth --all-targets -- -D warnings
cargo run -p xtask -- verify-migrations
cargo run -p xtask -- verify-schemas
cargo run -p xtask -- verify-acceptance
```

Commit once on `wave5-evolution` with
`feat(growth): add immutable policies and audited promotion pipeline`.
