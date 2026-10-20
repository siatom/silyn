//! Immutable policy artifacts, canonical checksums and the version store.
//!
//! A policy version is content addressed: the sha256 of the canonical compact
//! JSON is stored next to the row and `policy_versions` is write-once (a
//! trigger rejects `UPDATE` and `DELETE`). Candidates never write here; only a
//! promotion creates a new version from a patched artifact.
//!
//! The canonical form sorts every object key and serializes without whitespace,
//! so two equal artifacts always produce the same checksum, independent of the
//! order in which they were built.

use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use silyn_core::SilynError;
use silyn_core::ids::{AgentId, PolicyVersionId};
use silyn_storage::db_error;
use silyn_storage::error::parse_id;
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqliteConnection, SqlitePool};

/// Schema version of [`PolicyArtifact`]. Bump only with a new schema.
pub const POLICY_SCHEMA_VERSION: u32 = 1;
/// Upper bound for `context.memory_budget_ratio` (ADR 0003).
pub const MAX_MEMORY_BUDGET_RATIO: f64 = 0.6;
/// Lower bound for `context.retrieval.bm25_k`.
pub const MIN_BM25_K: u32 = 1;
/// Upper bound for `context.retrieval.bm25_k`.
pub const MAX_BM25_K: u32 = 200;
/// Lower bound for `context.retrieval.max_evidence`.
pub const MIN_MAX_EVIDENCE: u32 = 1;
/// Upper bound for `context.retrieval.max_evidence`.
pub const MAX_MAX_EVIDENCE: u32 = 50;
/// Entries allowed in one prompt, rule, allowlist or checker list.
pub const MAX_LIST_ENTRIES: usize = 100;
/// Byte ceiling for one text value. Mirrors the patch schema guard so an
/// artifact can never hold text a candidate could not have written.
pub const MAX_TEXT_BYTES: usize = 4000;

/// One immutable policy artifact (ADR 0003).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyArtifact {
    pub schema_version: u32,
    pub context: ContextPolicy,
    pub planner: PlannerPolicy,
    pub tools: ToolPolicy,
    pub verification: PolicyVerification,
}

/// Context assembly settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextPolicy {
    pub memory_budget_ratio: f64,
    pub retrieval: RetrievalPolicy,
}

/// Retrieval bounds. Both integers are bounded because they drive cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetrievalPolicy {
    pub bm25_k: u32,
    pub max_evidence: u32,
}

/// Planner text and stop conditions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannerPolicy {
    pub prompt_fragments: Vec<String>,
    pub rules: Vec<String>,
    #[serde(default = "empty_object")]
    pub stop_conditions: serde_json::Value,
}

/// Tool allowlist. A candidate can never widen it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolPolicy {
    pub allowlist: Vec<String>,
}

/// Verification configuration.
///
/// Named `PolicyVerification` because the verifier already exports
/// [`crate::verifier::VerificationPolicy`], which is a different concept
/// (how checker results combine, not which checkers exist).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyVerification {
    pub checkers: Vec<String>,
}

/// One stored policy version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyVersionRecord {
    pub id: PolicyVersionId,
    pub agent_id: AgentId,
    pub parent_version: Option<PolicyVersionId>,
    pub content: PolicyArtifact,
    pub checksum: String,
    pub created_at: DateTime<Utc>,
}

impl PolicyArtifact {
    /// Rejects an artifact the schema or ADR 0003 would not allow.
    pub fn validate(&self) -> Result<(), SilynError> {
        if self.schema_version != POLICY_SCHEMA_VERSION {
            return Err(SilynError::Schema(format!(
                "policy schema_version must be {POLICY_SCHEMA_VERSION}"
            )));
        }

        let ratio = self.context.memory_budget_ratio;
        if !ratio.is_finite() || !(0.0..=MAX_MEMORY_BUDGET_RATIO).contains(&ratio) {
            return Err(SilynError::Schema(format!(
                "memory_budget_ratio must be between 0 and {MAX_MEMORY_BUDGET_RATIO}"
            )));
        }
        let retrieval = self.context.retrieval;
        if !(MIN_BM25_K..=MAX_BM25_K).contains(&retrieval.bm25_k) {
            return Err(SilynError::Schema(format!(
                "bm25_k must be between {MIN_BM25_K} and {MAX_BM25_K}"
            )));
        }
        if !(MIN_MAX_EVIDENCE..=MAX_MAX_EVIDENCE).contains(&retrieval.max_evidence) {
            return Err(SilynError::Schema(format!(
                "max_evidence must be between {MIN_MAX_EVIDENCE} and {MAX_MAX_EVIDENCE}"
            )));
        }

        if !self.planner.stop_conditions.is_object() {
            return Err(SilynError::Schema(
                "planner.stop_conditions must be an object".to_owned(),
            ));
        }
        check_text_list("planner.prompt_fragments", &self.planner.prompt_fragments)?;
        check_text_list("planner.rules", &self.planner.rules)?;
        check_text_list("tools.allowlist", &self.tools.allowlist)?;
        if self.tools.allowlist.is_empty() {
            return Err(SilynError::Schema(
                "tools.allowlist must not be empty".to_owned(),
            ));
        }
        check_text_list("verification.checkers", &self.verification.checkers)?;
        if self.verification.checkers.is_empty() {
            return Err(SilynError::Schema(
                "verification.checkers must not be empty".to_owned(),
            ));
        }
        Ok(())
    }

    /// Canonical compact JSON: sorted keys, no whitespace.
    pub fn canonical_json(&self) -> Result<String, SilynError> {
        canonical_json_string(self)
    }

    /// sha256 of the canonical JSON, lowercase hex.
    pub fn checksum(&self) -> Result<String, SilynError> {
        Ok(sha256_hex(self.canonical_json()?.as_bytes()))
    }
}

/// Policy version reads and writes for one SQLite pool.
pub struct PolicyStore<'a> {
    pool: &'a SqlitePool,
}

impl<'a> PolicyStore<'a> {
    #[must_use]
    pub fn new(pool: &'a SqlitePool) -> Self {
        Self { pool }
    }

    /// Stores a new immutable version.
    ///
    /// Identical content for the same agent is idempotent and returns the
    /// existing row; identical content that already belongs to another agent is
    /// a permanent error because `policy_versions.checksum` is globally unique.
    pub async fn create(
        &self,
        agent_id: AgentId,
        parent: Option<PolicyVersionId>,
        policy: &PolicyArtifact,
    ) -> Result<PolicyVersionRecord, SilynError> {
        let mut transaction = self.pool.begin().await.map_err(db_error)?;
        let record = create_version(&mut transaction, agent_id, parent, policy).await?;
        transaction.commit().await.map_err(db_error)?;
        Ok(record)
    }

    /// Reads one version, scoped to the agent.
    pub async fn get(
        &self,
        agent_id: AgentId,
        id: PolicyVersionId,
    ) -> Result<Option<PolicyVersionRecord>, SilynError> {
        let mut connection = self.pool.acquire().await.map_err(db_error)?;
        load_version(&mut connection, agent_id, id).await
    }

    /// Newest version of the agent, ordered by creation time then id.
    pub async fn latest(
        &self,
        agent_id: AgentId,
    ) -> Result<Option<PolicyVersionRecord>, SilynError> {
        let row = sqlx::query(
            "SELECT id, agent_id, parent_version, content_json, checksum, created_at
             FROM policy_versions WHERE agent_id = ?1
             ORDER BY created_at DESC, id DESC LIMIT 1",
        )
        .bind(agent_id.to_string())
        .fetch_optional(self.pool)
        .await
        .map_err(db_error)?;

        row.as_ref().map(policy_from_row).transpose()
    }

    /// The version `agents.active_policy_version` points at, when set.
    pub async fn active(
        &self,
        agent_id: AgentId,
    ) -> Result<Option<PolicyVersionRecord>, SilynError> {
        let mut connection = self.pool.acquire().await.map_err(db_error)?;
        let Some(id) = active_policy_version(&mut connection, agent_id).await? else {
            return Ok(None);
        };
        load_version(&mut connection, agent_id, id).await
    }
}

/// Inserts one version inside the caller's transaction.
pub(crate) async fn create_version(
    conn: &mut SqliteConnection,
    agent_id: AgentId,
    parent: Option<PolicyVersionId>,
    policy: &PolicyArtifact,
) -> Result<PolicyVersionRecord, SilynError> {
    policy.validate()?;
    let content_json = policy.canonical_json()?;
    let checksum = sha256_hex(content_json.as_bytes());

    if let Some(existing) = find_by_checksum(conn, &checksum).await? {
        if existing.agent_id == agent_id {
            return Ok(existing);
        }
        return Err(SilynError::Permanent(format!(
            "an identical policy is already registered as {} for another agent",
            existing.id
        )));
    }

    let agent_exists: Option<String> = sqlx::query_scalar("SELECT id FROM agents WHERE id = ?1")
        .bind(agent_id.to_string())
        .fetch_optional(&mut *conn)
        .await
        .map_err(db_error)?;
    if agent_exists.is_none() {
        return Err(SilynError::Permanent(format!("agent {agent_id} not found")));
    }

    if let Some(parent) = parent {
        let owner: Option<String> =
            sqlx::query_scalar("SELECT agent_id FROM policy_versions WHERE id = ?1")
                .bind(parent.to_string())
                .fetch_optional(&mut *conn)
                .await
                .map_err(db_error)?;
        match owner {
            None => {
                return Err(SilynError::Permanent(format!(
                    "parent policy version {parent} not found"
                )));
            }
            Some(owner) if owner != agent_id.to_string() => {
                return Err(SilynError::PolicyDenied(format!(
                    "parent policy version {parent} belongs to another agent"
                )));
            }
            Some(_) => {}
        }
    }

    let id = PolicyVersionId::new();
    let created_at = Utc::now();
    let result = sqlx::query(
        "INSERT INTO policy_versions (id, agent_id, parent_version, content_json, checksum, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(id.to_string())
    .bind(agent_id.to_string())
    .bind(parent.map(|value| value.to_string()))
    .bind(&content_json)
    .bind(&checksum)
    .bind(created_at)
    .execute(&mut *conn)
    .await;

    if let Err(error) = result {
        if let sqlx::Error::Database(database) = &error
            && database.is_unique_violation()
            && let Some(existing) = find_by_checksum(conn, &checksum).await?
            && existing.agent_id == agent_id
        {
            return Ok(existing);
        }
        return Err(db_error(error));
    }

    Ok(PolicyVersionRecord {
        id,
        agent_id,
        parent_version: parent,
        content: policy.clone(),
        checksum,
        created_at,
    })
}

/// Reads one version inside the caller's transaction or connection.
pub(crate) async fn load_version(
    conn: &mut SqliteConnection,
    agent_id: AgentId,
    id: PolicyVersionId,
) -> Result<Option<PolicyVersionRecord>, SilynError> {
    let row = sqlx::query(
        "SELECT id, agent_id, parent_version, content_json, checksum, created_at
         FROM policy_versions WHERE id = ?1 AND agent_id = ?2",
    )
    .bind(id.to_string())
    .bind(agent_id.to_string())
    .fetch_optional(&mut *conn)
    .await
    .map_err(db_error)?;

    row.as_ref().map(policy_from_row).transpose()
}

/// Current `agents.active_policy_version` pointer.
pub(crate) async fn active_policy_version(
    conn: &mut SqliteConnection,
    agent_id: AgentId,
) -> Result<Option<PolicyVersionId>, SilynError> {
    let value: Option<String> =
        sqlx::query_scalar("SELECT active_policy_version FROM agents WHERE id = ?1")
            .bind(agent_id.to_string())
            .fetch_optional(&mut *conn)
            .await
            .map_err(db_error)?
            .flatten();

    value
        .map(|value| parse_id(&value, "agents.active_policy_version"))
        .transpose()
}

/// Moves the agent's active pointer. Callers own the transaction.
pub(crate) async fn set_active_policy_version(
    conn: &mut SqliteConnection,
    agent_id: AgentId,
    version: PolicyVersionId,
) -> Result<(), SilynError> {
    let updated = sqlx::query("UPDATE agents SET active_policy_version = ?2 WHERE id = ?1")
        .bind(agent_id.to_string())
        .bind(version.to_string())
        .execute(&mut *conn)
        .await
        .map_err(db_error)?;

    if updated.rows_affected() == 0 {
        return Err(SilynError::Permanent(format!("agent {agent_id} not found")));
    }
    Ok(())
}

async fn find_by_checksum(
    conn: &mut SqliteConnection,
    checksum: &str,
) -> Result<Option<PolicyVersionRecord>, SilynError> {
    let row = sqlx::query(
        "SELECT id, agent_id, parent_version, content_json, checksum, created_at
         FROM policy_versions WHERE checksum = ?1",
    )
    .bind(checksum)
    .fetch_optional(&mut *conn)
    .await
    .map_err(db_error)?;

    row.as_ref().map(policy_from_row).transpose()
}

fn policy_from_row(row: &SqliteRow) -> Result<PolicyVersionRecord, SilynError> {
    let content_json: String = row.try_get("content_json").map_err(db_error)?;
    let content: PolicyArtifact = serde_json::from_str(&content_json)?;
    // Recompute instead of trusting the column: a mismatch means the row was
    // written by something other than `create_version`.
    let checksum: String = row.try_get("checksum").map_err(db_error)?;
    if content.checksum()? != checksum {
        return Err(SilynError::Permanent(format!(
            "stored policy content does not match its checksum at {}",
            row.try_get::<String, _>("id").map_err(db_error)?
        )));
    }

    Ok(PolicyVersionRecord {
        id: parse_id(
            &row.try_get::<String, _>("id").map_err(db_error)?,
            "policy_versions.id",
        )?,
        agent_id: parse_id(
            &row.try_get::<String, _>("agent_id").map_err(db_error)?,
            "policy_versions.agent_id",
        )?,
        parent_version: row
            .try_get::<Option<String>, _>("parent_version")
            .map_err(db_error)?
            .map(|value| parse_id(&value, "policy_versions.parent_version"))
            .transpose()?,
        content,
        checksum,
        created_at: row.try_get("created_at").map_err(db_error)?,
    })
}

fn empty_object() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

fn check_text_list(field: &str, values: &[String]) -> Result<(), SilynError> {
    if values.len() > MAX_LIST_ENTRIES {
        return Err(SilynError::Schema(format!(
            "{field} must hold at most {MAX_LIST_ENTRIES} entries"
        )));
    }
    let mut seen = std::collections::BTreeSet::new();
    for value in values {
        check_text_value(field, value)?;
        if !seen.insert(value.as_str()) {
            return Err(SilynError::Schema(format!(
                "{field} contains the duplicate entry `{value}`"
            )));
        }
    }
    Ok(())
}

/// Shared text guard: non-empty after trimming, bounded bytes, no control
/// characters other than tab/newline/carriage return.
pub(crate) fn check_text_value(field: &str, text: &str) -> Result<(), SilynError> {
    if text.trim().is_empty() {
        return Err(SilynError::Schema(format!("{field} must not be empty")));
    }
    if text.len() > MAX_TEXT_BYTES {
        return Err(SilynError::Schema(format!(
            "{field} exceeds {MAX_TEXT_BYTES} bytes"
        )));
    }
    if text
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(SilynError::Schema(format!(
            "{field} contains control characters"
        )));
    }
    Ok(())
}

/// Canonical compact JSON for any serializable value.
pub(crate) fn canonical_json_string<T: Serialize>(value: &T) -> Result<String, SilynError> {
    let value = serde_json::to_value(value)?;
    serde_json::to_string(&canonicalize(value)).map_err(Into::into)
}

fn canonicalize(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(String, serde_json::Value)> = map.into_iter().collect();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            let mut sorted = serde_json::Map::with_capacity(entries.len());
            for (key, value) in entries {
                sorted.insert(key, canonicalize(value));
            }
            serde_json::Value::Object(sorted)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(canonicalize).collect())
        }
        other => other,
    }
}

/// Lowercase hex sha256.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Enum values are stored as their serde snake_case string.
pub(crate) fn encode_enum<T: Serialize>(value: &T) -> Result<String, SilynError> {
    let encoded = serde_json::to_value(value)?;
    encoded
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| SilynError::Schema("enum did not encode as a string".to_owned()))
}

/// Inverse of [`encode_enum`].
pub(crate) fn decode_enum<T: DeserializeOwned>(value: &str) -> Result<T, SilynError> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PolicyArtifact {
        PolicyArtifact {
            schema_version: POLICY_SCHEMA_VERSION,
            context: ContextPolicy {
                memory_budget_ratio: 0.35,
                retrieval: RetrievalPolicy {
                    bm25_k: 40,
                    max_evidence: 8,
                },
            },
            planner: PlannerPolicy {
                prompt_fragments: vec!["Summarise the data first.".to_owned()],
                rules: vec!["Recompute row counts.".to_owned()],
                stop_conditions: serde_json::json!({}),
            },
            tools: ToolPolicy {
                allowlist: vec!["fs.read".to_owned()],
            },
            verification: PolicyVerification {
                checkers: vec!["csv-stats-oracle".to_owned()],
            },
        }
    }

    #[test]
    fn canonical_json_is_compact_and_sorted() {
        let artifact = sample();
        let json = artifact.canonical_json().expect("canonical json");
        assert!(
            !json.contains("\": ") && !json.contains(", "),
            "canonical json must be compact"
        );
        assert!(
            json.starts_with("{\"context\":{"),
            "keys are sorted: {json}"
        );
        assert_eq!(artifact.checksum().expect("checksum").len(), 64);
    }

    #[test]
    fn validate_rejects_out_of_range_retrieval() {
        let mut artifact = sample();
        artifact.context.retrieval.bm25_k = 0;
        assert!(artifact.validate().is_err());
        artifact.context.retrieval.bm25_k = 201;
        assert!(artifact.validate().is_err());
        artifact.context.retrieval.bm25_k = 40;
        artifact.context.memory_budget_ratio = 0.61;
        assert!(artifact.validate().is_err());
        artifact.context.memory_budget_ratio = 0.35;
        artifact.tools.allowlist.clear();
        assert!(artifact.validate().is_err());
    }
}
