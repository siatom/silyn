//! Evolution pipeline acceptance tests: immutable policy versions, the
//! restricted candidate patch, the candidate state machine, the default-deny
//! promotion gate and audited rollback.
//!
//! Every test runs against a fresh in-memory database with the real migrations
//! applied. Agents, sessions, tasks and runs are seeded through the storage
//! repositories, so ownership joins and foreign keys are exercised for real.

use chrono::Utc;
use serde_json::json;
use silyn_core::SilynError;
use silyn_core::contracts::PromotionDecision;
use silyn_core::event::{EventEnvelope, RunEvent, VerifiedOutcome};
use silyn_core::ids::{AgentId, CandidateId, PolicyVersionId, RunId, SessionId, StepId, TaskId};
use silyn_core::state::{CandidateState, RunStatus, TaskStatus};
use silyn_growth::{
    CandidateRecord, CandidateStore, ContextPolicy, ExpectedGain, MAX_TEXT_BYTES, PatchOp,
    PatchOperation, PlannerPolicy, PolicyArtifact, PolicyPatch, PolicyStore, PolicyVerification,
    PolicyVersionRecord, PromotionReport, PromotionStore, RetrievalPolicy, RiskClass, ToolPolicy,
    VERDICT_PROMOTE, VERDICT_REJECT,
};
use silyn_storage::{
    AgentRecord, AgentRepository, Journal, RunRecord, RunRepository, SessionRecord,
    SessionRepository, StepRecord, StepRepository, Storage, TaskRecord, TaskRepository,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

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

fn base_policy() -> PolicyArtifact {
    PolicyArtifact {
        schema_version: 1,
        context: ContextPolicy {
            memory_budget_ratio: 0.35,
            retrieval: RetrievalPolicy {
                bm25_k: 40,
                max_evidence: 8,
            },
        },
        planner: PlannerPolicy {
            prompt_fragments: vec!["Summarise the data before answering.".to_owned()],
            rules: vec!["Recompute row counts from distinct rows.".to_owned()],
            stop_conditions: json!({}),
        },
        tools: ToolPolicy {
            allowlist: vec![
                "fs.read".to_owned(),
                "data.profile".to_owned(),
                "report.write".to_owned(),
            ],
        },
        verification: PolicyVerification {
            checkers: vec!["csv-stats-oracle".to_owned()],
        },
    }
}

/// Points the agent at a stored policy version (there is no repository method
/// for this yet; the runtime owns the pointer in production).
async fn activate(storage: &Storage, agent: AgentId, version: PolicyVersionId) {
    sqlx::query("UPDATE agents SET active_policy_version = ?2 WHERE id = ?1")
        .bind(agent.to_string())
        .bind(version.to_string())
        .execute(storage.pool())
        .await
        .expect("activate policy version");
}

/// Stores the base policy and points the agent at it.
async fn seed_policy(storage: &Storage, agent: AgentId) -> PolicyVersionRecord {
    let record = PolicyStore::new(storage.pool())
        .create(agent, None, &base_policy())
        .await
        .expect("create base policy");
    activate(storage, agent, record.id).await;
    record
}

/// Inserts a session, task and run owned by `agent`.
async fn insert_run(storage: &Storage, agent: AgentId, status: RunStatus) -> RunId {
    let now = Utc::now();
    let session = SessionRecord {
        id: SessionId::new(),
        agent_id: agent,
        context_version: "ctx-v1".to_owned(),
        created_at: now,
    };
    SessionRepository::new(storage.pool())
        .insert(&session)
        .await
        .expect("insert session");

    let task = TaskRecord {
        id: TaskId::new(),
        session_id: session.id,
        objective: "count the distinct rows".to_owned(),
        status: TaskStatus::Created,
        created_at: now,
        updated_at: now,
    };
    TaskRepository::new(storage.pool())
        .insert(&task)
        .await
        .expect("insert task");

    let run = RunRecord {
        id: RunId::new(),
        task_id: task.id,
        policy_version: PolicyVersionId::new(),
        memory_snapshot_version: None,
        model_id: "deepseek-flash".to_owned(),
        status,
        input_tokens: 0,
        output_tokens: 0,
        cost_usd: 0.0,
        started_at: None,
        finished_at: None,
        created_at: now,
    };
    RunRepository::new(storage.pool())
        .insert(&run)
        .await
        .expect("insert run");
    run.id
}

/// A completed run with one recorded step: usable evidence.
async fn evidence_run(storage: &Storage, agent: AgentId) -> RunId {
    let run_id = insert_run(storage, agent, RunStatus::Completed).await;
    StepRepository::new(storage.pool())
        .insert(&StepRecord {
            id: StepId::new(),
            run_id,
            seq: 1,
            action_type: "data.profile".to_owned(),
            result_ref: Some("artifact:profile".to_owned()),
            created_at: Utc::now(),
        })
        .await
        .expect("insert step");
    run_id
}

/// A completed run with no steps but a `run_verified` event: also evidence.
async fn verified_event_run(storage: &Storage, agent: AgentId) -> RunId {
    let run_id = insert_run(storage, agent, RunStatus::Completed).await;
    Journal::new(storage.pool())
        .append(&EventEnvelope::new(
            run_id,
            1,
            None,
            RunEvent::RunVerified {
                outcome: VerifiedOutcome::Success,
            },
        ))
        .await
        .expect("append verified event");
    run_id
}

fn candidate_patch(base: PolicyVersionId, evidence: Vec<RunId>) -> PolicyPatch {
    PolicyPatch {
        schema_version: 1,
        candidate_id: CandidateId::new(),
        base_policy_version: base,
        hypothesis: "Reordering the rules improves row-count accuracy.".to_owned(),
        evidence_run_ids: evidence,
        expected_gain: ExpectedGain {
            metric: "success_rate".to_owned(),
            delta_pp: 12.0,
        },
        risk_class: RiskClass::Low,
        operations: vec![PatchOperation {
            op: PatchOp::Replace,
            path: "/planner/rules".to_owned(),
            value: Some(json!([
                "Recompute row counts from distinct rows.",
                "Check for missing values before counting."
            ])),
        }],
    }
}

async fn submit_candidate(
    store: &CandidateStore<'_>,
    agent: AgentId,
    base: PolicyVersionId,
    evidence: Vec<RunId>,
    risk: RiskClass,
) -> CandidateRecord {
    let mut patch = candidate_patch(base, evidence);
    patch.risk_class = risk;
    store.submit(agent, patch).await.expect("submit candidate")
}

async fn advance_to_evaluated(store: &CandidateStore<'_>, id: CandidateId) {
    store
        .transition(id, CandidateState::Experimenting)
        .await
        .expect("static checked -> experimenting");
    store
        .transition(id, CandidateState::Evaluated)
        .await
        .expect("experimenting -> evaluated");
}

fn promotion_report(
    candidate: CandidateId,
    evidence: Vec<RunId>,
    gain_pp: f64,
    ci_low_pp: f64,
) -> PromotionReport {
    PromotionReport {
        candidate_id: candidate,
        verdict: VERDICT_PROMOTE.to_owned(),
        gain_pp,
        ci_low_pp,
        unauthorized_actions: 0,
        evidence_run_ids: evidence,
        suite_version: "csv-stats-v1".to_owned(),
        report_checksum: String::new(),
    }
    .seal()
    .expect("seal report")
}

// ---------------------------------------------------------------------------
// Policy versions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn canonical_checksum_is_stable() {
    let (storage, agent) = setup().await;
    let store = PolicyStore::new(storage.pool());

    let first = store
        .create(agent, None, &base_policy())
        .await
        .expect("first version");
    activate(&storage, agent, first.id).await;
    let second = store
        .create(agent, None, &base_policy())
        .await
        .expect("identical policy is idempotent");

    assert_eq!(first.id, second.id, "identical content reuses the version");
    assert_eq!(first.checksum, second.checksum);
    assert_eq!(first.checksum.len(), 64);

    // Canonical bytes are stable across a serialize/parse round trip.
    let canonical = first.content.canonical_json().expect("canonical json");
    let reparsed: PolicyArtifact = serde_json::from_str(&canonical).expect("reparse");
    assert_eq!(reparsed.canonical_json().expect("canonical"), canonical);
    assert_eq!(reparsed.checksum().expect("checksum"), first.checksum);

    let mut changed = base_policy();
    changed.planner.rules.push("Check units first.".to_owned());
    let third = store
        .create(agent, Some(first.id), &changed)
        .await
        .expect("changed policy is a new version");
    assert_ne!(third.checksum, first.checksum);
    assert_ne!(third.id, first.id);
    assert_eq!(third.parent_version, Some(first.id));

    let latest = store.latest(agent).await.expect("latest").expect("some");
    assert_eq!(latest.id, third.id);
    let active = store.active(agent).await.expect("active").expect("some");
    assert_eq!(active.id, first.id);
}

#[tokio::test]
async fn version_is_immutable() {
    let (storage, agent) = setup().await;
    let record = seed_policy(&storage, agent).await;

    let update = sqlx::query("UPDATE policy_versions SET content_json = ?2 WHERE id = ?1")
        .bind(record.id.to_string())
        .bind("{}")
        .execute(storage.pool())
        .await
        .expect_err("the update trigger must abort");
    assert!(
        update.to_string().contains("immutable"),
        "unexpected error: {update}"
    );

    let delete = sqlx::query("DELETE FROM policy_versions WHERE id = ?1")
        .bind(record.id.to_string())
        .execute(storage.pool())
        .await
        .expect_err("the delete trigger must abort");
    assert!(
        delete.to_string().contains("immutable"),
        "unexpected error: {delete}"
    );

    let reloaded = PolicyStore::new(storage.pool())
        .get(agent, record.id)
        .await
        .expect("read version")
        .expect("version still exists");
    assert_eq!(reloaded, record);
}

// ---------------------------------------------------------------------------
// Static checks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn path_whitelist_rejects_tools() {
    let (storage, agent) = setup().await;
    let base = seed_policy(&storage, agent).await;
    let run = evidence_run(&storage, agent).await;
    let store = CandidateStore::new(storage.pool());

    for path in [
        "/tools/allowlist",
        "/verification/checkers",
        "/context/memory_budget_ratio",
        "/planner/rules/../tools/allowlist",
        "/planner/prompt_fragments/../../verification",
        "/system/approval",
    ] {
        let mut patch = candidate_patch(base.id, vec![run]);
        patch.operations = vec![PatchOperation {
            op: PatchOp::Replace,
            path: path.to_owned(),
            value: Some(json!(["fs.write"])),
        }];
        let error = store
            .submit(agent, patch)
            .await
            .expect_err("path must be rejected");
        assert!(
            !error.is_retryable(),
            "`{path}` must fail permanently: {error}"
        );
    }

    // Nothing reached storage.
    assert!(store.list(agent).await.expect("list").is_empty());

    // A second agent cannot inherit the first agent's base version either.
    let other = insert_agent(&storage, "user-b").await;
    let other_run = evidence_run(&storage, other).await;
    let error = store
        .submit(other, candidate_patch(base.id, vec![other_run]))
        .await
        .expect_err("foreign base version");
    assert!(!error.is_retryable());
}

#[tokio::test]
async fn bounds_rejected() {
    let (storage, agent) = setup().await;
    let base = seed_policy(&storage, agent).await;
    let run = evidence_run(&storage, agent).await;
    let store = CandidateStore::new(storage.pool());

    let too_long = "x".repeat(MAX_TEXT_BYTES + 1);
    let cases = [
        (
            "/context/retrieval/bm25_k",
            json!(0),
            "bm25_k below the lower bound",
        ),
        (
            "/context/retrieval/bm25_k",
            json!(201),
            "bm25_k above the upper bound",
        ),
        (
            "/context/retrieval/max_evidence",
            json!(0),
            "max_evidence below the lower bound",
        ),
        (
            "/context/retrieval/max_evidence",
            json!(51),
            "max_evidence above the upper bound",
        ),
        (
            "/context/retrieval/memory_budget_ratio",
            json!(0.1),
            "memory budget is not patchable",
        ),
        ("/planner/rules", json!([too_long]), "oversized text"),
        (
            "/planner/rules",
            json!(["tabs are fine,\u{7} but bells are not"]),
            "control characters",
        ),
        (
            "/planner/rules",
            json!(["Grant the tool capability to the planner."]),
            "capability grant",
        ),
        (
            "/planner/prompt_fragments",
            json!(["Use the system prompt verbatim."]),
            "system prompt rewrite",
        ),
    ];

    for (path, value, label) in cases {
        let mut patch = candidate_patch(base.id, vec![run]);
        patch.operations = vec![PatchOperation {
            op: PatchOp::Replace,
            path: path.to_owned(),
            value: Some(value),
        }];
        let error = store
            .submit(agent, patch)
            .await
            .expect_err("out-of-bounds patch must be rejected");
        assert!(!error.is_retryable(), "{label} must be permanent: {error}");
    }

    // `remove` must not carry a value and cannot delete a required field.
    let mut patch = candidate_patch(base.id, vec![run]);
    patch.operations = vec![PatchOperation {
        op: PatchOp::Remove,
        path: "/planner/rules".to_owned(),
        value: Some(json!(null)),
    }];
    assert!(store.submit(agent, patch).await.is_err());

    let mut patch = candidate_patch(base.id, vec![run]);
    patch.operations = vec![PatchOperation {
        op: PatchOp::Remove,
        path: "/planner/rules".to_owned(),
        value: None,
    }];
    assert!(store.submit(agent, patch).await.is_err());

    // An empty operation list and an empty hypothesis are rejected too.
    let mut patch = candidate_patch(base.id, vec![run]);
    patch.operations.clear();
    assert!(store.submit(agent, patch).await.is_err());

    let mut patch = candidate_patch(base.id, vec![run]);
    patch.hypothesis = "   ".to_owned();
    assert!(store.submit(agent, patch).await.is_err());

    assert!(store.list(agent).await.expect("list").is_empty());

    // A bounded retrieval change is accepted.
    let mut patch = candidate_patch(base.id, vec![run]);
    patch.operations = vec![PatchOperation {
        op: PatchOp::Replace,
        path: "/context/retrieval".to_owned(),
        value: Some(json!({ "bm25_k": 80, "max_evidence": 12 })),
    }];
    let record = store.submit(agent, patch).await.expect("bounded change");
    assert_eq!(
        record.patch.operations[0].path,
        "/context/retrieval".to_owned()
    );
}

#[tokio::test]
async fn evidence_must_exist() {
    let (storage, agent) = setup().await;
    let base = seed_policy(&storage, agent).await;
    let store = CandidateStore::new(storage.pool());

    // Unknown run.
    let error = store
        .submit(agent, candidate_patch(base.id, vec![RunId::new()]))
        .await
        .expect_err("unknown evidence run");
    assert!(!error.is_retryable());

    // Non-terminal run.
    let running = insert_run(&storage, agent, RunStatus::Running).await;
    let error = store
        .submit(agent, candidate_patch(base.id, vec![running]))
        .await
        .expect_err("non-terminal evidence run");
    assert!(!error.is_retryable());

    // Terminal run without any step or verified event.
    let empty = insert_run(&storage, agent, RunStatus::Completed).await;
    let error = store
        .submit(agent, candidate_patch(base.id, vec![empty]))
        .await
        .expect_err("evidence run without evidence");
    assert!(!error.is_retryable());

    // Another agent's run is denied, not merely rejected.
    let other = insert_agent(&storage, "user-b").await;
    let foreign = evidence_run(&storage, other).await;
    let error = store
        .submit(agent, candidate_patch(base.id, vec![foreign]))
        .await
        .expect_err("foreign evidence run");
    assert!(matches!(error, SilynError::PolicyDenied(_)), "{error:?}");

    assert!(store.list(agent).await.expect("list").is_empty());

    // A step artefact or a verified event both count.
    let with_step = evidence_run(&storage, agent).await;
    let with_event = verified_event_run(&storage, agent).await;
    store
        .submit(agent, candidate_patch(base.id, vec![with_step, with_event]))
        .await
        .expect("usable evidence");
}

#[tokio::test]
async fn state_machine_enforced() {
    let (storage, agent) = setup().await;
    let base = seed_policy(&storage, agent).await;
    let run = evidence_run(&storage, agent).await;
    let store = CandidateStore::new(storage.pool());

    let candidate = submit_candidate(&store, agent, base.id, vec![run], RiskClass::Low).await;
    assert_eq!(
        candidate.state,
        CandidateState::StaticChecked,
        "a submitted candidate is already static checked"
    );

    let stored = store
        .get(agent, candidate.id)
        .await
        .expect("get")
        .expect("candidate");
    assert_eq!(stored.state, CandidateState::StaticChecked);

    // Skipping the experiment phase is illegal.
    let error = store
        .transition(candidate.id, CandidateState::Evaluated)
        .await
        .expect_err("cannot skip experimenting");
    assert!(matches!(error, SilynError::InvalidTransition { .. }));
    assert!(!error.is_retryable());

    // Going backwards is illegal.
    let moved = store
        .transition(candidate.id, CandidateState::Experimenting)
        .await
        .expect("experimenting");
    assert_eq!(moved.state, CandidateState::Experimenting);
    let error = store
        .transition(candidate.id, CandidateState::Proposed)
        .await
        .expect_err("cannot go back to proposed");
    assert!(matches!(error, SilynError::InvalidTransition { .. }));

    // Repeating the current state is a no-op, not a transition.
    let again = store
        .transition(candidate.id, CandidateState::Experimenting)
        .await
        .expect("idempotent");
    assert_eq!(again.state, CandidateState::Experimenting);

    // A rejected candidate is terminal.
    let rejected = store
        .transition(candidate.id, CandidateState::Rejected)
        .await
        .expect("reject");
    assert_eq!(rejected.state, CandidateState::Rejected);
    let error = store
        .transition(candidate.id, CandidateState::Experimenting)
        .await
        .expect_err("rejected is terminal");
    assert!(matches!(error, SilynError::InvalidTransition { .. }));

    // Candidates stay scoped to their agent.
    let other = insert_agent(&storage, "user-b").await;
    assert!(store.get(other, candidate.id).await.expect("get").is_none());
}

// ---------------------------------------------------------------------------
// Promotion and rollback
// ---------------------------------------------------------------------------

#[tokio::test]
async fn low_risk_auto_promotes() {
    let (storage, agent) = setup().await;
    let base = seed_policy(&storage, agent).await;
    let run = evidence_run(&storage, agent).await;
    let candidates = CandidateStore::new(storage.pool());
    let promotions = PromotionStore::new(storage.pool());

    let candidate = submit_candidate(&candidates, agent, base.id, vec![run], RiskClass::Low).await;
    advance_to_evaluated(&candidates, candidate.id).await;

    let report = promotion_report(candidate.id, vec![run], 12.0, 4.0);
    promotions
        .record_rollback_drill(agent, candidate.id, "operator:test")
        .await
        .expect("record drill");

    let decision = promotions.decide(agent, &report).await.expect("decide");
    assert!(
        matches!(decision, PromotionDecision::Promote),
        "low risk with all conditions met must auto-promote: {decision:?}"
    );

    let promotion = promotions
        .promote(agent, candidate.id, "gate:low-risk")
        .await
        .expect("promote");
    assert_eq!(promotion.from_version, base.id);
    assert_ne!(promotion.to_version, base.id);
    assert_eq!(promotion.decision, "promote");
    assert!(promotion.rolled_back_at.is_none());

    let active = PolicyStore::new(storage.pool())
        .active(agent)
        .await
        .expect("active")
        .expect("some");
    assert_eq!(active.id, promotion.to_version);
    assert_eq!(active.parent_version, Some(base.id));
    assert_eq!(
        active.content.planner.rules.len(),
        2,
        "the promoted artifact carries the patched rules"
    );
    assert_eq!(
        active.content.tools.allowlist, base.content.tools.allowlist,
        "tools are never touched by a candidate"
    );

    let reloaded = candidates
        .get(agent, candidate.id)
        .await
        .expect("get")
        .expect("candidate");
    assert_eq!(reloaded.state, CandidateState::Promoted);

    let recorded = promotions
        .get(promotion.id)
        .await
        .expect("get promotion")
        .expect("promotion");
    assert_eq!(recorded, promotion);
    assert_eq!(promotions.list(agent).await.expect("list").len(), 1);
}

#[tokio::test]
async fn default_is_deny() {
    let (storage, agent) = setup().await;
    let base = seed_policy(&storage, agent).await;
    let run = evidence_run(&storage, agent).await;
    let candidates = CandidateStore::new(storage.pool());
    let promotions = PromotionStore::new(storage.pool());

    // Medium risk: a valid, passing report still waits for an operator.
    let medium = submit_candidate(&candidates, agent, base.id, vec![run], RiskClass::Medium).await;
    advance_to_evaluated(&candidates, medium.id).await;
    promotions
        .record_rollback_drill(agent, medium.id, "operator:test")
        .await
        .expect("record drill");
    let decision = promotions
        .decide(agent, &promotion_report(medium.id, vec![run], 12.0, 4.0))
        .await
        .expect("decide");
    assert!(matches!(decision, PromotionDecision::AwaitApproval));
    let stored = candidates
        .get(agent, medium.id)
        .await
        .expect("get")
        .expect("candidate");
    assert_eq!(stored.state, CandidateState::AwaitingApproval);

    // Low risk without a rollback drill also waits.
    let no_drill = submit_candidate(&candidates, agent, base.id, vec![run], RiskClass::Low).await;
    advance_to_evaluated(&candidates, no_drill.id).await;
    let decision = promotions
        .decide(agent, &promotion_report(no_drill.id, vec![run], 12.0, 4.0))
        .await
        .expect("decide");
    assert!(matches!(decision, PromotionDecision::AwaitApproval));

    // Low risk with unauthorized actions waits.
    let unauthorized =
        submit_candidate(&candidates, agent, base.id, vec![run], RiskClass::Low).await;
    advance_to_evaluated(&candidates, unauthorized.id).await;
    promotions
        .record_rollback_drill(agent, unauthorized.id, "operator:test")
        .await
        .expect("record drill");
    let mut report = promotion_report(unauthorized.id, vec![run], 12.0, 4.0);
    report.unauthorized_actions = 1;
    let report = report.seal().expect("seal");
    let decision = promotions.decide(agent, &report).await.expect("decide");
    assert!(matches!(decision, PromotionDecision::AwaitApproval));

    // Low risk below the pre-registered gain waits.
    let small_gain = submit_candidate(&candidates, agent, base.id, vec![run], RiskClass::Low).await;
    advance_to_evaluated(&candidates, small_gain.id).await;
    promotions
        .record_rollback_drill(agent, small_gain.id, "operator:test")
        .await
        .expect("record drill");
    let decision = promotions
        .decide(agent, &promotion_report(small_gain.id, vec![run], 2.0, 0.5))
        .await
        .expect("decide");
    assert!(matches!(decision, PromotionDecision::AwaitApproval));

    // A tampered report is a hard failure and rejects the candidate.
    let tampered = submit_candidate(&candidates, agent, base.id, vec![run], RiskClass::Low).await;
    advance_to_evaluated(&candidates, tampered.id).await;
    promotions
        .record_rollback_drill(agent, tampered.id, "operator:test")
        .await
        .expect("record drill");
    let mut report = promotion_report(tampered.id, vec![run], 12.0, 4.0);
    report.gain_pp = 90.0;
    let decision = promotions.decide(agent, &report).await.expect("decide");
    assert!(
        matches!(decision, PromotionDecision::Reject { .. }),
        "a checksum mismatch must reject: {decision:?}"
    );
    let stored = candidates
        .get(agent, tampered.id)
        .await
        .expect("get")
        .expect("candidate");
    assert_eq!(stored.state, CandidateState::Rejected);

    // A report with evidence the candidate never declared is rejected too.
    let undeclared = submit_candidate(&candidates, agent, base.id, vec![run], RiskClass::Low).await;
    advance_to_evaluated(&candidates, undeclared.id).await;
    let other_run = evidence_run(&storage, agent).await;
    let decision = promotions
        .decide(
            agent,
            &promotion_report(undeclared.id, vec![other_run], 12.0, 4.0),
        )
        .await
        .expect("decide");
    assert!(matches!(decision, PromotionDecision::Reject { .. }));

    // Nothing was promoted and the pointer never moved.
    assert!(promotions.list(agent).await.expect("list").is_empty());
    let active = PolicyStore::new(storage.pool())
        .active(agent)
        .await
        .expect("active")
        .expect("some");
    assert_eq!(active.id, base.id);
    assert_eq!(active.content, base.content);

    // Promotion is refused for a candidate that never passed the gate.
    let error = promotions
        .promote(agent, medium.id, "operator:test")
        .await
        .expect_err("awaiting approval cannot be promoted");
    assert!(!error.is_retryable());
}

#[tokio::test]
async fn rollback_restores_previous() {
    let (storage, agent) = setup().await;
    let base = seed_policy(&storage, agent).await;
    let run = evidence_run(&storage, agent).await;
    let candidates = CandidateStore::new(storage.pool());
    let promotions = PromotionStore::new(storage.pool());

    let candidate = submit_candidate(&candidates, agent, base.id, vec![run], RiskClass::Low).await;
    advance_to_evaluated(&candidates, candidate.id).await;
    promotions
        .record_rollback_drill(agent, candidate.id, "operator:test")
        .await
        .expect("record drill");
    promotions
        .decide(agent, &promotion_report(candidate.id, vec![run], 12.0, 4.0))
        .await
        .expect("decide");
    let promotion = promotions
        .promote(agent, candidate.id, "gate:low-risk")
        .await
        .expect("promote");

    let restored = promotions
        .rollback(promotion.id, "operator:rollback")
        .await
        .expect("rollback");
    assert_eq!(restored.id, base.id);
    assert_eq!(restored.content, base.content);

    let active = PolicyStore::new(storage.pool())
        .active(agent)
        .await
        .expect("active")
        .expect("some");
    assert_eq!(active.id, base.id);

    let recorded = promotions
        .get(promotion.id)
        .await
        .expect("get")
        .expect("promotion");
    assert!(recorded.rolled_back_at.is_some());

    let rolled_back = candidates
        .get(agent, candidate.id)
        .await
        .expect("get")
        .expect("candidate");
    assert_eq!(rolled_back.state, CandidateState::RolledBack);

    let audit: (String, String) = sqlx::query_as(
        "SELECT actor, decision FROM audit_events
         WHERE action = 'promotion.rollback' AND subject = ?1",
    )
    .bind(candidate.id.to_string())
    .fetch_one(storage.pool())
    .await
    .expect("rollback audit row");
    assert_eq!(audit.0, "operator:rollback");
    assert_eq!(audit.1, "allow");

    let error = promotions
        .rollback(promotion.id, "operator:rollback")
        .await
        .expect_err("double rollback must fail");
    assert!(
        error.to_string().contains("already rolled back"),
        "unexpected error: {error}"
    );
    assert!(!error.is_retryable());
}

#[tokio::test]
async fn audit_chain_complete() {
    let (storage, agent) = setup().await;
    let base = seed_policy(&storage, agent).await;
    let run = evidence_run(&storage, agent).await;
    let candidates = CandidateStore::new(storage.pool());
    let promotions = PromotionStore::new(storage.pool());

    let candidate = submit_candidate(&candidates, agent, base.id, vec![run], RiskClass::Low).await;
    advance_to_evaluated(&candidates, candidate.id).await;
    promotions
        .record_rollback_drill(agent, candidate.id, "operator:drill")
        .await
        .expect("record drill");
    let report = promotion_report(candidate.id, vec![run], 12.0, 4.0);
    promotions.decide(agent, &report).await.expect("decide");
    let promotion = promotions
        .promote(agent, candidate.id, "gate:low-risk")
        .await
        .expect("promote");

    let rows: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT action, actor, decision, details_ref FROM audit_events
         WHERE subject = ?1 ORDER BY id ASC",
    )
    .bind(candidate.id.to_string())
    .fetch_all(storage.pool())
    .await
    .expect("audit rows");

    let actions: Vec<&str> = rows.iter().map(|row| row.0.as_str()).collect();
    assert!(
        actions.contains(&"promotion.rollback_drill"),
        "missing drill audit: {actions:?}"
    );
    assert!(
        actions.contains(&"promotion.decide"),
        "missing decision audit: {actions:?}"
    );
    assert!(
        actions.contains(&"promotion.promote"),
        "missing promotion audit: {actions:?}"
    );

    let promote_row = rows
        .iter()
        .find(|row| row.0 == "promotion.promote")
        .expect("promote audit row");
    assert_eq!(promote_row.1, "gate:low-risk");
    assert_eq!(promote_row.2, "allow");
    let details: serde_json::Value =
        serde_json::from_str(promote_row.3.as_deref().expect("details")).expect("details json");
    assert_eq!(details["promotion_id"], promotion.id.to_string());
    assert_eq!(details["from_version"], base.id.to_string());
    assert_eq!(details["to_version"], promotion.to_version.to_string());

    let drill = rows
        .iter()
        .find(|row| row.0 == "promotion.rollback_drill")
        .expect("drill audit row");
    assert_eq!(drill.1, "operator:drill");
    assert_eq!(drill.2, "pass");
    let drill_details: serde_json::Value =
        serde_json::from_str(drill.3.as_deref().expect("drill details")).expect("drill json");
    assert_eq!(drill_details["base_policy_version"], base.id.to_string());
    assert_eq!(drill_details["checksum"], base.checksum);

    // The promotions row carries the same actor and version pair.
    let stored: (String, String, String) = sqlx::query_as(
        "SELECT approved_by, from_version, to_version FROM promotions WHERE id = ?1",
    )
    .bind(promotion.id.to_string())
    .fetch_one(storage.pool())
    .await
    .expect("promotion row");
    assert_eq!(stored.0, "gate:low-risk");
    assert_eq!(stored.1, base.id.to_string());
    assert_eq!(stored.2, promotion.to_version.to_string());
}

// ---------------------------------------------------------------------------
// Extra guards (not gated)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn promotion_requires_an_evaluated_candidate() {
    let (storage, agent) = setup().await;
    let base = seed_policy(&storage, agent).await;
    let run = evidence_run(&storage, agent).await;
    let candidates = CandidateStore::new(storage.pool());
    let promotions = PromotionStore::new(storage.pool());

    let candidate = submit_candidate(&candidates, agent, base.id, vec![run], RiskClass::Low).await;
    promotions
        .record_rollback_drill(agent, candidate.id, "operator:test")
        .await
        .expect("record drill");

    let decision = promotions
        .decide(agent, &promotion_report(candidate.id, vec![run], 12.0, 4.0))
        .await
        .expect("decide");
    assert!(matches!(decision, PromotionDecision::Reject { .. }));
    let stored = candidates
        .get(agent, candidate.id)
        .await
        .expect("get")
        .expect("candidate");
    assert_eq!(stored.state, CandidateState::Rejected);
}

#[tokio::test]
async fn report_verdict_reject_waits_for_an_operator() {
    let (storage, agent) = setup().await;
    let base = seed_policy(&storage, agent).await;
    let run = evidence_run(&storage, agent).await;
    let candidates = CandidateStore::new(storage.pool());
    let promotions = PromotionStore::new(storage.pool());

    let candidate = submit_candidate(&candidates, agent, base.id, vec![run], RiskClass::Low).await;
    advance_to_evaluated(&candidates, candidate.id).await;
    promotions
        .record_rollback_drill(agent, candidate.id, "operator:test")
        .await
        .expect("record drill");

    let mut report = promotion_report(candidate.id, vec![run], 12.0, 4.0);
    report.verdict = VERDICT_REJECT.to_owned();
    let report = report.seal().expect("seal");
    let decision = promotions.decide(agent, &report).await.expect("decide");
    assert!(
        matches!(decision, PromotionDecision::AwaitApproval),
        "a failed evaluation is a soft condition: {decision:?}"
    );
}

#[tokio::test]
async fn drill_from_another_base_does_not_count() {
    let (storage, agent) = setup().await;
    let base = seed_policy(&storage, agent).await;
    let run = evidence_run(&storage, agent).await;
    let candidates = CandidateStore::new(storage.pool());
    let promotions = PromotionStore::new(storage.pool());

    let candidate = submit_candidate(&candidates, agent, base.id, vec![run], RiskClass::Low).await;
    advance_to_evaluated(&candidates, candidate.id).await;
    promotions
        .record_rollback_drill(agent, candidate.id, "operator:test")
        .await
        .expect("record drill");

    // Someone edits the drill row to name a different base: it must stop
    // counting instead of being trusted.
    sqlx::query(
        "UPDATE audit_events SET details_ref = ?2
         WHERE action = 'promotion.rollback_drill' AND subject = ?1",
    )
    .bind(candidate.id.to_string())
    .bind(
        json!({"base_policy_version": PolicyVersionId::new().to_string(), "checksum": "00"})
            .to_string(),
    )
    .execute(storage.pool())
    .await
    .expect("tamper drill");

    let decision = promotions
        .decide(agent, &promotion_report(candidate.id, vec![run], 12.0, 4.0))
        .await
        .expect("decide");
    assert!(matches!(decision, PromotionDecision::AwaitApproval));
}

#[tokio::test]
async fn submit_rejects_inactive_base_version() {
    let (storage, agent) = setup().await;
    let base = seed_policy(&storage, agent).await;
    let run = evidence_run(&storage, agent).await;
    let candidates = CandidateStore::new(storage.pool());

    let mut newer = base_policy();
    newer.planner.rules.push("Check units first.".to_owned());
    let next = PolicyStore::new(storage.pool())
        .create(agent, Some(base.id), &newer)
        .await
        .expect("newer version");
    activate(&storage, agent, next.id).await;

    let error = candidates
        .submit(agent, candidate_patch(base.id, vec![run]))
        .await
        .expect_err("stale base version");
    assert!(matches!(error, SilynError::PolicyDenied(_)), "{error:?}");
}

#[tokio::test]
async fn promotion_report_checksum_is_deterministic() {
    let candidate = CandidateId::new();
    let run = RunId::new();
    let first = promotion_report(candidate, vec![run], 12.0, 4.0);
    let second = promotion_report(candidate, vec![run], 12.0, 4.0);
    assert_eq!(first.report_checksum, second.report_checksum);
    assert!(first.checksum_matches().expect("checksum"));
    assert_ne!(VERDICT_PROMOTE, VERDICT_REJECT);
}
