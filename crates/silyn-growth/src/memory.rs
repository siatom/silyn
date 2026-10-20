//! Persistent memory records with owner/agent isolation, lifecycle transitions
//! and FTS5 retrieval.
//!
//! Isolation rule: every read takes both `owner_user_id` and `agent_id` from
//! the caller and filters on both. A mismatch yields no row, never a broader
//! scan. Relations and lifecycle writes additionally refuse to touch rows that
//! do not belong to one owner/agent pair.
//!
//! Trust rule (ADR 0005): provenance is stored verbatim. Model inference is not
//! more trusted than tool output, and a retrieval hit carries its provenance so
//! the caller can rank or reject it.

use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use silyn_core::SilynError;
use silyn_core::contracts::{MemoryScope, MemoryType};
use silyn_core::ids::{AgentId, MemoryId};
use silyn_storage::db_error;
use silyn_storage::error::parse_id;
use sqlx::sqlite::SqliteRow;
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};

use crate::content::insert_blob;

/// Lifecycle status of a memory row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    Active,
    Superseded,
    Expired,
    Deleted,
}

/// Where the memory text came from. Never used to widen authorization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    UserStatement,
    ExternalEvidence,
    ModelInference,
    ToolOutput,
}

/// Directed edge between two memories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationType {
    Supports,
    Contradicts,
    Supersedes,
}

/// A memory to persist. The store generates the id and timestamps.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// Retrieval scope. Both the owner and the agent are mandatory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryScopeFilter {
    pub owner_user_id: String,
    pub agent_id: AgentId,
    /// Empty means `agent` + `session` only; `shared` must be asked for
    /// explicitly.
    pub scopes: Vec<MemoryScope>,
    /// Empty means every type.
    pub types: Vec<MemoryType>,
    /// When true the validity window is ignored for *active* memories. Expired
    /// and deleted rows stay invisible because the status filter is separate.
    pub include_expired: bool,
}

/// A stored memory, with its payload resolved from the blob table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub id: MemoryId,
    pub owner_user_id: String,
    pub agent_id: AgentId,
    pub scope: MemoryScope,
    pub memory_type: MemoryType,
    pub content_ref: String,
    pub content: String,
    pub summary: String,
    pub source_event_ids: Vec<String>,
    pub provenance: Provenance,
    pub confidence: f32,
    pub importance: f32,
    pub validity_from: Option<DateTime<Utc>>,
    pub validity_to: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub status: MemoryStatus,
    pub supersedes_id: Option<MemoryId>,
    pub sensitivity: String,
    pub embedding_version: Option<String>,
}

/// One retrieval hit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryHitRecord {
    pub id: MemoryId,
    pub summary: String,
    pub content: String,
    pub provenance: Provenance,
    pub confidence: f32,
    pub importance: f32,
    pub status: MemoryStatus,
    /// Relevance in `-bm25` form: higher is better. `0.0` for the
    /// empty-query listing, which is ordered by recency instead.
    pub score: f32,
}

/// One stored relation edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRelation {
    pub from_id: MemoryId,
    pub to_id: MemoryId,
    pub relation_type: RelationType,
    pub created_at: DateTime<Utc>,
}

/// What a purge removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurgeReport {
    pub memory_id: MemoryId,
    pub blob_removed: bool,
    pub relations_removed: u64,
    pub sources_removed: u64,
}

/// Upper bound on one retrieval page, so a caller mistake cannot scan the
/// whole table.
const MAX_HITS: usize = 200;

const RECORD_COLUMNS: &str = "m.id, m.owner_user_id, m.agent_id, m.scope, m.type, \
     m.content_ref, b.content, m.summary, m.source_event_ids, m.provenance, \
     m.confidence, m.importance, m.validity_from, m.validity_to, m.last_used_at, \
     m.created_at, m.updated_at, m.status, m.supersedes_id, m.sensitivity, \
     m.embedding_version";

const RECORD_FROM: &str = " FROM memories m JOIN content_blobs b ON b.id = m.content_ref";

/// Memory reads and lifecycle writes for one SQLite pool.
pub struct MemoryStore<'a> {
    pool: &'a SqlitePool,
}

impl<'a> MemoryStore<'a> {
    #[must_use]
    pub fn new(pool: &'a SqlitePool) -> Self {
        Self { pool }
    }

    /// Validates, then writes the blob, memory row, source rows and FTS row in
    /// one transaction.
    pub async fn put(&self, new: NewMemory) -> Result<MemoryRecord, SilynError> {
        validate_new(&new)?;
        let NewMemory {
            owner_user_id,
            agent_id,
            scope,
            memory_type,
            content,
            summary,
            source_event_ids,
            provenance,
            confidence,
            importance,
            validity_from,
            validity_to,
            sensitivity,
        } = new;

        let mut transaction = self.pool.begin().await.map_err(db_error)?;

        let agent_owner: Option<String> =
            sqlx::query_scalar("SELECT owner_id FROM agents WHERE id = ?1")
                .bind(agent_id.to_string())
                .fetch_optional(&mut *transaction)
                .await
                .map_err(db_error)?;
        match agent_owner {
            None => {
                return Err(SilynError::Permanent(format!("agent {agent_id} not found")));
            }
            Some(owner) if owner != owner_user_id => {
                return Err(SilynError::PolicyDenied(format!(
                    "agent {agent_id} is not owned by {owner_user_id}"
                )));
            }
            Some(_) => {}
        }

        let now = Utc::now();
        let content_ref = insert_blob(&mut *transaction, &content, now).await?;
        let id = MemoryId::new();
        let source_json = serde_json::to_string(&source_event_ids)?;

        sqlx::query(
            "INSERT INTO memories (
                id, owner_user_id, agent_id, scope, type, content_ref, summary,
                source_event_ids, provenance, confidence, importance, validity_from,
                validity_to, last_used_at, created_at, updated_at, status,
                supersedes_id, sensitivity, embedding_version
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                ?16, ?17, ?18, ?19, ?20
             )",
        )
        .bind(id.to_string())
        .bind(&owner_user_id)
        .bind(agent_id.to_string())
        .bind(encode_enum(&scope)?)
        .bind(encode_enum(&memory_type)?)
        .bind(&content_ref)
        .bind(&summary)
        .bind(&source_json)
        .bind(encode_enum(&provenance)?)
        .bind(f64::from(confidence))
        .bind(f64::from(importance))
        .bind(validity_from)
        .bind(validity_to)
        .bind(Option::<DateTime<Utc>>::None)
        .bind(now)
        .bind(now)
        .bind(encode_enum(&MemoryStatus::Active)?)
        .bind(Option::<String>::None)
        .bind(&sensitivity)
        .bind(Option::<String>::None)
        .execute(&mut *transaction)
        .await
        .map_err(db_error)?;

        for source in &source_event_ids {
            sqlx::query(
                "INSERT INTO memory_sources (memory_id, source_type, source_ref, created_at)
                 VALUES (?1, 'event', ?2, ?3)
                 ON CONFLICT DO NOTHING",
            )
            .bind(id.to_string())
            .bind(source)
            .bind(now)
            .execute(&mut *transaction)
            .await
            .map_err(db_error)?;
        }

        sqlx::query("INSERT INTO memory_fts (memory_id, summary, content) VALUES (?1, ?2, ?3)")
            .bind(id.to_string())
            .bind(&summary)
            .bind(&content)
            .execute(&mut *transaction)
            .await
            .map_err(db_error)?;

        transaction.commit().await.map_err(db_error)?;

        Ok(MemoryRecord {
            id,
            owner_user_id,
            agent_id,
            scope,
            memory_type,
            content_ref,
            content,
            summary,
            source_event_ids,
            provenance,
            confidence,
            importance,
            validity_from,
            validity_to,
            last_used_at: None,
            created_at: now,
            updated_at: now,
            status: MemoryStatus::Active,
            supersedes_id: None,
            sensitivity,
            embedding_version: None,
        })
    }

    /// Reads one memory. Owner and agent must both match.
    ///
    /// `deleted` rows stay in storage for audit but are not visible through the
    /// store API; only [`Self::purge`] removes them.
    pub async fn get(
        &self,
        owner: &str,
        agent: AgentId,
        id: MemoryId,
    ) -> Result<Option<MemoryRecord>, SilynError> {
        let sql = format!(
            "SELECT {RECORD_COLUMNS}{RECORD_FROM} WHERE m.id = ?1 AND m.owner_user_id = ?2 AND m.agent_id = ?3 AND m.status <> 'deleted'"
        );
        let row = sqlx::query(&sql)
            .bind(id.to_string())
            .bind(owner)
            .bind(agent.to_string())
            .fetch_optional(self.pool)
            .await
            .map_err(db_error)?;

        row.as_ref().map(record_from_row).transpose()
    }

    /// Full-text retrieval over active memories.
    ///
    /// User text is split into tokens and quoted, so FTS5 operators and
    /// special characters in a query are treated as literal terms instead of
    /// syntax. An empty (or punctuation-only) query lists the newest active
    /// memories instead.
    pub async fn search(
        &self,
        filter: &MemoryScopeFilter,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MemoryHitRecord>, SilynError> {
        let now = Utc::now();
        let limit = i64::try_from(limit.clamp(1, MAX_HITS))
            .map_err(|_| SilynError::Schema("retrieval limit is out of range".to_owned()))?;
        let match_query = fts_terms(query);

        let mut builder = QueryBuilder::<Sqlite>::new(
            "SELECT m.id, m.summary, b.content, m.provenance, m.confidence, \
             m.importance, m.status, ",
        );
        if let Some(match_query) = &match_query {
            builder.push(
                "bm25(memory_fts) AS score FROM memory_fts \
                 JOIN memories m ON m.id = memory_fts.memory_id \
                 JOIN content_blobs b ON b.id = m.content_ref \
                 WHERE memory_fts MATCH ",
            );
            builder.push_bind(match_query);
            builder.push(" AND ");
        } else {
            builder.push(
                "0.0 AS score FROM memories m \
                 JOIN content_blobs b ON b.id = m.content_ref WHERE ",
            );
        }

        builder
            .push("m.owner_user_id = ")
            .push_bind(filter.owner_user_id.clone());
        builder
            .push(" AND m.agent_id = ")
            .push_bind(filter.agent_id.to_string());
        builder.push(" AND m.status = 'active'");
        builder
            .push(" AND (m.validity_from IS NULL OR m.validity_from <= ")
            .push_bind(now)
            .push(")");
        if !filter.include_expired {
            builder
                .push(" AND (m.validity_to IS NULL OR m.validity_to > ")
                .push_bind(now)
                .push(")");
        }

        let scopes = if filter.scopes.is_empty() {
            vec![MemoryScope::Agent, MemoryScope::Session]
        } else {
            filter.scopes.clone()
        };
        builder.push(" AND m.scope IN (");
        for (index, scope) in scopes.iter().enumerate() {
            if index > 0 {
                builder.push(", ");
            }
            builder.push_bind(encode_enum(scope)?);
        }
        builder.push(")");

        if !filter.types.is_empty() {
            builder.push(" AND m.type IN (");
            for (index, memory_type) in filter.types.iter().enumerate() {
                if index > 0 {
                    builder.push(", ");
                }
                builder.push_bind(encode_enum(memory_type)?);
            }
            builder.push(")");
        }

        if match_query.is_some() {
            builder.push(" ORDER BY score ASC, m.importance DESC, m.created_at DESC");
        } else {
            builder.push(" ORDER BY m.created_at DESC, m.importance DESC");
        }
        builder.push(" LIMIT ").push_bind(limit);

        let rows = builder
            .build()
            .fetch_all(self.pool)
            .await
            .map_err(db_error)?;
        rows.iter().map(hit_from_row).collect()
    }

    /// Inserts a relation edge. Idempotent for an identical edge.
    ///
    /// Both memories must belong to the same owner and agent. `contradicts`
    /// records the conflict and never changes either status or summary.
    pub async fn relate(
        &self,
        from: MemoryId,
        to: MemoryId,
        relation: RelationType,
    ) -> Result<(), SilynError> {
        if from == to {
            return Err(SilynError::Permanent(
                "a memory cannot relate to itself".to_owned(),
            ));
        }

        let mut transaction = self.pool.begin().await.map_err(db_error)?;
        memory_pair(&mut transaction, from, to).await?;

        sqlx::query(
            "INSERT INTO memory_relations (from_id, to_id, relation_type, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT DO NOTHING",
        )
        .bind(from.to_string())
        .bind(to.to_string())
        .bind(encode_enum(&relation)?)
        .bind(Utc::now())
        .execute(&mut *transaction)
        .await
        .map_err(db_error)?;

        transaction.commit().await.map_err(db_error)?;
        Ok(())
    }

    /// Marks `old` superseded and points `new` at it, transactionally.
    ///
    /// The old row, blob, sources and FTS entry stay in place: supersession is
    /// history, not deletion. The relation is stored as
    /// `from = new, to = old, type = supersedes`.
    pub async fn supersede(&self, old: MemoryId, new: MemoryId) -> Result<(), SilynError> {
        if old == new {
            return Err(SilynError::Permanent(
                "a memory cannot supersede itself".to_owned(),
            ));
        }

        let mut transaction = self.pool.begin().await.map_err(db_error)?;
        memory_pair(&mut transaction, old, new).await?;
        let now = Utc::now();

        let updated = sqlx::query(
            "UPDATE memories SET status = ?2, updated_at = ?3 WHERE id = ?1 AND status <> 'deleted'",
        )
        .bind(old.to_string())
        .bind(encode_enum(&MemoryStatus::Superseded)?)
        .bind(now)
        .execute(&mut *transaction)
        .await
        .map_err(db_error)?;
        if updated.rows_affected() == 0 {
            return Err(SilynError::Permanent(format!(
                "memory {old} is missing or was deleted"
            )));
        }

        let updated = sqlx::query(
            "UPDATE memories SET supersedes_id = ?2, updated_at = ?3 WHERE id = ?1 AND status <> 'deleted'",
        )
        .bind(new.to_string())
        .bind(old.to_string())
        .bind(now)
        .execute(&mut *transaction)
        .await
        .map_err(db_error)?;
        if updated.rows_affected() == 0 {
            return Err(SilynError::Permanent(format!(
                "memory {new} is missing or was deleted"
            )));
        }

        sqlx::query(
            "INSERT INTO memory_relations (from_id, to_id, relation_type, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT DO NOTHING",
        )
        .bind(new.to_string())
        .bind(old.to_string())
        .bind(encode_enum(&RelationType::Supersedes)?)
        .bind(now)
        .execute(&mut *transaction)
        .await
        .map_err(db_error)?;

        transaction.commit().await.map_err(db_error)?;
        Ok(())
    }

    /// Closes a memory: status `expired` and `validity_to` set to `at`.
    pub async fn expire(&self, id: MemoryId, at: DateTime<Utc>) -> Result<(), SilynError> {
        let updated = sqlx::query(
            "UPDATE memories SET status = ?2, validity_to = ?3, updated_at = ?4
             WHERE id = ?1 AND status <> 'deleted'",
        )
        .bind(id.to_string())
        .bind(encode_enum(&MemoryStatus::Expired)?)
        .bind(at)
        .bind(Utc::now())
        .execute(self.pool)
        .await
        .map_err(db_error)?;

        if updated.rows_affected() == 0 {
            return Err(SilynError::Permanent(format!(
                "memory {id} is missing or was deleted"
            )));
        }
        Ok(())
    }

    /// Records that an active memory was used at `at`.
    pub async fn touch(&self, id: MemoryId, at: DateTime<Utc>) -> Result<(), SilynError> {
        let updated = sqlx::query(
            "UPDATE memories SET last_used_at = ?2 WHERE id = ?1 AND status = 'active'",
        )
        .bind(id.to_string())
        .bind(at)
        .execute(self.pool)
        .await
        .map_err(db_error)?;

        if updated.rows_affected() == 0 {
            return Err(SilynError::Permanent(format!(
                "memory {id} is missing or not active"
            )));
        }
        Ok(())
    }

    /// Hides a memory from retrieval while keeping the payload for audit.
    ///
    /// The blob and the FTS row survive until [`Self::purge`]. The soft delete
    /// is audited because ADR 0005 requires deletions to name an actor.
    pub async fn soft_delete(&self, id: MemoryId, actor: &str) -> Result<(), SilynError> {
        let now = Utc::now();
        let mut transaction = self.pool.begin().await.map_err(db_error)?;

        let updated = sqlx::query(
            "UPDATE memories SET status = ?2, updated_at = ?3 WHERE id = ?1 AND status <> 'deleted'",
        )
        .bind(id.to_string())
        .bind(encode_enum(&MemoryStatus::Deleted)?)
        .bind(now)
        .execute(&mut *transaction)
        .await
        .map_err(db_error)?;
        if updated.rows_affected() == 0 {
            return Err(SilynError::Permanent(format!(
                "memory {id} is missing or already deleted"
            )));
        }

        audit(&mut transaction, actor, "memory.soft_delete", id, now).await?;
        transaction.commit().await.map_err(db_error)?;
        Ok(())
    }

    /// Hard-deletes a memory and everything derived from it.
    ///
    /// Removes the row, its FTS entry, sources and relations, then drops the
    /// blob when no memory references it any more. The purge itself is audited
    /// with the actor, the subject and decision `allow`.
    pub async fn purge(&self, id: MemoryId, actor: &str) -> Result<PurgeReport, SilynError> {
        let mut transaction = self.pool.begin().await.map_err(db_error)?;

        let content_ref: Option<String> =
            sqlx::query_scalar("SELECT content_ref FROM memories WHERE id = ?1")
                .bind(id.to_string())
                .fetch_optional(&mut *transaction)
                .await
                .map_err(db_error)?;
        let content_ref =
            content_ref.ok_or_else(|| SilynError::Permanent(format!("memory {id} not found")))?;

        sqlx::query("DELETE FROM memory_fts WHERE memory_id = ?1")
            .bind(id.to_string())
            .execute(&mut *transaction)
            .await
            .map_err(db_error)?;

        let sources_removed = sqlx::query("DELETE FROM memory_sources WHERE memory_id = ?1")
            .bind(id.to_string())
            .execute(&mut *transaction)
            .await
            .map_err(db_error)?
            .rows_affected();

        let relations_removed =
            sqlx::query("DELETE FROM memory_relations WHERE from_id = ?1 OR to_id = ?1")
                .bind(id.to_string())
                .execute(&mut *transaction)
                .await
                .map_err(db_error)?
                .rows_affected();

        sqlx::query("DELETE FROM memories WHERE id = ?1")
            .bind(id.to_string())
            .execute(&mut *transaction)
            .await
            .map_err(db_error)?;

        let remaining: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM memories WHERE content_ref = ?1")
                .bind(&content_ref)
                .fetch_one(&mut *transaction)
                .await
                .map_err(db_error)?;

        let blob_removed = if remaining == 0 {
            let removed = sqlx::query("DELETE FROM content_blobs WHERE id = ?1")
                .bind(&content_ref)
                .execute(&mut *transaction)
                .await
                .map_err(db_error)?
                .rows_affected();
            removed > 0
        } else {
            false
        };

        audit(&mut transaction, actor, "memory.purge", id, Utc::now()).await?;
        transaction.commit().await.map_err(db_error)?;

        Ok(PurgeReport {
            memory_id: id,
            blob_removed,
            relations_removed,
            sources_removed,
        })
    }

    /// Relations touching this memory, restricted to one owner/agent pair.
    pub async fn relations_for(
        &self,
        owner: &str,
        agent: AgentId,
        id: MemoryId,
    ) -> Result<Vec<MemoryRelation>, SilynError> {
        let rows = sqlx::query(
            "SELECT r.from_id, r.to_id, r.relation_type, r.created_at
             FROM memory_relations r
             JOIN memories source ON source.id = r.from_id
             JOIN memories target ON target.id = r.to_id
             WHERE (r.from_id = ?1 OR r.to_id = ?1)
               AND source.owner_user_id = ?2 AND source.agent_id = ?3
               AND target.owner_user_id = ?2 AND target.agent_id = ?3
             ORDER BY r.created_at ASC, r.from_id ASC, r.relation_type ASC",
        )
        .bind(id.to_string())
        .bind(owner)
        .bind(agent.to_string())
        .fetch_all(self.pool)
        .await
        .map_err(db_error)?;

        rows.iter()
            .map(|row| {
                Ok(MemoryRelation {
                    from_id: parse_id(&text(row, "from_id")?, "memory_relations.from_id")?,
                    to_id: parse_id(&text(row, "to_id")?, "memory_relations.to_id")?,
                    relation_type: decode_enum(&text(row, "relation_type")?)?,
                    created_at: timestamp(row, "created_at")?,
                })
            })
            .collect()
    }
}

/// Asserts that two memories exist and belong to one owner/agent pair.
///
/// Relation writes have no principal argument, so this is the only guard
/// against stitching one agent's memory onto another's.
async fn memory_pair(
    transaction: &mut sqlx::Transaction<'_, Sqlite>,
    first: MemoryId,
    second: MemoryId,
) -> Result<(String, String), SilynError> {
    let rows = sqlx::query("SELECT id, owner_user_id, agent_id FROM memories WHERE id IN (?1, ?2)")
        .bind(first.to_string())
        .bind(second.to_string())
        .fetch_all(&mut **transaction)
        .await
        .map_err(db_error)?;

    if rows.len() != 2 {
        return Err(SilynError::Permanent(format!(
            "memory {first} or {second} not found"
        )));
    }

    let mut pair: Option<(String, String)> = None;
    for row in &rows {
        let candidate = (text(row, "owner_user_id")?, text(row, "agent_id")?);
        match &pair {
            None => pair = Some(candidate),
            Some(expected) if expected == &candidate => {}
            Some(_) => {
                return Err(SilynError::PolicyDenied(format!(
                    "memories {first} and {second} belong to different owners or agents"
                )));
            }
        }
    }

    pair.ok_or_else(|| SilynError::Permanent("memory pair is empty".to_owned()))
}

async fn audit(
    transaction: &mut sqlx::Transaction<'_, Sqlite>,
    actor: &str,
    action: &str,
    subject: MemoryId,
    occurred_at: DateTime<Utc>,
) -> Result<(), SilynError> {
    sqlx::query(
        "INSERT INTO audit_events (occurred_at, actor, action, subject, decision)
         VALUES (?1, ?2, ?3, ?4, 'allow')",
    )
    .bind(occurred_at)
    .bind(actor)
    .bind(action)
    .bind(subject.to_string())
    .execute(&mut **transaction)
    .await
    .map_err(db_error)?;
    Ok(())
}

fn validate_new(new: &NewMemory) -> Result<(), SilynError> {
    if new.summary.trim().is_empty() {
        return Err(SilynError::Schema(
            "memory summary must not be empty".to_owned(),
        ));
    }
    if new.content.trim().is_empty() {
        return Err(SilynError::Schema(
            "memory content must not be empty".to_owned(),
        ));
    }
    for (value, field) in [
        (new.confidence, "confidence"),
        (new.importance, "importance"),
    ] {
        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
            return Err(SilynError::Schema(format!(
                "memory {field} must be between 0 and 1"
            )));
        }
    }
    if let (Some(from), Some(to)) = (new.validity_from, new.validity_to)
        && to <= from
    {
        return Err(SilynError::Schema(
            "validity_to must be after validity_from".to_owned(),
        ));
    }
    if !matches!(new.sensitivity.as_str(), "public" | "private" | "secret") {
        return Err(SilynError::Schema(format!(
            "unknown sensitivity `{}`",
            new.sensitivity
        )));
    }
    Ok(())
}

/// Quotes every user token so FTS5 operators cannot reach the parser.
///
/// Tokens without letters or digits are dropped: they tokenize to nothing and
/// a phrase with no terms would match no row anyway. `None` means "no query".
fn fts_terms(query: &str) -> Option<String> {
    let mut terms = Vec::new();
    for token in query.split_whitespace() {
        if !token.chars().any(char::is_alphanumeric) {
            continue;
        }
        terms.push(format!("\"{}\"", token.replace('"', "\"\"")));
    }
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" "))
    }
}

fn record_from_row(row: &SqliteRow) -> Result<MemoryRecord, SilynError> {
    let sources: String = text(row, "source_event_ids")?;
    Ok(MemoryRecord {
        id: parse_id(&text(row, "id")?, "memories.id")?,
        owner_user_id: text(row, "owner_user_id")?,
        agent_id: parse_id(&text(row, "agent_id")?, "memories.agent_id")?,
        scope: decode_enum(&text(row, "scope")?)?,
        memory_type: decode_enum(&text(row, "type")?)?,
        content_ref: text(row, "content_ref")?,
        content: text(row, "content")?,
        summary: text(row, "summary")?,
        source_event_ids: serde_json::from_str(&sources)?,
        provenance: decode_enum(&text(row, "provenance")?)?,
        confidence: real(row, "confidence")? as f32,
        importance: real(row, "importance")? as f32,
        validity_from: optional_timestamp(row, "validity_from")?,
        validity_to: optional_timestamp(row, "validity_to")?,
        last_used_at: optional_timestamp(row, "last_used_at")?,
        created_at: timestamp(row, "created_at")?,
        updated_at: timestamp(row, "updated_at")?,
        status: decode_enum(&text(row, "status")?)?,
        supersedes_id: optional_text(row, "supersedes_id")?
            .map(|value| parse_id(&value, "memories.supersedes_id"))
            .transpose()?,
        sensitivity: text(row, "sensitivity")?,
        embedding_version: optional_text(row, "embedding_version")?,
    })
}

fn hit_from_row(row: &SqliteRow) -> Result<MemoryHitRecord, SilynError> {
    let score = real(row, "score")?;
    Ok(MemoryHitRecord {
        id: parse_id(&text(row, "id")?, "memories.id")?,
        summary: text(row, "summary")?,
        content: text(row, "content")?,
        provenance: decode_enum(&text(row, "provenance")?)?,
        confidence: real(row, "confidence")? as f32,
        importance: real(row, "importance")? as f32,
        status: decode_enum(&text(row, "status")?)?,
        score: (-score) as f32,
    })
}

fn encode_enum<T: Serialize>(value: &T) -> Result<String, SilynError> {
    let encoded = serde_json::to_value(value)?;
    encoded
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| SilynError::Schema("enum did not encode as a string".to_owned()))
}

fn decode_enum<T: DeserializeOwned>(value: &str) -> Result<T, SilynError> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).map_err(Into::into)
}

fn text(row: &SqliteRow, column: &str) -> Result<String, SilynError> {
    row.try_get(column).map_err(db_error)
}

fn optional_text(row: &SqliteRow, column: &str) -> Result<Option<String>, SilynError> {
    row.try_get(column).map_err(db_error)
}

fn real(row: &SqliteRow, column: &str) -> Result<f64, SilynError> {
    row.try_get(column).map_err(db_error)
}

fn timestamp(row: &SqliteRow, column: &str) -> Result<DateTime<Utc>, SilynError> {
    row.try_get(column).map_err(db_error)
}

fn optional_timestamp(row: &SqliteRow, column: &str) -> Result<Option<DateTime<Utc>>, SilynError> {
    row.try_get(column).map_err(db_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts_terms_quotes_user_input() {
        assert_eq!(
            fts_terms("deploy window").as_deref(),
            Some("\"deploy\" \"window\"")
        );
        assert_eq!(
            fts_terms("(a) -b *c \"d\" AND").as_deref(),
            Some("\"(a)\" \"-b\" \"*c\" \"\"\"d\"\"\" \"AND\"")
        );
        assert_eq!(fts_terms("\"\"\" - * ( )"), None);
        assert_eq!(fts_terms("   "), None);
    }
}
