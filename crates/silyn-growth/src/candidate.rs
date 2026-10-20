//! Policy candidates: the restricted patch protocol, static checks and the
//! candidate state machine (ADR 0003).
//!
//! A candidate may only edit planner text and retrieval bounds. Every other
//! path — tools, verification, budgets, thresholds, approval configuration —
//! is outside the whitelist and is rejected before anything is written, so a
//! compromised reflection process cannot widen its own permissions.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use silyn_core::SilynError;
use silyn_core::ids::{AgentId, CandidateId, PolicyVersionId, RunId};
use silyn_core::state::{CandidateState, LifecycleState, RunStatus};
use silyn_storage::db_error;
use silyn_storage::error::parse_id;
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqliteConnection, SqlitePool};

use crate::policy::{
    MAX_BM25_K, MAX_LIST_ENTRIES, MAX_MAX_EVIDENCE, MIN_BM25_K, MIN_MAX_EVIDENCE, PolicyArtifact,
    PolicyStore, RetrievalPolicy, check_text_value, decode_enum, encode_enum,
};

/// Schema version of [`PolicyPatch`]. Bump only with a new schema.
pub const PATCH_SCHEMA_VERSION: u32 = 1;
/// Longest accepted hypothesis, in characters.
pub const MAX_HYPOTHESIS_CHARS: usize = 2000;
/// Operations allowed in one patch.
pub const MAX_OPERATIONS: usize = 20;
/// Evidence runs allowed on one candidate.
pub const MAX_EVIDENCE_RUNS: usize = 50;

const PROMPT_FRAGMENTS_PATH: &str = "/planner/prompt_fragments";
const RULES_PATH: &str = "/planner/rules";
const RETRIEVAL_PATH: &str = "/context/retrieval";

/// Substrings that never belong in a candidate-authored text value.
const ESCALATION_SUBSTRINGS: [&str; 2] = ["fs.", "system prompt"];
/// Whole words that attempt to grant capability, widen budgets or touch the
/// grader. Matched on word boundaries so `toolbox` does not trip them.
const ESCALATION_WORDS: [&str; 12] = [
    "capability",
    "capabilities",
    "tool",
    "tools",
    "approval",
    "approvals",
    "threshold",
    "thresholds",
    "oracle",
    "oracles",
    "budget",
    "budgets",
];

/// How much review a change needs. Only `low` can auto-promote (ADR 0008).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskClass {
    Low,
    Medium,
    High,
}

/// RFC 6902 operation name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchOp {
    Add,
    Replace,
    Remove,
}

/// One restricted patch operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchOperation {
    pub op: PatchOp,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
}

/// Expected improvement, recorded with the candidate but never trusted alone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedGain {
    pub metric: String,
    pub delta_pp: f64,
}

/// A proposed policy change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyPatch {
    pub schema_version: u32,
    pub candidate_id: CandidateId,
    pub base_policy_version: PolicyVersionId,
    pub hypothesis: String,
    pub evidence_run_ids: Vec<RunId>,
    pub expected_gain: ExpectedGain,
    pub risk_class: RiskClass,
    pub operations: Vec<PatchOperation>,
}

/// One stored candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateRecord {
    pub id: CandidateId,
    pub agent_id: AgentId,
    pub base_policy_version: PolicyVersionId,
    pub patch: PolicyPatch,
    pub risk_class: RiskClass,
    pub state: CandidateState,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// What a whitelisted path addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    PromptFragments(Option<usize>),
    Rules(Option<usize>),
    Retrieval(RetrievalLeaf),
}

/// Which part of `context.retrieval` a path addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetrievalLeaf {
    /// The whole object; individual keys replace the stored values.
    Object,
    Bm25K,
    MaxEvidence,
}

/// Rejects a patch that static checks would fail, without touching storage.
///
/// This is the whole allowlist: schema version, hypothesis, operation count,
/// path whitelist, op/value pairing, text caps and escalation phrases, and
/// retrieval bounds. Evidence and base-version checks need the database and
/// live in [`CandidateStore::submit`].
pub fn validate_patch(patch: &PolicyPatch) -> Result<(), SilynError> {
    if patch.schema_version != PATCH_SCHEMA_VERSION {
        return Err(SilynError::Schema(format!(
            "patch schema_version must be {PATCH_SCHEMA_VERSION}"
        )));
    }

    let hypothesis = &patch.hypothesis;
    if hypothesis.trim().is_empty() {
        return Err(SilynError::Schema(
            "hypothesis must not be empty".to_owned(),
        ));
    }
    if hypothesis.chars().count() > MAX_HYPOTHESIS_CHARS {
        return Err(SilynError::Schema(format!(
            "hypothesis exceeds {MAX_HYPOTHESIS_CHARS} characters"
        )));
    }
    if hypothesis
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(SilynError::Schema(
            "hypothesis contains control characters".to_owned(),
        ));
    }

    if patch.expected_gain.metric.trim().is_empty() {
        return Err(SilynError::Schema(
            "expected_gain.metric must not be empty".to_owned(),
        ));
    }
    if !patch.expected_gain.delta_pp.is_finite() {
        return Err(SilynError::Schema(
            "expected_gain.delta_pp must be finite".to_owned(),
        ));
    }

    if patch.evidence_run_ids.is_empty() {
        return Err(SilynError::Schema(
            "a candidate needs at least one evidence run".to_owned(),
        ));
    }
    if patch.evidence_run_ids.len() > MAX_EVIDENCE_RUNS {
        return Err(SilynError::Schema(format!(
            "a candidate accepts at most {MAX_EVIDENCE_RUNS} evidence runs"
        )));
    }
    let mut seen = HashSet::new();
    for run_id in &patch.evidence_run_ids {
        if !seen.insert(*run_id) {
            return Err(SilynError::Schema(format!(
                "evidence run {run_id} is listed twice"
            )));
        }
    }

    if patch.operations.is_empty() {
        return Err(SilynError::Schema(
            "a candidate needs at least one operation".to_owned(),
        ));
    }
    if patch.operations.len() > MAX_OPERATIONS {
        return Err(SilynError::Schema(format!(
            "a candidate accepts at most {MAX_OPERATIONS} operations"
        )));
    }
    for operation in &patch.operations {
        validate_operation(operation)?;
    }
    Ok(())
}

/// Applies a validated patch to a base artifact.
///
/// List semantics follow RFC 6902: `/planner/rules` replaces the whole list,
/// `/planner/rules/2` replaces, inserts or removes one entry. Retrieval bounds
/// accept either the leaf path (`/context/retrieval/bm25_k`) or the whole
/// object, whose supplied keys replace the stored values. The result is
/// validated again, so a patch can never produce an out-of-bounds artifact.
pub fn apply_patch(
    base: &PolicyArtifact,
    operations: &[PatchOperation],
) -> Result<PolicyArtifact, SilynError> {
    let mut policy = base.clone();
    for operation in operations {
        match parse_path(&operation.path)? {
            Target::PromptFragments(index) => {
                apply_list(&mut policy.planner.prompt_fragments, index, operation)?;
            }
            Target::Rules(index) => {
                apply_list(&mut policy.planner.rules, index, operation)?;
            }
            Target::Retrieval(leaf) => {
                apply_retrieval(&mut policy.context.retrieval, leaf, operation)?;
            }
        }
    }
    policy.validate()?;
    Ok(policy)
}

/// Candidate reads and writes for one SQLite pool.
pub struct CandidateStore<'a> {
    pool: &'a SqlitePool,
    policies: PolicyStore<'a>,
}

impl<'a> CandidateStore<'a> {
    #[must_use]
    pub fn new(pool: &'a SqlitePool) -> Self {
        Self {
            pool,
            policies: PolicyStore::new(pool),
        }
    }

    /// Runs static checks and stores the candidate as `StaticChecked`.
    ///
    /// A candidate starts `Proposed` and moves to `StaticChecked` inside the
    /// same transaction. Nothing is written when any check fails, and no
    /// evaluation run is created here.
    pub async fn submit(
        &self,
        agent_id: AgentId,
        patch: PolicyPatch,
    ) -> Result<CandidateRecord, SilynError> {
        validate_patch(&patch)?;

        let base = self
            .policies
            .get(agent_id, patch.base_policy_version)
            .await?
            .ok_or_else(|| {
                SilynError::Permanent(format!(
                    "base policy version {} not found for agent {agent_id}",
                    patch.base_policy_version
                ))
            })?;
        match self.policies.active(agent_id).await? {
            Some(active) if active.id == base.id => {}
            Some(_) => {
                return Err(SilynError::PolicyDenied(format!(
                    "base policy version {} is not the active policy",
                    patch.base_policy_version
                )));
            }
            None => {
                return Err(SilynError::Permanent(format!(
                    "agent {agent_id} has no active policy version"
                )));
            }
        }

        for run_id in &patch.evidence_run_ids {
            self.verify_evidence(agent_id, *run_id).await?;
        }

        let patch_json = serde_json::to_string(&patch)?;
        let risk_class = patch.risk_class;
        let candidate_id = patch.candidate_id;
        let mut transaction = self.pool.begin().await.map_err(db_error)?;

        if let Some(existing) = load_candidate(&mut transaction, candidate_id).await? {
            if existing.agent_id != agent_id {
                return Err(SilynError::PolicyDenied(format!(
                    "candidate {candidate_id} belongs to another agent"
                )));
            }
            if existing.patch == patch {
                transaction.rollback().await.map_err(db_error)?;
                return Ok(existing);
            }
            return Err(SilynError::Permanent(format!(
                "candidate {candidate_id} already exists with a different patch"
            )));
        }

        let now = Utc::now();
        sqlx::query(
            "INSERT INTO evolution_candidates (
                id, agent_id, base_policy_version, patch_json, risk_class, state,
                created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )
        .bind(candidate_id.to_string())
        .bind(agent_id.to_string())
        .bind(base.id.to_string())
        .bind(&patch_json)
        .bind(encode_enum(&risk_class)?)
        .bind(encode_enum(&CandidateState::Proposed)?)
        .bind(now)
        .bind(now)
        .execute(&mut *transaction)
        .await
        .map_err(db_error)?;

        update_state(
            &mut transaction,
            candidate_id,
            CandidateState::Proposed,
            CandidateState::StaticChecked,
            now,
        )
        .await?;
        transaction.commit().await.map_err(db_error)?;

        Ok(CandidateRecord {
            id: candidate_id,
            agent_id,
            base_policy_version: base.id,
            patch,
            risk_class,
            state: CandidateState::StaticChecked,
            created_at: now,
            updated_at: now,
        })
    }

    /// Reads one candidate, scoped to the agent.
    pub async fn get(
        &self,
        agent_id: AgentId,
        id: CandidateId,
    ) -> Result<Option<CandidateRecord>, SilynError> {
        let mut connection = self.pool.acquire().await.map_err(db_error)?;
        let record = load_candidate(&mut connection, id).await?;
        Ok(record.filter(|record| record.agent_id == agent_id))
    }

    /// Moves a candidate along the state machine.
    ///
    /// Repeating the current state is a no-op; any edge missing from
    /// [`CandidateState::can_transition_to`] is a permanent error.
    pub async fn transition(
        &self,
        id: CandidateId,
        to: CandidateState,
    ) -> Result<CandidateRecord, SilynError> {
        let mut transaction = self.pool.begin().await.map_err(db_error)?;
        let record = load_candidate(&mut transaction, id)
            .await?
            .ok_or_else(|| SilynError::Permanent(format!("candidate {id} not found")))?;

        if record.state == to {
            transaction.rollback().await.map_err(db_error)?;
            return Ok(record);
        }
        if !record.state.can_transition_to(to) {
            return Err(SilynError::InvalidTransition {
                from: encode_enum(&record.state)?,
                to: encode_enum(&to)?,
            });
        }

        let now = Utc::now();
        update_state(&mut transaction, id, record.state, to, now).await?;
        transaction.commit().await.map_err(db_error)?;

        Ok(CandidateRecord {
            state: to,
            updated_at: now,
            ..record
        })
    }

    /// Every candidate of one agent, oldest first.
    pub async fn list(&self, agent_id: AgentId) -> Result<Vec<CandidateRecord>, SilynError> {
        let rows = sqlx::query(
            "SELECT id, agent_id, base_policy_version, patch_json, risk_class, state,
                    created_at, updated_at
             FROM evolution_candidates WHERE agent_id = ?1
             ORDER BY created_at ASC, id ASC",
        )
        .bind(agent_id.to_string())
        .fetch_all(self.pool)
        .await
        .map_err(db_error)?;

        rows.iter().map(candidate_from_row).collect()
    }

    /// A candidate cannot cite a run from another agent, a run that is still
    /// running, or a run without any step or verified event.
    async fn verify_evidence(&self, agent_id: AgentId, run_id: RunId) -> Result<(), SilynError> {
        let mut connection = self.pool.acquire().await.map_err(db_error)?;
        verify_evidence_run(&mut connection, agent_id, run_id).await
    }
}

/// Evidence rule shared by the candidate store and the promotion gate.
pub(crate) async fn verify_evidence_run(
    conn: &mut SqliteConnection,
    agent_id: AgentId,
    run_id: RunId,
) -> Result<(), SilynError> {
    let owner: Option<String> = sqlx::query_scalar(
        "SELECT s.agent_id FROM runs r
         JOIN tasks t ON t.id = r.task_id
         JOIN sessions s ON s.id = t.session_id
         WHERE r.id = ?1",
    )
    .bind(run_id.to_string())
    .fetch_optional(&mut *conn)
    .await
    .map_err(db_error)?;

    match owner {
        None => {
            return Err(SilynError::Permanent(format!(
                "evidence run {run_id} not found"
            )));
        }
        Some(owner) if owner != agent_id.to_string() => {
            return Err(SilynError::PolicyDenied(format!(
                "evidence run {run_id} does not belong to agent {agent_id}"
            )));
        }
        Some(_) => {}
    }

    let status: String = sqlx::query_scalar("SELECT status FROM runs WHERE id = ?1")
        .bind(run_id.to_string())
        .fetch_one(&mut *conn)
        .await
        .map_err(db_error)?;
    let status: RunStatus = decode_enum(&status)?;
    if !status.is_terminal() {
        return Err(SilynError::Permanent(format!(
            "evidence run {run_id} is not terminal ({status:?})"
        )));
    }

    let evidence: i64 = sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM run_steps WHERE run_id = ?1)
              + (SELECT COUNT(*) FROM event_journal
                 WHERE aggregate_id = ?1 AND event_type = 'run_verified')",
    )
    .bind(run_id.to_string())
    .fetch_one(&mut *conn)
    .await
    .map_err(db_error)?;
    if evidence == 0 {
        return Err(SilynError::Permanent(format!(
            "evidence run {run_id} has no step or verified event"
        )));
    }
    Ok(())
}

/// Reads a candidate inside the caller's transaction or connection.
pub(crate) async fn load_candidate(
    conn: &mut SqliteConnection,
    id: CandidateId,
) -> Result<Option<CandidateRecord>, SilynError> {
    let row = sqlx::query(
        "SELECT id, agent_id, base_policy_version, patch_json, risk_class, state,
                created_at, updated_at
         FROM evolution_candidates WHERE id = ?1",
    )
    .bind(id.to_string())
    .fetch_optional(&mut *conn)
    .await
    .map_err(db_error)?;

    row.as_ref().map(candidate_from_row).transpose()
}

/// Conditional state write. Fails when the row is no longer in `from`.
pub(crate) async fn update_state(
    conn: &mut SqliteConnection,
    id: CandidateId,
    from: CandidateState,
    to: CandidateState,
    updated_at: DateTime<Utc>,
) -> Result<(), SilynError> {
    if !from.can_transition_to(to) {
        return Err(SilynError::InvalidTransition {
            from: encode_enum(&from)?,
            to: encode_enum(&to)?,
        });
    }

    let updated = sqlx::query(
        "UPDATE evolution_candidates SET state = ?3, updated_at = ?4
         WHERE id = ?1 AND state = ?2",
    )
    .bind(id.to_string())
    .bind(encode_enum(&from)?)
    .bind(encode_enum(&to)?)
    .bind(updated_at)
    .execute(&mut *conn)
    .await
    .map_err(db_error)?;

    if updated.rows_affected() == 0 {
        return Err(SilynError::Permanent(format!(
            "candidate {id} is no longer in state {from:?}"
        )));
    }
    Ok(())
}

fn candidate_from_row(row: &SqliteRow) -> Result<CandidateRecord, SilynError> {
    let patch_json: String = row.try_get("patch_json").map_err(db_error)?;
    Ok(CandidateRecord {
        id: parse_id(
            &row.try_get::<String, _>("id").map_err(db_error)?,
            "evolution_candidates.id",
        )?,
        agent_id: parse_id(
            &row.try_get::<String, _>("agent_id").map_err(db_error)?,
            "evolution_candidates.agent_id",
        )?,
        base_policy_version: parse_id(
            &row.try_get::<String, _>("base_policy_version")
                .map_err(db_error)?,
            "evolution_candidates.base_policy_version",
        )?,
        patch: serde_json::from_str(&patch_json)?,
        risk_class: decode_enum(&row.try_get::<String, _>("risk_class").map_err(db_error)?)?,
        state: decode_enum(&row.try_get::<String, _>("state").map_err(db_error)?)?,
        created_at: row.try_get("created_at").map_err(db_error)?,
        updated_at: row.try_get("updated_at").map_err(db_error)?,
    })
}

fn validate_operation(operation: &PatchOperation) -> Result<(), SilynError> {
    let target = parse_path(&operation.path)?;
    match operation.op {
        PatchOp::Add | PatchOp::Replace => {
            let value = operation.value.as_ref().ok_or_else(|| {
                SilynError::Schema(format!(
                    "`{:?}` at `{}` requires a value",
                    operation.op, operation.path
                ))
            })?;
            validate_value(target, value)
        }
        PatchOp::Remove => {
            if operation.value.is_some() {
                return Err(SilynError::Schema(format!(
                    "`remove` at `{}` must not carry a value",
                    operation.path
                )));
            }
            match target {
                Target::PromptFragments(None) | Target::Rules(None) => Err(SilynError::Schema(
                    "required policy fields cannot be removed".to_owned(),
                )),
                Target::Retrieval(_) => Err(SilynError::Schema(
                    "retrieval settings cannot be removed".to_owned(),
                )),
                Target::PromptFragments(Some(_)) | Target::Rules(Some(_)) => Ok(()),
            }
        }
    }
}

fn validate_value(target: Target, value: &Value) -> Result<(), SilynError> {
    match target {
        Target::PromptFragments(None) | Target::Rules(None) => {
            for item in string_list(value)? {
                check_candidate_text("planner text", item)?;
            }
            Ok(())
        }
        Target::PromptFragments(Some(_)) | Target::Rules(Some(_)) => {
            check_candidate_text("planner text", string_value(value)?)
        }
        Target::Retrieval(RetrievalLeaf::Object) => {
            let object = value.as_object().ok_or_else(|| {
                SilynError::Schema("retrieval replacement must be an object".to_owned())
            })?;
            if object.is_empty() {
                return Err(SilynError::Schema(
                    "retrieval replacement must name at least one bound".to_owned(),
                ));
            }
            for (key, value) in object {
                match key.as_str() {
                    "bm25_k" => check_integer("bm25_k", value, MIN_BM25_K, MAX_BM25_K)?,
                    "max_evidence" => {
                        check_integer("max_evidence", value, MIN_MAX_EVIDENCE, MAX_MAX_EVIDENCE)?;
                    }
                    other => {
                        return Err(SilynError::PolicyDenied(format!(
                            "`context.retrieval.{other}` is not patchable"
                        )));
                    }
                }
            }
            Ok(())
        }
        Target::Retrieval(RetrievalLeaf::Bm25K) => {
            check_integer("bm25_k", value, MIN_BM25_K, MAX_BM25_K)
        }
        Target::Retrieval(RetrievalLeaf::MaxEvidence) => {
            check_integer("max_evidence", value, MIN_MAX_EVIDENCE, MAX_MAX_EVIDENCE)
        }
    }
}

/// Path whitelist plus traversal guard.
fn parse_path(path: &str) -> Result<Target, SilynError> {
    if !path.starts_with('/') {
        return Err(SilynError::Schema(format!(
            "patch path `{path}` must be a JSON pointer"
        )));
    }
    for segment in path.split('/').skip(1) {
        if segment == ".." || segment == "." {
            return Err(SilynError::PolicyDenied(format!(
                "patch path `{path}` attempts path traversal"
            )));
        }
        if segment.is_empty() {
            return Err(SilynError::Schema(format!(
                "patch path `{path}` has an empty segment"
            )));
        }
        if segment.chars().any(char::is_control) {
            return Err(SilynError::Schema(format!(
                "patch path `{path}` contains control characters"
            )));
        }
    }

    if path == PROMPT_FRAGMENTS_PATH {
        return Ok(Target::PromptFragments(None));
    }
    if path == RULES_PATH {
        return Ok(Target::Rules(None));
    }
    if let Some(rest) = path.strip_prefix(PROMPT_FRAGMENTS_PATH) {
        return Ok(Target::PromptFragments(Some(parse_index(path, rest)?)));
    }
    if let Some(rest) = path.strip_prefix(RULES_PATH) {
        return Ok(Target::Rules(Some(parse_index(path, rest)?)));
    }
    if path == RETRIEVAL_PATH {
        return Ok(Target::Retrieval(RetrievalLeaf::Object));
    }
    if let Some(rest) = path.strip_prefix(RETRIEVAL_PATH) {
        return match rest {
            "/bm25_k" => Ok(Target::Retrieval(RetrievalLeaf::Bm25K)),
            "/max_evidence" => Ok(Target::Retrieval(RetrievalLeaf::MaxEvidence)),
            _ => Err(SilynError::PolicyDenied(format!(
                "patch path `{path}` is not mutable by a candidate"
            ))),
        };
    }

    Err(SilynError::PolicyDenied(format!(
        "patch path `{path}` is not mutable by a candidate"
    )))
}

fn parse_index(path: &str, rest: &str) -> Result<usize, SilynError> {
    let digits = rest.strip_prefix('/').ok_or_else(|| {
        SilynError::PolicyDenied(format!("patch path `{path}` is not mutable by a candidate"))
    })?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(SilynError::PolicyDenied(format!(
            "patch path `{path}` addresses an unknown entry"
        )));
    }
    digits
        .parse::<usize>()
        .map_err(|_| SilynError::Schema(format!("patch path `{path}` has an out-of-range index")))
}

fn apply_list(
    list: &mut Vec<String>,
    index: Option<usize>,
    operation: &PatchOperation,
) -> Result<(), SilynError> {
    match (operation.op, index) {
        (PatchOp::Add, None) | (PatchOp::Replace, None) => {
            let items = string_list(operation.value.as_ref().ok_or_else(|| {
                SilynError::Schema("list replacement requires a value".to_owned())
            })?)?;
            *list = items.iter().map(|item| (*item).to_owned()).collect();
            Ok(())
        }
        (PatchOp::Add, Some(index)) => {
            if index > list.len() {
                return Err(SilynError::Schema(format!(
                    "list index {index} is out of bounds"
                )));
            }
            let text =
                string_value(operation.value.as_ref().ok_or_else(|| {
                    SilynError::Schema("list insert requires a value".to_owned())
                })?)?;
            list.insert(index, text.to_owned());
            Ok(())
        }
        (PatchOp::Replace, Some(index)) => {
            let text =
                string_value(operation.value.as_ref().ok_or_else(|| {
                    SilynError::Schema("list replace requires a value".to_owned())
                })?)?;
            let slot = list.get_mut(index).ok_or_else(|| {
                SilynError::Schema(format!("list index {index} is out of bounds"))
            })?;
            *slot = text.to_owned();
            Ok(())
        }
        (PatchOp::Remove, Some(index)) => {
            if index >= list.len() {
                return Err(SilynError::Schema(format!(
                    "list index {index} is out of bounds"
                )));
            }
            list.remove(index);
            Ok(())
        }
        (PatchOp::Remove, None) => Err(SilynError::Schema(
            "required policy fields cannot be removed".to_owned(),
        )),
    }
}

fn apply_retrieval(
    retrieval: &mut RetrievalPolicy,
    leaf: RetrievalLeaf,
    operation: &PatchOperation,
) -> Result<(), SilynError> {
    let value = operation
        .value
        .as_ref()
        .ok_or_else(|| SilynError::Schema("a retrieval change requires a value".to_owned()))?;

    match leaf {
        RetrievalLeaf::Object => {
            let object = value.as_object().ok_or_else(|| {
                SilynError::Schema("retrieval replacement must be an object".to_owned())
            })?;
            if object.is_empty() {
                return Err(SilynError::Schema(
                    "retrieval replacement must name at least one bound".to_owned(),
                ));
            }
            let mut next = *retrieval;
            for (key, value) in object {
                match key.as_str() {
                    "bm25_k" => next.bm25_k = integer_value(value)?,
                    "max_evidence" => next.max_evidence = integer_value(value)?,
                    other => {
                        return Err(SilynError::PolicyDenied(format!(
                            "`context.retrieval.{other}` is not patchable"
                        )));
                    }
                }
            }
            *retrieval = next;
            Ok(())
        }
        RetrievalLeaf::Bm25K => {
            retrieval.bm25_k = integer_value(value)?;
            Ok(())
        }
        RetrievalLeaf::MaxEvidence => {
            retrieval.max_evidence = integer_value(value)?;
            Ok(())
        }
    }
}

fn string_list(value: &Value) -> Result<Vec<&str>, SilynError> {
    let items = value.as_array().ok_or_else(|| {
        SilynError::Schema("a planner list must be an array of strings".to_owned())
    })?;
    if items.len() > MAX_LIST_ENTRIES {
        return Err(SilynError::Schema(format!(
            "a planner list accepts at most {MAX_LIST_ENTRIES} entries"
        )));
    }
    items
        .iter()
        .map(|item| {
            item.as_str()
                .ok_or_else(|| SilynError::Schema("planner text must be a string".to_owned()))
        })
        .collect()
}

fn string_value(value: &Value) -> Result<&str, SilynError> {
    value
        .as_str()
        .ok_or_else(|| SilynError::Schema("planner text must be a string".to_owned()))
}

fn integer_value(value: &Value) -> Result<u32, SilynError> {
    let number = value.as_u64().ok_or_else(|| {
        SilynError::Schema("retrieval bounds must be non-negative integers".to_owned())
    })?;
    u32::try_from(number)
        .map_err(|_| SilynError::Schema("retrieval bound exceeds its range".to_owned()))
}

fn check_integer(field: &str, value: &Value, min: u32, max: u32) -> Result<(), SilynError> {
    let number = integer_value(value)?;
    if !(min..=max).contains(&number) {
        return Err(SilynError::Schema(format!(
            "{field} must be between {min} and {max}"
        )));
    }
    Ok(())
}

/// Candidate text may not grant capability, move a budget or touch the grader.
fn check_candidate_text(field: &str, text: &str) -> Result<(), SilynError> {
    check_text_value(field, text)?;
    if let Some(phrase) = escalating_phrase(text) {
        return Err(SilynError::PolicyDenied(format!(
            "{field} attempts to widen capability (`{phrase}`)"
        )));
    }
    Ok(())
}

fn escalating_phrase(text: &str) -> Option<&'static str> {
    let lowered = text.to_lowercase();
    for needle in ESCALATION_SUBSTRINGS {
        if lowered.contains(needle) {
            return Some(needle);
        }
    }
    for word in lowered.split(|character: char| !character.is_alphanumeric() && character != '_') {
        if let Some(needle) = ESCALATION_WORDS.iter().find(|needle| **needle == word) {
            return Some(needle);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn retrieval() -> RetrievalPolicy {
        RetrievalPolicy {
            bm25_k: 40,
            max_evidence: 8,
        }
    }

    #[test]
    fn whitelist_rejects_tools_and_traversal() {
        assert!(parse_path("/tools/allowlist").is_err());
        assert!(parse_path("/verification/checkers").is_err());
        assert!(parse_path("/planner/rules/../tools").is_err());
        assert!(parse_path("/context/memory_budget_ratio").is_err());
        assert!(matches!(
            parse_path("/planner/rules/2"),
            Ok(Target::Rules(Some(2)))
        ));
        assert!(matches!(
            parse_path("/context/retrieval/bm25_k"),
            Ok(Target::Retrieval(RetrievalLeaf::Bm25K))
        ));
    }

    #[test]
    fn escalation_words_are_word_boundary_matched() {
        assert!(escalating_phrase("grant fs.read to the planner").is_some());
        assert!(escalating_phrase("call the tool").is_some());
        assert!(escalating_phrase("raise the budget").is_some());
        assert!(escalating_phrase("keep the toolbox tidy").is_none());
        assert!(escalating_phrase("count rows").is_none());
    }

    #[test]
    fn apply_replaces_whole_list_and_updates_retrieval() {
        let base = PolicyArtifact {
            schema_version: 1,
            context: crate::policy::ContextPolicy {
                memory_budget_ratio: 0.35,
                retrieval: retrieval(),
            },
            planner: crate::policy::PlannerPolicy {
                prompt_fragments: vec!["keep me".to_owned()],
                rules: vec!["old rule".to_owned()],
                stop_conditions: serde_json::json!({}),
            },
            tools: crate::policy::ToolPolicy {
                allowlist: vec!["fs.read".to_owned()],
            },
            verification: crate::policy::PolicyVerification {
                checkers: vec!["csv-stats-oracle".to_owned()],
            },
        };

        let patched = apply_patch(
            &base,
            &[
                PatchOperation {
                    op: PatchOp::Replace,
                    path: "/planner/rules".to_owned(),
                    value: Some(serde_json::json!(["new rule"])),
                },
                PatchOperation {
                    op: PatchOp::Add,
                    path: "/context/retrieval/bm25_k".to_owned(),
                    value: Some(serde_json::json!(60)),
                },
            ],
        )
        .expect("patch applies");

        assert_eq!(patched.planner.rules, vec!["new rule".to_owned()]);
        assert_eq!(patched.context.retrieval.bm25_k, 60);
        assert_eq!(
            patched.planner.prompt_fragments,
            base.planner.prompt_fragments
        );

        // Out-of-bounds values are refused even when the raw operation is
        // well-formed, because the result is validated again.
        assert!(
            apply_patch(
                &base,
                &[PatchOperation {
                    op: PatchOp::Replace,
                    path: "/context/retrieval/max_evidence".to_owned(),
                    value: Some(serde_json::json!(51)),
                }]
            )
            .is_err()
        );
    }
}
