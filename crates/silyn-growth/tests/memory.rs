//! Memory store acceptance tests: isolation, lifecycle, retrieval and purge.
//!
//! Every test runs against a fresh in-memory database with the real migrations
//! applied, so no fixture database and no network access are needed.

use std::collections::HashSet;

use chrono::{Duration, Utc};
use silyn_core::contracts::{MemoryScope, MemoryType};
use silyn_core::ids::{AgentId, EventId};
use silyn_growth::{
    ContentStore, MemoryScopeFilter, MemoryStatus, MemoryStore, NewMemory, Provenance, RelationType,
};
use silyn_storage::{AgentRecord, AgentRepository, Storage};

async fn setup() -> (Storage, AgentId) {
    let storage = Storage::connect_in_memory()
        .await
        .expect("in-memory storage with migrations");
    let agent = insert_agent(&storage, "user-a").await;
    (storage, agent)
}

async fn insert_agent(storage: &Storage, owner: &str) -> AgentId {
    let record = AgentRecord {
        id: AgentId::new(),
        owner_id: owner.to_owned(),
        profile_version: "profile-v1".to_owned(),
        active_policy_version: None,
        created_at: Utc::now(),
    };
    AgentRepository::new(storage.pool())
        .insert(&record)
        .await
        .expect("insert agent");
    record.id
}

fn filter_for(owner: &str, agent: AgentId) -> MemoryScopeFilter {
    MemoryScopeFilter {
        owner_user_id: owner.to_owned(),
        agent_id: agent,
        scopes: Vec::new(),
        types: Vec::new(),
        include_expired: false,
    }
}

fn new_memory(owner: &str, agent: AgentId, summary: &str, content: &str) -> NewMemory {
    NewMemory {
        owner_user_id: owner.to_owned(),
        agent_id: agent,
        scope: MemoryScope::Agent,
        memory_type: MemoryType::Semantic,
        content: content.to_owned(),
        summary: summary.to_owned(),
        source_event_ids: vec![EventId::new().to_string()],
        provenance: Provenance::UserStatement,
        confidence: 0.9,
        importance: 0.5,
        validity_from: None,
        validity_to: None,
        sensitivity: "private".to_owned(),
    }
}

async fn count(storage: &Storage, sql: &str, bind: &str) -> i64 {
    sqlx::query_scalar(sql)
        .bind(bind)
        .fetch_one(storage.pool())
        .await
        .expect("count rows")
}

#[tokio::test]
async fn cross_agent_isolation() {
    let (storage, agent_a) = setup().await;
    let agent_b = insert_agent(&storage, "user-a").await;
    let agent_c = insert_agent(&storage, "user-b").await;
    let store = MemoryStore::new(storage.pool());

    let record = store
        .put(new_memory(
            "user-a",
            agent_a,
            "budget plan",
            "the quarterly budget was approved",
        ))
        .await
        .expect("put memory");

    assert!(
        store
            .get("user-a", agent_a, record.id)
            .await
            .expect("get")
            .is_some(),
        "the owning agent must read its own memory"
    );
    assert!(
        store
            .get("user-a", agent_b, record.id)
            .await
            .expect("get")
            .is_none(),
        "another agent must not read the memory"
    );
    assert!(
        store
            .get("user-b", agent_a, record.id)
            .await
            .expect("get")
            .is_none(),
        "a wrong owner must not read the memory"
    );

    let search_other_agent = store
        .search(&filter_for("user-a", agent_b), "budget", 10)
        .await
        .expect("search as another agent");
    assert!(search_other_agent.is_empty());

    let search_wrong_owner = store
        .search(&filter_for("user-b", agent_a), "budget", 10)
        .await
        .expect("search as a wrong owner");
    assert!(search_wrong_owner.is_empty());

    let listing_other_agent = store
        .search(&filter_for("user-a", agent_b), "", 10)
        .await
        .expect("empty-query search as another agent");
    assert!(listing_other_agent.is_empty());

    let unrelated = store
        .search(&filter_for("user-b", agent_c), "", 10)
        .await
        .expect("empty-query search as an unrelated agent");
    assert!(unrelated.is_empty());

    // The record itself carries no way to widen the scope: the agent id is a
    // caller-supplied argument on every read.
    assert_eq!(record.agent_id, agent_a);
    assert_eq!(record.owner_user_id, "user-a");
}

#[tokio::test]
async fn conflict_does_not_overwrite() {
    let (storage, agent) = setup().await;
    let store = MemoryStore::new(storage.pool());

    let first = store
        .put(new_memory(
            "user-a",
            agent,
            "deploy window is monday",
            "deploys happen on monday at 09:00",
        ))
        .await
        .expect("first memory");
    let second = store
        .put(new_memory(
            "user-a",
            agent,
            "deploy window is tuesday",
            "deploys happen on tuesday at 09:00",
        ))
        .await
        .expect("second memory");

    store
        .relate(first.id, second.id, RelationType::Contradicts)
        .await
        .expect("record the conflict");
    store
        .relate(first.id, second.id, RelationType::Contradicts)
        .await
        .expect("relate is idempotent");

    let reloaded_first = store
        .get("user-a", agent, first.id)
        .await
        .expect("get first")
        .expect("first still exists");
    let reloaded_second = store
        .get("user-a", agent, second.id)
        .await
        .expect("get second")
        .expect("second still exists");

    assert_eq!(reloaded_first.status, MemoryStatus::Active);
    assert_eq!(reloaded_second.status, MemoryStatus::Active);
    assert_eq!(reloaded_first.summary, "deploy window is monday");
    assert_eq!(reloaded_second.summary, "deploy window is tuesday");
    assert_eq!(reloaded_first.updated_at, first.updated_at);
    assert_eq!(reloaded_second.updated_at, second.updated_at);

    let relations = store
        .relations_for("user-a", agent, first.id)
        .await
        .expect("relations");
    assert_eq!(relations.len(), 1, "the duplicate insert was ignored");
    assert_eq!(relations[0].from_id, first.id);
    assert_eq!(relations[0].to_id, second.id);
    assert_eq!(relations[0].relation_type, RelationType::Contradicts);

    let hits = store
        .search(&filter_for("user-a", agent), "deploys", 10)
        .await
        .expect("search");
    assert_eq!(hits.len(), 2, "both conflicting memories stay retrievable");

    let self_relation = store
        .relate(first.id, first.id, RelationType::Supports)
        .await;
    assert!(self_relation.is_err(), "self relations are rejected");
}

#[tokio::test]
async fn expiry_filters_retrieval() {
    let (storage, agent) = setup().await;
    let store = MemoryStore::new(storage.pool());
    let now = Utc::now();

    let mut stale = new_memory(
        "user-a",
        agent,
        "stale launch date",
        "the launch was scheduled for last friday",
    );
    stale.validity_from = Some(now - Duration::days(10));
    stale.validity_to = Some(now - Duration::days(1));
    let stale = store.put(stale).await.expect("put stale memory");

    let default_filter = filter_for("user-a", agent);
    assert!(
        store
            .search(&default_filter, "launch", 10)
            .await
            .expect("search")
            .is_empty(),
        "a closed validity window is excluded by default"
    );
    assert!(
        store
            .search(&default_filter, "", 10)
            .await
            .expect("listing")
            .is_empty()
    );

    let mut inclusive = default_filter.clone();
    inclusive.include_expired = true;
    let expanded = store
        .search(&inclusive, "launch", 10)
        .await
        .expect("search including expired windows");
    assert_eq!(expanded.len(), 1);
    assert_eq!(expanded[0].id, stale.id);

    let live = store
        .put(new_memory(
            "user-a",
            agent,
            "current launch date",
            "the launch is scheduled for friday",
        ))
        .await
        .expect("put live memory");

    store.expire(live.id, now).await.expect("expire");
    let reloaded = store
        .get("user-a", agent, live.id)
        .await
        .expect("get")
        .expect("expired row is still readable");
    assert_eq!(reloaded.status, MemoryStatus::Expired);
    assert_eq!(reloaded.validity_to, Some(now));

    for filter in [&default_filter, &inclusive] {
        let hits = store.search(filter, "launch", 10).await.expect("search");
        assert!(
            hits.iter().all(|hit| hit.id != live.id),
            "an expired status is not resurrected by include_expired"
        );
    }
}

#[tokio::test]
async fn fts_search_returns_ranked_hits() {
    let (storage, agent) = setup().await;
    let store = MemoryStore::new(storage.pool());
    let filter = filter_for("user-a", agent);

    let summary_hit = store
        .put(new_memory(
            "user-a",
            agent,
            "quarterly zephyr report",
            "numbers for the quarter",
        ))
        .await
        .expect("summary memory");
    let content_hit = store
        .put(new_memory(
            "user-a",
            agent,
            "release checklist",
            "ship the zephyr service on friday",
        ))
        .await
        .expect("content memory");

    let hits = store.search(&filter, "zephyr", 10).await.expect("search");
    assert_eq!(hits.len(), 2);
    let ids: HashSet<_> = hits.iter().map(|hit| hit.id).collect();
    assert_eq!(ids, HashSet::from([summary_hit.id, content_hit.id]));
    assert!(
        hits.iter().all(|hit| hit.score >= 0.0),
        "score is -bm25 so higher means more relevant"
    );

    let mut low = new_memory(
        "user-a",
        agent,
        "customer escalation",
        "the customer escalated about latency",
    );
    low.importance = 0.2;
    let low = store.put(low).await.expect("low importance memory");
    let mut high = new_memory(
        "user-a",
        agent,
        "customer escalation",
        "the customer escalated about latency",
    );
    high.importance = 0.95;
    let high = store.put(high).await.expect("high importance memory");

    let ranked = store
        .search(&filter, "escalation", 10)
        .await
        .expect("search");
    assert_eq!(ranked.len(), 2);
    assert_eq!(
        ranked.iter().map(|hit| hit.id).collect::<Vec<_>>(),
        vec![high.id, low.id],
        "equal text is ranked by importance, highest first"
    );
    assert!(ranked[0].score >= ranked[1].score);

    let typed = store
        .search(
            &MemoryScopeFilter {
                types: vec![MemoryType::Working],
                ..filter.clone()
            },
            "zephyr",
            10,
        )
        .await
        .expect("typed search");
    assert!(typed.is_empty(), "type filters are applied");

    let special = store
        .search(
            &filter,
            "(zephyr) -latency *star \"escalation\" AND OR NOT",
            10,
        )
        .await
        .expect("special characters must not break MATCH");
    assert!(
        special.is_empty(),
        "quoted terms are ANDed, so no memory matches all of them"
    );

    let nonsense = store
        .search(&filter, "((-)) *** \"\"\"", 10)
        .await
        .expect("punctuation-only queries fall back to a listing");
    assert_eq!(nonsense.len(), 4);

    let unmatched = store
        .search(&filter, "zzznomatch", 10)
        .await
        .expect("search without hits");
    assert!(unmatched.is_empty());

    // An empty query lists the newest active memories, newest first.
    let quiet_agent = insert_agent(&storage, "user-a").await;
    let quiet_filter = filter_for("user-a", quiet_agent);
    let first = store
        .put(new_memory(
            "user-a",
            quiet_agent,
            "first note",
            "the first note body",
        ))
        .await
        .expect("first quiet memory");
    let second = store
        .put(new_memory(
            "user-a",
            quiet_agent,
            "second note",
            "the second note body",
        ))
        .await
        .expect("second quiet memory");

    let newest = store
        .search(&quiet_filter, "   ", 10)
        .await
        .expect("empty-query listing");
    assert_eq!(
        newest.iter().map(|hit| hit.id).collect::<Vec<_>>(),
        vec![second.id, first.id]
    );
}

#[tokio::test]
async fn supersede_preserves_history() {
    let (storage, agent) = setup().await;
    let store = MemoryStore::new(storage.pool());
    let blobs = ContentStore::new(storage.pool());

    let old = store
        .put(new_memory(
            "user-a",
            agent,
            "the office is in berlin",
            "the office moved to berlin in 2024",
        ))
        .await
        .expect("old memory");
    let new = store
        .put(new_memory(
            "user-a",
            agent,
            "the office is in munich",
            "the office moved to munich in 2026",
        ))
        .await
        .expect("new memory");

    store.supersede(old.id, new.id).await.expect("supersede");

    let old_record = store
        .get("user-a", agent, old.id)
        .await
        .expect("get old")
        .expect("the old row remains readable for audit");
    assert_eq!(old_record.status, MemoryStatus::Superseded);
    assert_eq!(old_record.summary, "the office is in berlin");
    assert_eq!(
        blobs
            .get(&old_record.content_ref)
            .await
            .expect("blob lookup"),
        Some("the office moved to berlin in 2024".to_owned()),
        "supersession keeps the old payload"
    );

    let new_record = store
        .get("user-a", agent, new.id)
        .await
        .expect("get new")
        .expect("new row exists");
    assert_eq!(new_record.status, MemoryStatus::Active);
    assert_eq!(new_record.supersedes_id, Some(old.id));

    let relations = store
        .relations_for("user-a", agent, old.id)
        .await
        .expect("relations");
    assert_eq!(relations.len(), 1);
    assert_eq!(relations[0].from_id, new.id);
    assert_eq!(relations[0].to_id, old.id);
    assert_eq!(relations[0].relation_type, RelationType::Supersedes);

    let hits = store
        .search(&filter_for("user-a", agent), "office", 10)
        .await
        .expect("search");
    assert_eq!(
        hits.iter().map(|hit| hit.id).collect::<Vec<_>>(),
        vec![new.id],
        "only the successor is retrievable"
    );

    assert!(
        store.supersede(old.id, old.id).await.is_err(),
        "self supersession is rejected"
    );
}

#[tokio::test]
async fn purge_removes_blob_and_index() {
    let (storage, agent) = setup().await;
    let store = MemoryStore::new(storage.pool());
    let blobs = ContentStore::new(storage.pool());
    let shared = "shared payload used by two memories";

    let first = store
        .put(new_memory("user-a", agent, "first shared note", shared))
        .await
        .expect("first memory");
    let second = store
        .put(new_memory("user-a", agent, "second shared note", shared))
        .await
        .expect("second memory");
    assert_eq!(first.content_ref, second.content_ref);
    assert_eq!(
        blobs
            .ref_count(&first.content_ref)
            .await
            .expect("ref count"),
        2
    );
    store
        .relate(first.id, second.id, RelationType::Supports)
        .await
        .expect("relate");

    store
        .soft_delete(first.id, "user-a")
        .await
        .expect("soft delete");
    assert!(
        store
            .get("user-a", agent, first.id)
            .await
            .expect("get")
            .is_none(),
        "a soft-deleted memory is hidden from the store API"
    );
    assert_eq!(
        count(
            &storage,
            "SELECT COUNT(*) FROM memories WHERE id = ?1",
            &first.id.to_string()
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &storage,
            "SELECT COUNT(*) FROM memory_fts WHERE memory_id = ?1",
            &first.id.to_string()
        )
        .await,
        1,
        "the index entry survives until purge"
    );

    let report = store.purge(first.id, "user-a").await.expect("purge");
    assert_eq!(report.memory_id, first.id);
    assert_eq!(report.sources_removed, 1);
    assert_eq!(report.relations_removed, 1);
    assert!(
        !report.blob_removed,
        "the blob is still referenced by the second memory"
    );

    assert!(
        store
            .get("user-a", agent, first.id)
            .await
            .expect("get")
            .is_none()
    );
    assert_eq!(
        count(
            &storage,
            "SELECT COUNT(*) FROM memories WHERE id = ?1",
            &first.id.to_string()
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &storage,
            "SELECT COUNT(*) FROM memory_fts WHERE memory_id = ?1",
            &first.id.to_string()
        )
        .await,
        0,
        "the FTS entry is removed"
    );
    assert_eq!(
        count(
            &storage,
            "SELECT COUNT(*) FROM memory_sources WHERE memory_id = ?1",
            &first.id.to_string()
        )
        .await,
        0,
        "provenance rows are removed"
    );
    assert_eq!(
        count(
            &storage,
            "SELECT COUNT(*) FROM memory_relations WHERE from_id = ?1 OR to_id = ?1",
            &first.id.to_string()
        )
        .await,
        0,
        "relation rows are removed"
    );
    assert_eq!(
        blobs
            .ref_count(&first.content_ref)
            .await
            .expect("ref count"),
        1,
        "the shared blob stays alive"
    );
    assert!(
        store
            .search(&filter_for("user-a", agent), "shared payload", 10)
            .await
            .expect("search")
            .iter()
            .any(|hit| hit.id == second.id),
        "the remaining memory is still retrievable"
    );

    let audit: (String, String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT actor, action, subject, decision FROM audit_events
         WHERE action = 'memory.purge' AND subject = ?1",
    )
    .bind(first.id.to_string())
    .fetch_one(storage.pool())
    .await
    .expect("purge audit row");
    assert_eq!(audit.0, "user-a");
    assert_eq!(audit.1, "memory.purge");
    assert_eq!(audit.2, Some(first.id.to_string()));
    assert_eq!(audit.3.as_deref(), Some("allow"));

    let second_report = store.purge(second.id, "user-a").await.expect("purge");
    assert!(
        second_report.blob_removed,
        "the last reference to the blob is removed"
    );
    assert!(
        blobs
            .get(&first.content_ref)
            .await
            .expect("blob lookup")
            .is_none()
    );
    assert_eq!(
        count(
            &storage,
            "SELECT COUNT(*) FROM content_blobs WHERE id = ?1",
            &first.content_ref
        )
        .await,
        0
    );

    assert!(
        store.purge(first.id, "user-a").await.is_err(),
        "purging a missing memory is an error"
    );
}

#[tokio::test]
async fn blob_deduplicates_content() {
    let (storage, agent) = setup().await;
    let blobs = ContentStore::new(storage.pool());

    let first = blobs.put("identical payload").await.expect("put once");
    let second = blobs.put("identical payload").await.expect("put twice");
    assert_eq!(first, second, "identical bytes share one digest");
    assert_eq!(first.len(), 64, "sha256 hex digest");

    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM content_blobs")
        .fetch_one(storage.pool())
        .await
        .expect("count blobs");
    assert_eq!(rows, 1);
    assert_eq!(
        blobs.get(&first).await.expect("get"),
        Some("identical payload".to_owned())
    );
    assert_eq!(blobs.ref_count(&first).await.expect("ref count"), 0);

    let other = blobs.put("a different payload").await.expect("put other");
    assert_ne!(first, other);
    assert!(
        blobs.put("   ").await.is_err(),
        "whitespace-only content is rejected"
    );
    assert!(blobs.put("").await.is_err(), "empty content is rejected");

    let store = MemoryStore::new(storage.pool());
    let memory = store
        .put(new_memory(
            "user-a",
            agent,
            "dedupe note",
            "identical payload",
        ))
        .await
        .expect("put memory");
    assert_eq!(
        memory.content_ref, first,
        "a memory reuses the existing blob"
    );
    assert_eq!(blobs.ref_count(&first).await.expect("ref count"), 1);
    assert_eq!(
        count(
            &storage,
            "SELECT COUNT(*) FROM content_blobs WHERE id = ?1",
            &first
        )
        .await,
        1,
        "storing the payload again did not add a row"
    );
}
