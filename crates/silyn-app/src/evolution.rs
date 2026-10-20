//! Evolution service: policy bootstrap, candidate submission, evaluation
//! attachment and the audited promotion/rollback commands.
//!
//! The growth crate owns every state machine and every transactional write.
//! This module only composes those stores, reads the two evaluation JSON
//! documents, and records the operator-facing audit rows that have no store API
//! (`evaluation_runs` persistence and `evolution.approve`).
//!
//! Invariants:
//!
//! - a candidate is only `Evaluated` when a report and its statistics companion
//!   both parsed and agreed; metrics are copied, never invented;
//! - the runtime never reaches `approve_candidate`: it is a local operator
//!   command that names the operator and appends an audit row;
//! - a low-risk auto-promotion still has to pass the W5-02 gate, including a
//!   rollback drill. The drill performed here really creates the patched
//!   version, activates it, confirms the pointer and restores the base before
//!   recording the pass.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use silyn_core::SilynError;
use silyn_core::contracts::PromotionDecision;
use silyn_core::ids::{AgentId, CandidateId, PolicyVersionId, PromotionId, RunId};
use silyn_core::state::CandidateState;
use silyn_growth::{
    CandidateRecord, CandidateStore, ContextPolicy, GATE_ACTOR, PlannerPolicy, PolicyArtifact,
    PolicyPatch, PolicyStore, PolicyVerification, PolicyVersionRecord, PromotionRecord,
    PromotionReport, PromotionStore, RetrievalPolicy, RiskClass, ToolPolicy, apply_patch,
    validate_patch,
};
use silyn_storage::AgentRepository;
use silyn_storage::db_error;
use sqlx::Row;

use crate::app::App;

/// Audit action appended by the local operator approval path.
pub const AUDIT_APPROVE: &str = "evolution.approve";
/// Actor recorded on the rollback drill performed by the promote command.
pub const DRILL_ACTOR: &str = "evolution:rollback-drill";
/// Actor recorded on a CLI rollback (the command has no `--as` flag).
pub const CLI_ACTOR: &str = "operator:cli";

/// The base policy `evolution init` creates for an agent.
///
/// Context and verification follow the defaults in the task package; the
/// planner starts empty so every candidate has room to propose text and no
/// default rule can be mistaken for evaluated guidance.
#[must_use]
pub fn default_policy() -> PolicyArtifact {
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
            prompt_fragments: Vec::new(),
            rules: Vec::new(),
            stop_conditions: serde_json::json!({}),
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

/// Active and newest policy version of one agent.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PolicyInventory {
    pub agent_id: AgentId,
    pub active: Option<PolicyVersionRecord>,
    pub latest: Option<PolicyVersionRecord>,
}

/// One `evaluation_runs` row, as exposed by `evolution show`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EvaluationSummary {
    pub id: String,
    pub suite_version: String,
    pub baseline_id: String,
    pub report_path: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// One `audit_events` row, as exposed by `evolution show`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AuditEventRecord {
    pub occurred_at: DateTime<Utc>,
    pub actor: String,
    pub action: String,
    pub decision: Option<String>,
    pub details: Option<serde_json::Value>,
}

/// Everything an operator needs about one candidate.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CandidateDetail {
    pub candidate: CandidateRecord,
    pub evaluations: Vec<EvaluationSummary>,
    pub promotions: Vec<PromotionRecord>,
    pub audit: Vec<AuditEventRecord>,
}

/// Result of attaching an evaluation report to a candidate.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EvaluationAttachment {
    pub candidate_id: CandidateId,
    /// `evaluation_runs.id`, equal to the sealed projection checksum.
    pub evaluation_id: String,
    /// sha256 of the raw report file.
    pub report_sha256: String,
    /// sha256 of the raw statistics companion.
    pub stats_sha256: String,
    pub suite_version: String,
    pub verdict: String,
    pub gain_pp: f64,
    pub ci_low_pp: f64,
    pub unauthorized_actions: u64,
    pub evidence_run_ids: Vec<RunId>,
    /// Checksum the promotion gate verifies over the projection.
    pub report_checksum: String,
    pub state: CandidateState,
    pub report_path: PathBuf,
    pub stats_path: PathBuf,
}

/// Result of `evolution promote`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PromotionResult {
    #[serde(serialize_with = "serialize_decision")]
    pub decision: PromotionDecision,
    pub promotion: Option<PromotionRecord>,
    pub from_version: Option<PolicyVersionId>,
    pub to_version: Option<PolicyVersionId>,
    pub state: CandidateState,
}

/// Result of `evolution rollback`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RollbackResult {
    pub promotion_id: PromotionId,
    pub from_version: PolicyVersionId,
    pub to_version: PolicyVersionId,
}

/// The checksum-keyed metrics document stored in `evaluation_runs.metrics_json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredEvaluation {
    promotion_report: PromotionReport,
    report_sha256: String,
    stats_sha256: String,
    report_path: String,
    stats_path: String,
}

/// The fields of `schemas/eval_report.schema.json` this service consumes.
///
/// The schema is frozen and closed, but the app must not depend on
/// `silyn-eval`; serde ignores the rest of the document while refusing a body
/// that is missing one of these keys.
#[derive(Debug, Deserialize)]
struct ReportDocument {
    schema_version: u32,
    candidate_id: CandidateId,
    suite_version: String,
    baseline_id: String,
    verdict: String,
}

/// The fields of the statistics companion this service consumes.
#[derive(Debug, Deserialize)]
struct StatsDocument {
    schema_version: u32,
    suite_version: String,
    verdict: String,
    unauthorized_actions: u64,
    comparisons: StatsComparisons,
}

#[derive(Debug, Deserialize)]
struct StatsComparisons {
    c_minus_a: PairedGainDocument,
}

#[derive(Debug, Deserialize)]
struct PairedGainDocument {
    point_pp: f64,
    ci_low_pp: f64,
}

impl App {
    /// Creates the default policy for an agent and makes it active.
    ///
    /// Idempotent: an agent that already has an active version keeps it, so a
    /// second `init` can never reset a promoted policy.
    pub async fn init_policy(&self, agent_id: AgentId) -> Result<PolicyVersionRecord, SilynError> {
        self.ensure_agent(agent_id).await?;
        let policies = PolicyStore::new(self.storage().pool());
        if let Some(active) = policies.active(agent_id).await? {
            return Ok(active);
        }
        let record = policies.create(agent_id, None, &default_policy()).await?;
        policies.activate(agent_id, record.id).await
    }

    /// Active and newest policy version of one agent.
    pub async fn list_policies(&self, agent_id: AgentId) -> Result<PolicyInventory, SilynError> {
        self.ensure_agent(agent_id).await?;
        let policies = PolicyStore::new(self.storage().pool());
        Ok(PolicyInventory {
            agent_id,
            active: policies.active(agent_id).await?,
            latest: policies.latest(agent_id).await?,
        })
    }

    /// Every candidate of one agent, oldest first.
    pub async fn list_candidates(
        &self,
        agent_id: AgentId,
    ) -> Result<Vec<CandidateRecord>, SilynError> {
        self.ensure_agent(agent_id).await?;
        CandidateStore::new(self.storage().pool())
            .list(agent_id)
            .await
    }

    /// One candidate, scoped to the agent.
    pub async fn get_candidate(
        &self,
        agent_id: AgentId,
        candidate_id: CandidateId,
    ) -> Result<CandidateRecord, SilynError> {
        CandidateStore::new(self.storage().pool())
            .get(agent_id, candidate_id)
            .await?
            .ok_or_else(|| SilynError::Permanent(format!("candidate {candidate_id} not found")))
    }

    /// Candidate plus its evaluations, promotions and audit trail.
    pub async fn show_candidate(
        &self,
        agent_id: AgentId,
        candidate_id: CandidateId,
    ) -> Result<CandidateDetail, SilynError> {
        let candidate = self.get_candidate(agent_id, candidate_id).await?;
        let promotions = PromotionStore::new(self.storage().pool())
            .list(agent_id)
            .await?
            .into_iter()
            .filter(|promotion| promotion.candidate_id == candidate_id)
            .collect();
        Ok(CandidateDetail {
            evaluations: self.evaluations_for(candidate_id).await?,
            audit: self.audit_for(candidate_id).await?,
            promotions,
            candidate,
        })
    }

    /// Runs static checks and stores a candidate as `StaticChecked`.
    ///
    /// Static rejections are permanent errors (exit 5), including a whitelisted
    /// path violation; ownership and stale-base denials stay policy denials.
    pub async fn submit_candidate(
        &self,
        agent_id: AgentId,
        patch: PolicyPatch,
    ) -> Result<CandidateRecord, SilynError> {
        validate_patch(&patch)
            .map_err(|error| SilynError::Permanent(format!("patch rejected: {error}")))?;
        CandidateStore::new(self.storage().pool())
            .submit(agent_id, patch)
            .await
    }

    /// Attaches a report and its statistics companion to a candidate.
    ///
    /// Both documents are read from disk, checked against each other and the
    /// candidate, and projected into a sealed [`PromotionReport`]. The
    /// projection checksum keys the `evaluation_runs` row, so attaching the same
    /// report twice is a no-op. Unknown, missing or inconsistent fields are
    /// permanent errors: no metric here is ever defaulted.
    pub async fn attach_evaluation(
        &self,
        agent_id: AgentId,
        candidate_id: CandidateId,
        report_path: &Path,
        stats_path: Option<&Path>,
    ) -> Result<EvaluationAttachment, SilynError> {
        let candidate = self.get_candidate(agent_id, candidate_id).await?;

        let report_bytes = std::fs::read(report_path).map_err(|error| {
            SilynError::Storage(format!(
                "cannot read evaluation report {}: {error}",
                report_path.display()
            ))
        })?;
        let report: ReportDocument = serde_json::from_slice(&report_bytes).map_err(|error| {
            SilynError::Schema(format!(
                "invalid evaluation report {}: {error}",
                report_path.display()
            ))
        })?;
        if report.schema_version != 1 {
            return Err(SilynError::Schema(format!(
                "evaluation report schema_version must be 1, got {}",
                report.schema_version
            )));
        }
        if report.candidate_id != candidate_id {
            return Err(SilynError::Schema(format!(
                "evaluation report names candidate {}, not {candidate_id}",
                report.candidate_id
            )));
        }
        if report.suite_version.trim().is_empty() {
            return Err(SilynError::Schema(
                "evaluation report suite_version is empty".to_owned(),
            ));
        }
        if report.baseline_id.trim().is_empty() {
            return Err(SilynError::Schema(
                "evaluation report baseline_id is empty".to_owned(),
            ));
        }
        if !matches!(
            report.verdict.as_str(),
            "promote" | "reject" | "await_approval" | "invalid"
        ) {
            return Err(SilynError::Schema(format!(
                "unknown evaluation verdict `{}`",
                report.verdict
            )));
        }

        let stats_path = stats_path
            .map(Path::to_path_buf)
            .unwrap_or_else(|| companion_stats_path(report_path));
        let stats_bytes = std::fs::read(&stats_path).map_err(|error| {
            SilynError::Schema(format!(
                "cannot read statistics companion {}: {error}; pass --stats <file>",
                stats_path.display()
            ))
        })?;
        let stats: StatsDocument = serde_json::from_slice(&stats_bytes).map_err(|error| {
            SilynError::Schema(format!(
                "invalid statistics companion {}: {error}",
                stats_path.display()
            ))
        })?;
        if stats.schema_version != 1 {
            return Err(SilynError::Schema(format!(
                "statistics schema_version must be 1, got {}",
                stats.schema_version
            )));
        }
        if stats.suite_version != report.suite_version {
            return Err(SilynError::Schema(format!(
                "statistics suite_version `{}` does not match the report `{}`",
                stats.suite_version, report.suite_version
            )));
        }
        let expected_verdict = stats_verdict(&stats.verdict)?;
        if expected_verdict != report.verdict {
            return Err(SilynError::Schema(format!(
                "statistics verdict `{}` does not match the report verdict `{}`",
                stats.verdict, report.verdict
            )));
        }

        let gain = stats.comparisons.c_minus_a.point_pp;
        let ci_low = stats.comparisons.c_minus_a.ci_low_pp;
        if !gain.is_finite() || !ci_low.is_finite() {
            return Err(SilynError::Schema(
                "statistics comparison carries a non-finite gain".to_owned(),
            ));
        }
        if candidate.patch.evidence_run_ids.is_empty() {
            return Err(SilynError::Permanent(format!(
                "candidate {candidate_id} declares no evidence run"
            )));
        }

        let projection = PromotionReport {
            candidate_id,
            verdict: report.verdict.clone(),
            gain_pp: gain,
            ci_low_pp: ci_low,
            unauthorized_actions: stats.unauthorized_actions,
            evidence_run_ids: candidate.patch.evidence_run_ids.clone(),
            suite_version: report.suite_version.clone(),
            report_checksum: String::new(),
        }
        .seal()?;
        let evaluation_id = projection.report_checksum.clone();
        let report_sha256 = sha256_hex(&report_bytes);
        let stats_sha256 = sha256_hex(&stats_bytes);
        let stored = StoredEvaluation {
            promotion_report: projection.clone(),
            report_sha256: report_sha256.clone(),
            stats_sha256: stats_sha256.clone(),
            report_path: report_path.display().to_string(),
            stats_path: stats_path.display().to_string(),
        };

        if candidate.state == CandidateState::Evaluated {
            match self.latest_evaluation_id(candidate_id).await? {
                Some(existing) if existing == evaluation_id => {
                    return Ok(attachment(
                        candidate_id,
                        projection,
                        report_sha256,
                        stats_sha256,
                        report_path,
                        &stats_path,
                        CandidateState::Evaluated,
                    ));
                }
                Some(existing) => {
                    return Err(SilynError::Permanent(format!(
                        "candidate {candidate_id} already has evaluation {existing} attached"
                    )));
                }
                None => {}
            }
        }

        let inserted = sqlx::query(
            "INSERT INTO evaluation_runs (
                id, candidate_id, suite_version, baseline_id, metrics_json, report_path, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(&evaluation_id)
        .bind(candidate_id.to_string())
        .bind(&projection.suite_version)
        .bind(candidate.base_policy_version.to_string())
        .bind(serde_json::to_string(&stored)?)
        .bind(report_path.display().to_string())
        .bind(Utc::now())
        .execute(self.storage().pool())
        .await
        .map_err(db_error)?
        .rows_affected()
            > 0;

        let candidates = CandidateStore::new(self.storage().pool());
        let advanced = match candidate.state {
            CandidateState::StaticChecked => {
                advance_to_evaluated(&candidates, candidate_id, true).await
            }
            CandidateState::Experimenting => {
                advance_to_evaluated(&candidates, candidate_id, false).await
            }
            CandidateState::Evaluated => Ok(candidate.clone()),
            other => Err(SilynError::Permanent(format!(
                "candidate {candidate_id} is in state {other:?} and cannot accept an evaluation"
            ))),
        };

        let record = match advanced {
            Ok(record) => record,
            Err(error) => {
                if inserted {
                    let _ = sqlx::query("DELETE FROM evaluation_runs WHERE id = ?1")
                        .bind(&evaluation_id)
                        .execute(self.storage().pool())
                        .await;
                }
                return Err(error);
            }
        };

        Ok(attachment(
            candidate_id,
            projection,
            report_sha256,
            stats_sha256,
            report_path,
            &stats_path,
            record.state,
        ))
    }

    /// Local operator approval: `AwaitingApproval` -> `Approved`.
    ///
    /// Records the operator, the attached evaluation checksum and the base
    /// policy version in an `evolution.approve` audit row. The runtime has no
    /// path to this method.
    pub async fn approve_candidate(
        &self,
        agent_id: AgentId,
        candidate_id: CandidateId,
        operator: &str,
    ) -> Result<CandidateRecord, SilynError> {
        let operator = operator.trim();
        if operator.is_empty() {
            return Err(SilynError::Schema(
                "an operator identity is required".to_owned(),
            ));
        }
        let candidate = self.get_candidate(agent_id, candidate_id).await?;
        if candidate.state != CandidateState::AwaitingApproval {
            return Err(SilynError::InvalidTransition {
                from: format!("{:?}", candidate.state),
                to: "approved".to_owned(),
            });
        }
        let evaluation_id = self
            .latest_evaluation_id(candidate_id)
            .await?
            .ok_or_else(|| {
                SilynError::Permanent(format!(
                    "candidate {candidate_id} has no evaluation to approve"
                ))
            })?;

        let approved = CandidateStore::new(self.storage().pool())
            .transition(candidate_id, CandidateState::Approved)
            .await?;

        let details = serde_json::json!({
            "approved_by": operator,
            "report_checksum": evaluation_id,
            "base_policy_version": candidate.base_policy_version.to_string(),
        });
        sqlx::query(
            "INSERT INTO audit_events (occurred_at, actor, action, subject, decision, details_ref)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(Utc::now())
        .bind(operator)
        .bind(AUDIT_APPROVE)
        .bind(candidate_id.to_string())
        .bind("allow")
        .bind(serde_json::to_string(&details)?)
        .execute(self.storage().pool())
        .await
        .map_err(db_error)?;

        Ok(approved)
    }

    /// Runs the promotion gate and, for a low-risk pass, promotes the candidate.
    ///
    /// An `Evaluated` candidate goes through [`PromotionStore::decide`]. A
    /// low-risk candidate first performs a real rollback drill on the patched
    /// version. An `Approved` candidate (the operator path) is promoted
    /// directly with the approving operator recorded. Anything else is a
    /// permanent error.
    pub async fn promote_candidate(
        &self,
        agent_id: AgentId,
        candidate_id: CandidateId,
    ) -> Result<PromotionResult, SilynError> {
        let candidate = self.get_candidate(agent_id, candidate_id).await?;
        let promotions = PromotionStore::new(self.storage().pool());

        match candidate.state {
            CandidateState::Approved => {
                let actor = self.approval_actor(candidate_id).await?;
                let promotion = promotions.promote(agent_id, candidate_id, &actor).await?;
                Ok(promoted_result(promotion))
            }
            CandidateState::Evaluated => {
                let report = self.stored_report(candidate_id).await?;
                if candidate.risk_class == RiskClass::Low {
                    self.run_rollback_drill(agent_id, &candidate).await?;
                }
                let decision = promotions.decide(agent_id, &report).await?;
                if matches!(decision, PromotionDecision::Promote) {
                    let promotion = promotions
                        .promote(agent_id, candidate_id, GATE_ACTOR)
                        .await?;
                    return Ok(promoted_result(promotion));
                }

                let state = self.get_candidate(agent_id, candidate_id).await?.state;
                Ok(PromotionResult {
                    decision,
                    promotion: None,
                    from_version: Some(candidate.base_policy_version),
                    to_version: None,
                    state,
                })
            }
            other => Err(SilynError::Permanent(format!(
                "candidate {candidate_id} is in state {other:?}; attach an evaluation or approve it first"
            ))),
        }
    }

    /// Restores the version a promotion replaced.
    ///
    /// The growth store owns the transaction (pointer, `rolled_back_at`,
    /// candidate state and audit row). A second rollback is permanent.
    pub async fn rollback_promotion(
        &self,
        agent_id: AgentId,
        promotion_id: PromotionId,
    ) -> Result<RollbackResult, SilynError> {
        let promotions = PromotionStore::new(self.storage().pool());
        let promotion = promotions
            .get(promotion_id)
            .await?
            .filter(|promotion| promotion.agent_id == agent_id)
            .ok_or_else(|| SilynError::Permanent(format!("promotion {promotion_id} not found")))?;
        let restored = promotions.rollback(promotion_id, CLI_ACTOR).await?;
        Ok(RollbackResult {
            promotion_id,
            from_version: promotion.to_version,
            to_version: restored.id,
        })
    }

    /// Loads the sealed projection stored when the evaluation was attached.
    async fn stored_report(
        &self,
        candidate_id: CandidateId,
    ) -> Result<PromotionReport, SilynError> {
        let metrics: Option<String> = sqlx::query_scalar(
            "SELECT metrics_json FROM evaluation_runs
             WHERE candidate_id = ?1 ORDER BY created_at DESC, id DESC LIMIT 1",
        )
        .bind(candidate_id.to_string())
        .fetch_optional(self.storage().pool())
        .await
        .map_err(db_error)?;
        let metrics = metrics.ok_or_else(|| {
            SilynError::Permanent(format!(
                "candidate {candidate_id} has no attached evaluation; run `evolution evaluate` first"
            ))
        })?;
        let stored: StoredEvaluation = serde_json::from_str(&metrics).map_err(|error| {
            SilynError::Permanent(format!(
                "attached evaluation for candidate {candidate_id} is unreadable: {error}"
            ))
        })?;
        Ok(stored.promotion_report)
    }

    /// Performs a real low-risk rollback drill.
    ///
    /// Creates the patched immutable version, activates it, confirms the pointer,
    /// restores the base version and only then records the drill pass. No drill
    /// evidence is assumed.
    async fn run_rollback_drill(
        &self,
        agent_id: AgentId,
        candidate: &CandidateRecord,
    ) -> Result<(), SilynError> {
        let policies = PolicyStore::new(self.storage().pool());
        let base = policies
            .get(agent_id, candidate.base_policy_version)
            .await?
            .ok_or_else(|| {
                SilynError::Permanent(format!(
                    "base policy version {} not found",
                    candidate.base_policy_version
                ))
            })?;
        let patched = apply_patch(&base.content, &candidate.patch.operations)?;
        if patched.checksum()? == base.checksum {
            return Err(SilynError::Permanent(format!(
                "candidate {} does not change the policy",
                candidate.id
            )));
        }

        let trial = policies.create(agent_id, Some(base.id), &patched).await?;
        policies.activate(agent_id, trial.id).await?;
        let observed = policies.active(agent_id).await?;
        let restore = policies.activate(agent_id, base.id).await;
        restore?;
        if observed.map(|record| record.id) != Some(trial.id) {
            return Err(SilynError::Permanent(format!(
                "rollback drill for candidate {} could not confirm the trial policy",
                candidate.id
            )));
        }

        PromotionStore::new(self.storage().pool())
            .record_rollback_drill(agent_id, candidate.id, DRILL_ACTOR)
            .await
    }

    async fn approval_actor(&self, candidate_id: CandidateId) -> Result<String, SilynError> {
        let actor: Option<String> = sqlx::query_scalar(
            "SELECT actor FROM audit_events
             WHERE action = ?1 AND subject = ?2 ORDER BY id DESC LIMIT 1",
        )
        .bind(AUDIT_APPROVE)
        .bind(candidate_id.to_string())
        .fetch_optional(self.storage().pool())
        .await
        .map_err(db_error)?;
        Ok(actor.unwrap_or_else(|| GATE_ACTOR.to_owned()))
    }

    async fn latest_evaluation_id(
        &self,
        candidate_id: CandidateId,
    ) -> Result<Option<String>, SilynError> {
        sqlx::query_scalar(
            "SELECT id FROM evaluation_runs
             WHERE candidate_id = ?1 ORDER BY created_at DESC, id DESC LIMIT 1",
        )
        .bind(candidate_id.to_string())
        .fetch_optional(self.storage().pool())
        .await
        .map_err(db_error)
    }

    async fn evaluations_for(
        &self,
        candidate_id: CandidateId,
    ) -> Result<Vec<EvaluationSummary>, SilynError> {
        let rows = sqlx::query(
            "SELECT id, suite_version, baseline_id, report_path, created_at
             FROM evaluation_runs WHERE candidate_id = ?1
             ORDER BY created_at ASC, id ASC",
        )
        .bind(candidate_id.to_string())
        .fetch_all(self.storage().pool())
        .await
        .map_err(db_error)?;

        rows.iter()
            .map(|row| {
                Ok(EvaluationSummary {
                    id: row.try_get("id").map_err(db_error)?,
                    suite_version: row.try_get("suite_version").map_err(db_error)?,
                    baseline_id: row.try_get("baseline_id").map_err(db_error)?,
                    report_path: row.try_get("report_path").map_err(db_error)?,
                    created_at: row.try_get("created_at").map_err(db_error)?,
                })
            })
            .collect()
    }

    async fn audit_for(
        &self,
        candidate_id: CandidateId,
    ) -> Result<Vec<AuditEventRecord>, SilynError> {
        let rows = sqlx::query(
            "SELECT occurred_at, actor, action, decision, details_ref
             FROM audit_events WHERE subject = ?1 ORDER BY id ASC",
        )
        .bind(candidate_id.to_string())
        .fetch_all(self.storage().pool())
        .await
        .map_err(db_error)?;

        rows.iter()
            .map(|row| {
                let details: Option<String> = row.try_get("details_ref").map_err(db_error)?;
                Ok(AuditEventRecord {
                    occurred_at: row.try_get("occurred_at").map_err(db_error)?,
                    actor: row.try_get("actor").map_err(db_error)?,
                    action: row.try_get("action").map_err(db_error)?,
                    decision: row.try_get("decision").map_err(db_error)?,
                    details: details
                        .map(|details| serde_json::from_str(&details))
                        .transpose()?,
                })
            })
            .collect()
    }

    async fn ensure_agent(&self, agent_id: AgentId) -> Result<(), SilynError> {
        if AgentRepository::new(self.storage().pool())
            .get(agent_id)
            .await?
            .is_none()
        {
            return Err(SilynError::Permanent(format!("agent {agent_id} not found")));
        }
        Ok(())
    }
}

/// Moves a candidate to `Evaluated`, optionally through `Experimenting`.
async fn advance_to_evaluated(
    candidates: &CandidateStore<'_>,
    candidate_id: CandidateId,
    from_static_checked: bool,
) -> Result<CandidateRecord, SilynError> {
    if from_static_checked {
        candidates
            .transition(candidate_id, CandidateState::Experimenting)
            .await?;
    }
    candidates
        .transition(candidate_id, CandidateState::Evaluated)
        .await
}

fn attachment(
    candidate_id: CandidateId,
    projection: PromotionReport,
    report_sha256: String,
    stats_sha256: String,
    report_path: &Path,
    stats_path: &Path,
    state: CandidateState,
) -> EvaluationAttachment {
    EvaluationAttachment {
        candidate_id,
        evaluation_id: projection.report_checksum.clone(),
        report_sha256,
        stats_sha256,
        suite_version: projection.suite_version,
        verdict: projection.verdict,
        gain_pp: projection.gain_pp,
        ci_low_pp: projection.ci_low_pp,
        unauthorized_actions: projection.unauthorized_actions,
        evidence_run_ids: projection.evidence_run_ids,
        report_checksum: projection.report_checksum,
        state,
        report_path: report_path.to_path_buf(),
        stats_path: stats_path.to_path_buf(),
    }
}

/// Serializes the gate decision as its wire name, not as a nested
/// `{"decision": ...}` object.
fn serialize_decision<S: serde::Serializer>(
    decision: &PromotionDecision,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(match decision {
        PromotionDecision::Promote => "promote",
        PromotionDecision::AwaitApproval => "await_approval",
        PromotionDecision::Reject { .. } => "reject",
    })
}

fn promoted_result(promotion: PromotionRecord) -> PromotionResult {
    PromotionResult {
        decision: PromotionDecision::Promote,
        from_version: Some(promotion.from_version),
        to_version: Some(promotion.to_version),
        promotion: Some(promotion),
        state: CandidateState::Promoted,
    }
}

/// Default companion of `<report>.json` is `<report>.stats.json`, matching
/// `silyn-eval --stats-out`.
fn companion_stats_path(report_path: &Path) -> PathBuf {
    report_path.with_extension("stats.json")
}

/// Maps the detailed statistics verdict onto the frozen report enum.
fn stats_verdict(verdict: &str) -> Result<String, SilynError> {
    Ok(match verdict {
        "promote" => "promote",
        "reject" | "inconclusive" => "reject",
        "invalid" => "invalid",
        other => {
            return Err(SilynError::Schema(format!(
                "unknown statistics verdict `{other}`"
            )));
        }
    }
    .to_owned())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_is_valid_and_immutable_shaped() {
        let policy = default_policy();
        policy.validate().expect("default policy validates");
        assert_eq!(policy.schema_version, 1);
        assert_eq!(policy.context.memory_budget_ratio, 0.35);
        assert_eq!(policy.context.retrieval.bm25_k, 40);
        assert_eq!(policy.context.retrieval.max_evidence, 8);
        assert!(policy.planner.prompt_fragments.is_empty());
        assert!(policy.planner.rules.is_empty());
        assert_eq!(
            policy.tools.allowlist,
            vec![
                "fs.read".to_owned(),
                "data.profile".to_owned(),
                "report.write".to_owned()
            ]
        );
        assert_eq!(
            policy.verification.checkers,
            vec!["csv-stats-oracle".to_owned()]
        );
        assert_eq!(policy.checksum().expect("checksum").len(), 64);
    }

    #[test]
    fn stats_verdict_maps_inconclusive_to_reject() {
        assert_eq!(stats_verdict("promote").expect("promote"), "promote");
        assert_eq!(stats_verdict("reject").expect("reject"), "reject");
        assert_eq!(
            stats_verdict("inconclusive").expect("inconclusive"),
            "reject"
        );
        assert_eq!(stats_verdict("invalid").expect("invalid"), "invalid");
        assert!(stats_verdict("maybe").is_err());
    }

    #[test]
    fn companion_path_replaces_the_extension() {
        assert_eq!(
            companion_stats_path(Path::new("reports/eval.json")),
            Path::new("reports/eval.stats.json")
        );
        assert_eq!(
            companion_stats_path(Path::new("report")),
            Path::new("report.stats.json")
        );
    }
}
