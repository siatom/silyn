//! The promotion gate: evaluation-report decisions, audited promotion and
//! audited rollback (ADR 0008).
//!
//! Default deny. A candidate is promoted automatically only when it is low
//! risk, stays inside the patch whitelist, carries a passing report with a
//! positive confidence bound, cites verified terminal evidence, matches its
//! report checksum and has a successful rollback drill recorded against the same
//! base policy. Everything else moves to `AwaitingApproval`; only a structurally
//! invalid report, unusable evidence, a stale base policy or a checksum mismatch
//! is `Reject`ed.
//!
//! Promotion and rollback each run in one transaction: the immutable version,
//! the `agents.active_policy_version` pointer, the `promotions` row, the
//! candidate state and the `audit_events` row either all land or none do.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use silyn_core::SilynError;
use silyn_core::contracts::{PromotionDecision, PromotionPolicy};
use silyn_core::ids::{AgentId, CandidateId, PolicyVersionId, PromotionId, RunId};
use silyn_core::state::{CandidateState, LifecycleState};
use silyn_storage::db_error;
use silyn_storage::error::parse_id;
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqliteConnection, SqlitePool};

use crate::candidate::{
    CandidateRecord, RiskClass, apply_patch, load_candidate, update_state, validate_patch,
    verify_evidence_run,
};
use crate::policy::{
    PolicyVersionRecord, active_policy_version, canonical_json_string, create_version,
    load_version, set_active_policy_version, sha256_hex,
};

/// Verdict string a report uses when the candidate should be promoted.
pub const VERDICT_PROMOTE: &str = "promote";
/// Verdict string a report uses when the candidate lost the evaluation.
pub const VERDICT_REJECT: &str = "reject";

/// Pre-registered minimum gain (`csv-stats-v1` thresholds, ADR 0004).
pub const DEFAULT_MIN_GAIN_PP: f64 = 10.0;
/// Pre-registered cost ceiling, kept for `PromotionPolicy` fidelity.
pub const DEFAULT_MAX_COST_RATIO: f64 = 2.0;
/// Pre-registered regression ceiling, kept for `PromotionPolicy` fidelity.
pub const DEFAULT_MAX_REGRESSION_PP: f64 = 5.0;

/// Audit action recorded by [`PromotionStore::record_rollback_drill`].
pub const AUDIT_ROLLBACK_DRILL: &str = "promotion.rollback_drill";
/// Audit action recorded by [`PromotionStore::decide`].
pub const AUDIT_DECIDE: &str = "promotion.decide";
/// Audit action recorded by [`PromotionStore::promote`].
pub const AUDIT_PROMOTE: &str = "promotion.promote";
/// Audit action recorded by [`PromotionStore::rollback`].
pub const AUDIT_ROLLBACK: &str = "promotion.rollback";

/// Actor used for gate decisions that no operator triggered.
pub const GATE_ACTOR: &str = "promotion-gate";
/// Decision stored on a `promotions` row created by [`PromotionStore::promote`].
pub const PROMOTION_DECISION: &str = "promote";

/// The evaluation evidence the gate consumes (a projection of the W5-01
/// report). `promote` only needs this; the full report stays in W5-01.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromotionReport {
    pub candidate_id: CandidateId,
    /// `promote` or `reject`, lowercase.
    pub verdict: String,
    /// Point estimate of the gain in percentage points.
    pub gain_pp: f64,
    /// Lower bound of the 95% paired bootstrap interval, in percentage points.
    pub ci_low_pp: f64,
    pub unauthorized_actions: u64,
    pub evidence_run_ids: Vec<RunId>,
    pub suite_version: String,
    /// sha256 of the report this projection came from.
    pub report_checksum: String,
}

/// One stored promotion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromotionRecord {
    pub id: PromotionId,
    pub candidate_id: CandidateId,
    pub agent_id: AgentId,
    pub from_version: PolicyVersionId,
    pub to_version: PolicyVersionId,
    pub approved_by: String,
    pub decision: String,
    pub reason: Option<String>,
    pub rolled_back_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl PromotionReport {
    /// Recomputes the checksum over the report with the checksum field blank.
    ///
    /// The gate compares this with the supplied `report_checksum`, so editing
    /// the verdict or gain after the report was sealed is detected.
    pub fn compute_checksum(&self) -> Result<String, SilynError> {
        let mut unsigned = self.clone();
        unsigned.report_checksum = String::new();
        Ok(sha256_hex(canonical_json_string(&unsigned)?.as_bytes()))
    }

    /// Seals the report with its own checksum.
    pub fn seal(mut self) -> Result<Self, SilynError> {
        self.report_checksum = self.compute_checksum()?;
        Ok(self)
    }

    /// True when `report_checksum` matches the current content.
    pub fn checksum_matches(&self) -> Result<bool, SilynError> {
        Ok(!self.report_checksum.is_empty() && self.compute_checksum()? == self.report_checksum)
    }
}

/// Gate decisions, promotions and rollbacks for one SQLite pool.
pub struct PromotionStore<'a> {
    pool: &'a SqlitePool,
    policy: PromotionPolicy,
}

impl<'a> PromotionStore<'a> {
    /// Uses the pre-registered thresholds and enables low-risk auto promotion.
    #[must_use]
    pub fn new(pool: &'a SqlitePool) -> Self {
        Self::with_policy(
            pool,
            PromotionPolicy {
                allow_low_risk_auto: true,
                min_gain_pp: DEFAULT_MIN_GAIN_PP,
                max_cost_ratio: DEFAULT_MAX_COST_RATIO,
                max_regression_pp: DEFAULT_MAX_REGRESSION_PP,
            },
        )
    }

    /// Overrides the thresholds, for experiments and tests.
    #[must_use]
    pub fn with_policy(pool: &'a SqlitePool, policy: PromotionPolicy) -> Self {
        Self { pool, policy }
    }

    /// Runs the gate on one report.
    ///
    /// Hard failures (checksum, evidence, base policy, unusable patch) reject
    /// the candidate. A valid report that does not clear every auto-promotion
    /// condition leaves the candidate in `AwaitingApproval`; with no operator it
    /// stays there, which is the default-deny path. Only a candidate in
    /// `Evaluated` can be decided; a report for a candidate that was already
    /// decided is a permanent error.
    pub async fn decide(
        &self,
        agent_id: AgentId,
        report: &PromotionReport,
    ) -> Result<PromotionDecision, SilynError> {
        let mut transaction = self.pool.begin().await.map_err(db_error)?;
        let candidate = load_candidate(&mut transaction, report.candidate_id)
            .await?
            .ok_or_else(|| {
                SilynError::Permanent(format!("candidate {} not found", report.candidate_id))
            })?;
        if candidate.agent_id != agent_id {
            return Err(SilynError::PolicyDenied(format!(
                "candidate {} does not belong to agent {agent_id}",
                report.candidate_id
            )));
        }

        let (hard, soft, base) = self
            .gate_failures(agent_id, &candidate, report, &mut transaction)
            .await?;
        let now = Utc::now();

        if !hard.is_empty() {
            transition_if_legal(&mut transaction, &candidate, CandidateState::Rejected, now)
                .await?;
            record_audit(
                &mut transaction,
                GATE_ACTOR,
                AUDIT_DECIDE,
                candidate.id,
                "deny",
                Some(&serde_json::json!({
                    "decision": "reject",
                    "report_checksum": report.report_checksum,
                    "suite_version": report.suite_version,
                    "reasons": &hard,
                })),
                now,
            )
            .await?;
            transaction.commit().await.map_err(db_error)?;
            return Ok(PromotionDecision::Reject {
                reason: hard.join("; "),
            });
        }

        match candidate.state {
            CandidateState::Evaluated => {}
            CandidateState::Proposed
            | CandidateState::StaticChecked
            | CandidateState::Experimenting => {
                // No evaluation preceded this report: a hard failure.
                let reason = format!(
                    "candidate {} was never evaluated (state {:?})",
                    candidate.id, candidate.state
                );
                transition_if_legal(&mut transaction, &candidate, CandidateState::Rejected, now)
                    .await?;
                record_audit(
                    &mut transaction,
                    GATE_ACTOR,
                    AUDIT_DECIDE,
                    candidate.id,
                    "deny",
                    Some(&serde_json::json!({
                        "decision": "reject",
                        "report_checksum": report.report_checksum,
                        "reasons": [&reason],
                    })),
                    now,
                )
                .await?;
                transaction.commit().await.map_err(db_error)?;
                return Ok(PromotionDecision::Reject { reason });
            }
            other => {
                return Err(SilynError::Permanent(format!(
                    "candidate {} is in state {other:?} and can no longer be decided",
                    candidate.id
                )));
            }
        }

        record_evaluation(&mut transaction, &candidate, report, now).await?;

        let decision = if soft.is_empty() {
            update_state(
                &mut transaction,
                candidate.id,
                CandidateState::Evaluated,
                CandidateState::Approved,
                now,
            )
            .await?;
            PromotionDecision::Promote
        } else {
            update_state(
                &mut transaction,
                candidate.id,
                CandidateState::Evaluated,
                CandidateState::AwaitingApproval,
                now,
            )
            .await?;
            PromotionDecision::AwaitApproval
        };

        record_audit(
            &mut transaction,
            GATE_ACTOR,
            AUDIT_DECIDE,
            candidate.id,
            match decision {
                PromotionDecision::Promote => "allow",
                _ => "await_approval",
            },
            Some(&serde_json::json!({
                "decision": match decision {
                    PromotionDecision::Promote => "promote",
                    _ => "await_approval",
                },
                "report_checksum": report.report_checksum,
                "suite_version": report.suite_version,
                "base_policy_version": base.map(|base| base.id.to_string()),
                "reasons": soft,
            })),
            now,
        )
        .await?;
        transaction.commit().await.map_err(db_error)?;
        Ok(decision)
    }

    /// Activates the candidate's patched policy.
    ///
    /// Requires an `Approved` candidate, a still-active base version and a
    /// stored evaluation. Creates the immutable patched version, switches the
    /// agent pointer, writes the `promotions` row and audits everything in one
    /// transaction.
    pub async fn promote(
        &self,
        agent_id: AgentId,
        candidate_id: CandidateId,
        approved_by: &str,
    ) -> Result<PromotionRecord, SilynError> {
        require_actor(approved_by)?;

        let mut transaction = self.pool.begin().await.map_err(db_error)?;
        let candidate = load_candidate(&mut transaction, candidate_id)
            .await?
            .ok_or_else(|| SilynError::Permanent(format!("candidate {candidate_id} not found")))?;
        if candidate.agent_id != agent_id {
            return Err(SilynError::PolicyDenied(format!(
                "candidate {candidate_id} does not belong to agent {agent_id}"
            )));
        }
        if candidate.state != CandidateState::Approved {
            return Err(SilynError::InvalidTransition {
                from: format!("{:?}", candidate.state),
                to: "approved".to_owned(),
            });
        }

        let base = load_version(&mut transaction, agent_id, candidate.base_policy_version)
            .await?
            .ok_or_else(|| {
                SilynError::Permanent(format!(
                    "base policy version {} not found",
                    candidate.base_policy_version
                ))
            })?;
        match active_policy_version(&mut transaction, agent_id).await? {
            Some(active) if active == base.id => {}
            Some(_) => {
                return Err(SilynError::Permanent(format!(
                    "base policy version {} is no longer active",
                    base.id
                )));
            }
            None => {
                return Err(SilynError::Permanent(format!(
                    "agent {agent_id} has no active policy version"
                )));
            }
        }

        let evaluated: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM evaluation_runs WHERE candidate_id = ?1")
                .bind(candidate.id.to_string())
                .fetch_one(&mut *transaction)
                .await
                .map_err(db_error)?;
        if evaluated == 0 {
            return Err(SilynError::Permanent(format!(
                "candidate {candidate_id} has no evaluation run"
            )));
        }

        let patched = apply_patch(&base.content, &candidate.patch.operations)?;
        let version = create_version(&mut transaction, agent_id, Some(base.id), &patched).await?;
        if version.checksum == base.checksum {
            return Err(SilynError::Permanent(format!(
                "candidate {candidate_id} does not change the policy"
            )));
        }

        set_active_policy_version(&mut transaction, agent_id, version.id).await?;
        update_state(
            &mut transaction,
            candidate.id,
            CandidateState::Approved,
            CandidateState::Promoted,
            Utc::now(),
        )
        .await?;

        let now = Utc::now();
        let promotion_id = PromotionId::new();
        sqlx::query(
            "INSERT INTO promotions (
                id, candidate_id, agent_id, from_version, to_version, approved_by,
                decision, reason, rolled_back_at, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )
        .bind(promotion_id.to_string())
        .bind(candidate.id.to_string())
        .bind(agent_id.to_string())
        .bind(base.id.to_string())
        .bind(version.id.to_string())
        .bind(approved_by)
        .bind(PROMOTION_DECISION)
        .bind(Option::<String>::None)
        .bind(Option::<DateTime<Utc>>::None)
        .bind(now)
        .execute(&mut *transaction)
        .await
        .map_err(db_error)?;

        record_audit(
            &mut transaction,
            approved_by,
            AUDIT_PROMOTE,
            candidate.id,
            "allow",
            Some(&serde_json::json!({
                "promotion_id": promotion_id.to_string(),
                "from_version": base.id.to_string(),
                "to_version": version.id.to_string(),
                "to_checksum": version.checksum,
            })),
            now,
        )
        .await?;
        transaction.commit().await.map_err(db_error)?;

        Ok(PromotionRecord {
            id: promotion_id,
            candidate_id: candidate.id,
            agent_id,
            from_version: base.id,
            to_version: version.id,
            approved_by: approved_by.to_owned(),
            decision: PROMOTION_DECISION.to_owned(),
            reason: None,
            rolled_back_at: None,
            created_at: now,
        })
    }

    /// Restores the version the promotion replaced.
    ///
    /// The promotion must still be the active policy and must not have been
    /// rolled back already; either violation is permanent. Writes the pointer,
    /// `rolled_back_at`, the candidate state and the audit row together.
    pub async fn rollback(
        &self,
        promotion_id: PromotionId,
        actor: &str,
    ) -> Result<PolicyVersionRecord, SilynError> {
        require_actor(actor)?;

        let mut transaction = self.pool.begin().await.map_err(db_error)?;
        let promotion = load_promotion(&mut transaction, promotion_id)
            .await?
            .ok_or_else(|| SilynError::Permanent(format!("promotion {promotion_id} not found")))?;
        if promotion.rolled_back_at.is_some() {
            return Err(SilynError::Permanent(format!(
                "promotion {promotion_id} was already rolled back"
            )));
        }
        if promotion.decision != PROMOTION_DECISION {
            return Err(SilynError::Permanent(format!(
                "promotion {promotion_id} is not an active promotion"
            )));
        }
        match active_policy_version(&mut transaction, promotion.agent_id).await? {
            Some(active) if active == promotion.to_version => {}
            Some(_) => {
                return Err(SilynError::Permanent(format!(
                    "promotion {promotion_id} is not the active policy any more"
                )));
            }
            None => {
                return Err(SilynError::Permanent(format!(
                    "agent {} has no active policy version",
                    promotion.agent_id
                )));
            }
        }

        let restored = load_version(&mut transaction, promotion.agent_id, promotion.from_version)
            .await?
            .ok_or_else(|| {
                SilynError::Permanent(format!(
                    "previous policy version {} is missing",
                    promotion.from_version
                ))
            })?;

        set_active_policy_version(&mut transaction, promotion.agent_id, restored.id).await?;

        let now = Utc::now();
        sqlx::query("UPDATE promotions SET rolled_back_at = ?2 WHERE id = ?1")
            .bind(promotion_id.to_string())
            .bind(now)
            .execute(&mut *transaction)
            .await
            .map_err(db_error)?;

        let candidate = load_candidate(&mut transaction, promotion.candidate_id)
            .await?
            .ok_or_else(|| {
                SilynError::Permanent(format!("candidate {} not found", promotion.candidate_id))
            })?;
        update_state(
            &mut transaction,
            candidate.id,
            candidate.state,
            CandidateState::RolledBack,
            now,
        )
        .await?;

        record_audit(
            &mut transaction,
            actor,
            AUDIT_ROLLBACK,
            candidate.id,
            "allow",
            Some(&serde_json::json!({
                "promotion_id": promotion_id.to_string(),
                "from_version": promotion.to_version.to_string(),
                "to_version": restored.id.to_string(),
                "restored_checksum": restored.checksum,
            })),
            now,
        )
        .await?;
        transaction.commit().await.map_err(db_error)?;

        Ok(restored)
    }

    /// Records a successful rollback drill for a candidate.
    ///
    /// The drill is an audited event rather than a column: it names the actor,
    /// the base policy version and that version's checksum, so the gate can
    /// refuse a drill recorded against a different policy. Nothing is assumed —
    /// without this call the gate never auto-promotes.
    pub async fn record_rollback_drill(
        &self,
        agent_id: AgentId,
        candidate_id: CandidateId,
        actor: &str,
    ) -> Result<(), SilynError> {
        require_actor(actor)?;

        let mut transaction = self.pool.begin().await.map_err(db_error)?;
        let candidate = load_candidate(&mut transaction, candidate_id)
            .await?
            .ok_or_else(|| SilynError::Permanent(format!("candidate {candidate_id} not found")))?;
        if candidate.agent_id != agent_id {
            return Err(SilynError::PolicyDenied(format!(
                "candidate {candidate_id} does not belong to agent {agent_id}"
            )));
        }
        match candidate.state {
            CandidateState::StaticChecked
            | CandidateState::Experimenting
            | CandidateState::Evaluated
            | CandidateState::AwaitingApproval
            | CandidateState::Approved => {}
            other => {
                return Err(SilynError::Permanent(format!(
                    "candidate {candidate_id} is in state {other:?}; a rollback drill needs a pre-promotion candidate"
                )));
            }
        }

        let base = load_version(&mut transaction, agent_id, candidate.base_policy_version)
            .await?
            .ok_or_else(|| {
                SilynError::Permanent(format!(
                    "base policy version {} not found",
                    candidate.base_policy_version
                ))
            })?;

        let now = Utc::now();
        record_audit(
            &mut transaction,
            actor,
            AUDIT_ROLLBACK_DRILL,
            candidate.id,
            "pass",
            Some(&serde_json::json!({
                "base_policy_version": base.id.to_string(),
                "checksum": base.checksum,
            })),
            now,
        )
        .await?;
        transaction.commit().await.map_err(db_error)?;
        Ok(())
    }

    /// Reads one promotion.
    pub async fn get(
        &self,
        promotion_id: PromotionId,
    ) -> Result<Option<PromotionRecord>, SilynError> {
        let mut connection = self.pool.acquire().await.map_err(db_error)?;
        let promotion = load_promotion(&mut connection, promotion_id).await?;
        Ok(promotion)
    }

    /// Every promotion of one agent, oldest first.
    pub async fn list(&self, agent_id: AgentId) -> Result<Vec<PromotionRecord>, SilynError> {
        let rows = sqlx::query(
            "SELECT id, candidate_id, agent_id, from_version, to_version, approved_by,
                    decision, reason, rolled_back_at, created_at
             FROM promotions WHERE agent_id = ?1 ORDER BY created_at ASC, id ASC",
        )
        .bind(agent_id.to_string())
        .fetch_all(self.pool)
        .await
        .map_err(db_error)?;

        rows.iter().map(promotion_from_row).collect()
    }

    /// Splits the report into hard failures (reject) and unmet soft conditions
    /// (await approval), returning the base version when it loaded.
    async fn gate_failures(
        &self,
        agent_id: AgentId,
        candidate: &CandidateRecord,
        report: &PromotionReport,
        conn: &mut SqliteConnection,
    ) -> Result<(Vec<String>, Vec<String>, Option<PolicyVersionRecord>), SilynError> {
        let mut hard: Vec<String> = Vec::new();
        let mut soft: Vec<String> = Vec::new();

        if report.candidate_id != candidate.id {
            hard.push("report candidate_id does not match the candidate".to_owned());
        }
        if !report.checksum_matches()? {
            hard.push("report_checksum does not match the report content".to_owned());
        }
        if report.suite_version.trim().is_empty() {
            hard.push("report suite_version is empty".to_owned());
        }
        if !report.gain_pp.is_finite() || !report.ci_low_pp.is_finite() {
            hard.push("report gains must be finite".to_owned());
        }
        match report.verdict.to_ascii_lowercase().as_str() {
            VERDICT_PROMOTE | VERDICT_REJECT => {}
            other => hard.push(format!("unknown report verdict `{other}`")),
        }

        if report.evidence_run_ids.is_empty() {
            hard.push("report cites no evidence run".to_owned());
        }
        let declared: std::collections::BTreeSet<RunId> =
            candidate.patch.evidence_run_ids.iter().copied().collect();
        let mut seen = std::collections::BTreeSet::new();
        for run_id in &report.evidence_run_ids {
            if !seen.insert(*run_id) {
                hard.push(format!("evidence run {run_id} is cited twice"));
                continue;
            }
            if !declared.contains(run_id) {
                hard.push(format!(
                    "evidence run {run_id} was not declared by the candidate"
                ));
            }
            if let Err(error) = verify_evidence_run(conn, agent_id, *run_id).await {
                hard.push(format!("evidence run {run_id} is not usable: {error}"));
            }
        }

        if let Err(error) = validate_patch(&candidate.patch) {
            hard.push(format!(
                "stored patch no longer passes static checks: {error}"
            ));
        }

        let base = load_version(conn, agent_id, candidate.base_policy_version).await?;
        let Some(base) = base else {
            hard.push("base policy version does not exist for this agent".to_owned());
            return Ok((hard, soft, None));
        };
        match active_policy_version(conn, agent_id).await? {
            Some(active) if active == base.id => {}
            Some(_) => hard.push(format!(
                "base policy version {} is not the active version",
                base.id
            )),
            None => hard.push("agent has no active policy version".to_owned()),
        }

        match apply_patch(&base.content, &candidate.patch.operations) {
            Ok(patched) => {
                if patched.checksum()? == base.checksum {
                    hard.push("patch does not change the policy".to_owned());
                }
            }
            Err(error) => hard.push(format!(
                "patch cannot be applied to the base policy: {error}"
            )),
        }

        if candidate.patch.risk_class != candidate.risk_class {
            hard.push("stored risk class does not match the patch".to_owned());
        }

        // Soft conditions: a valid report that has not earned auto promotion.
        if candidate.risk_class != RiskClass::Low {
            soft.push(format!(
                "risk class is {:?}, only low risk auto-promotes",
                candidate.risk_class
            ));
        }
        if !self.policy.allow_low_risk_auto {
            soft.push("automatic low-risk promotion is disabled".to_owned());
        }
        if !report.verdict.eq_ignore_ascii_case(VERDICT_PROMOTE) {
            soft.push(format!("evaluation verdict is `{}`", report.verdict));
        }
        if report.gain_pp < self.policy.min_gain_pp {
            soft.push(format!(
                "gain {:.2} pp is below the pre-registered {:.2} pp",
                report.gain_pp, self.policy.min_gain_pp
            ));
        }
        if report.ci_low_pp <= 0.0 {
            soft.push(format!(
                "confidence interval lower bound {:.2} pp is not above zero",
                report.ci_low_pp
            ));
        }
        if report.unauthorized_actions != 0 {
            soft.push(format!(
                "report counted {} unauthorized actions",
                report.unauthorized_actions
            ));
        }
        if !rollback_drill_passed(conn, candidate, &base).await? {
            soft.push("no successful rollback drill is recorded for this base policy".to_owned());
        }

        Ok((hard, soft, Some(base)))
    }
}

fn require_actor(actor: &str) -> Result<(), SilynError> {
    if actor.trim().is_empty() {
        return Err(SilynError::Schema(
            "an operator or actor identity is required".to_owned(),
        ));
    }
    Ok(())
}

/// Moves the candidate when the edge exists; a terminal or already-decided
/// candidate keeps its state and the decision is still reported.
async fn transition_if_legal(
    conn: &mut SqliteConnection,
    candidate: &CandidateRecord,
    to: CandidateState,
    now: DateTime<Utc>,
) -> Result<(), SilynError> {
    if !candidate.state.can_transition_to(to) {
        return Ok(());
    }
    update_state(conn, candidate.id, candidate.state, to, now).await
}

async fn record_evaluation(
    conn: &mut SqliteConnection,
    candidate: &CandidateRecord,
    report: &PromotionReport,
    now: DateTime<Utc>,
) -> Result<(), SilynError> {
    // The report checksum is the natural key: re-deciding the same report is a
    // no-op instead of a duplicate row.
    let metrics = serde_json::json!({
        "verdict": report.verdict,
        "gain_pp": report.gain_pp,
        "ci_low_pp": report.ci_low_pp,
        "unauthorized_actions": report.unauthorized_actions,
        "evidence_run_ids": report.evidence_run_ids,
        "report_checksum": report.report_checksum,
    });

    sqlx::query(
        "INSERT INTO evaluation_runs (
            id, candidate_id, suite_version, baseline_id, metrics_json, report_path, created_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(&report.report_checksum)
    .bind(candidate.id.to_string())
    .bind(&report.suite_version)
    .bind(candidate.base_policy_version.to_string())
    .bind(serde_json::to_string(&metrics)?)
    .bind(Option::<String>::None)
    .bind(now)
    .execute(&mut *conn)
    .await
    .map_err(db_error)?;
    Ok(())
}

/// True when a passing drill names the candidate's base version and checksum.
async fn rollback_drill_passed(
    conn: &mut SqliteConnection,
    candidate: &CandidateRecord,
    base: &PolicyVersionRecord,
) -> Result<bool, SilynError> {
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT decision, details_ref FROM audit_events
         WHERE action = ?1 AND subject = ?2 ORDER BY id DESC",
    )
    .bind(AUDIT_ROLLBACK_DRILL)
    .bind(candidate.id.to_string())
    .fetch_all(&mut *conn)
    .await
    .map_err(db_error)?;

    for (decision, details) in rows {
        if decision != "pass" {
            continue;
        }
        let Some(details) = details else { continue };
        let Ok(details) = serde_json::from_str::<serde_json::Value>(&details) else {
            continue;
        };
        let matches_version = details
            .get("base_policy_version")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| value == base.id.to_string());
        let matches_checksum = details
            .get("checksum")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| value == base.checksum);
        if matches_version && matches_checksum {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn record_audit(
    conn: &mut SqliteConnection,
    actor: &str,
    action: &str,
    subject: CandidateId,
    decision: &str,
    details: Option<&serde_json::Value>,
    occurred_at: DateTime<Utc>,
) -> Result<(), SilynError> {
    let details = details.map(serde_json::to_string).transpose()?;
    sqlx::query(
        "INSERT INTO audit_events (occurred_at, actor, action, subject, decision, details_ref)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(occurred_at)
    .bind(actor)
    .bind(action)
    .bind(subject.to_string())
    .bind(decision)
    .bind(details)
    .execute(&mut *conn)
    .await
    .map_err(db_error)?;
    Ok(())
}

async fn load_promotion(
    conn: &mut SqliteConnection,
    id: PromotionId,
) -> Result<Option<PromotionRecord>, SilynError> {
    let row = sqlx::query(
        "SELECT id, candidate_id, agent_id, from_version, to_version, approved_by,
                decision, reason, rolled_back_at, created_at
         FROM promotions WHERE id = ?1",
    )
    .bind(id.to_string())
    .fetch_optional(&mut *conn)
    .await
    .map_err(db_error)?;

    row.as_ref().map(promotion_from_row).transpose()
}

fn promotion_from_row(row: &SqliteRow) -> Result<PromotionRecord, SilynError> {
    Ok(PromotionRecord {
        id: parse_id(
            &row.try_get::<String, _>("id").map_err(db_error)?,
            "promotions.id",
        )?,
        candidate_id: parse_id(
            &row.try_get::<String, _>("candidate_id").map_err(db_error)?,
            "promotions.candidate_id",
        )?,
        agent_id: parse_id(
            &row.try_get::<String, _>("agent_id").map_err(db_error)?,
            "promotions.agent_id",
        )?,
        from_version: parse_id(
            &row.try_get::<String, _>("from_version").map_err(db_error)?,
            "promotions.from_version",
        )?,
        to_version: parse_id(
            &row.try_get::<String, _>("to_version").map_err(db_error)?,
            "promotions.to_version",
        )?,
        approved_by: row.try_get("approved_by").map_err(db_error)?,
        decision: row.try_get("decision").map_err(db_error)?,
        reason: row.try_get("reason").map_err(db_error)?,
        rolled_back_at: row.try_get("rolled_back_at").map_err(db_error)?,
        created_at: row.try_get("created_at").map_err(db_error)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(gain: f64) -> PromotionReport {
        PromotionReport {
            candidate_id: CandidateId::new(),
            verdict: VERDICT_PROMOTE.to_owned(),
            gain_pp: gain,
            ci_low_pp: gain / 2.0,
            unauthorized_actions: 0,
            evidence_run_ids: vec![RunId::new()],
            suite_version: "csv-stats-v1".to_owned(),
            report_checksum: String::new(),
        }
        .seal()
        .expect("seal report")
    }

    #[test]
    fn sealed_report_is_stable_and_detects_edits() {
        let report = report(12.0);
        let reloaded: PromotionReport =
            serde_json::from_str(&serde_json::to_string(&report).expect("json"))
                .expect("round trip");
        assert_eq!(reloaded, report);
        assert!(report.checksum_matches().expect("checksum"));

        let mut tampered = report.clone();
        tampered.gain_pp = 40.0;
        assert!(!tampered.checksum_matches().expect("checksum"));
    }
}
