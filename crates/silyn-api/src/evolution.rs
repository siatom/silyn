//! Evolution routes: candidate submission, evaluation attachment, promotion and
//! rollback.
//!
//! The growth crate owns every state machine and every gate. The only thing
//! this module adds is a transport concern: `attach_evaluation` reads a report
//! and its statistics companion from disk, so JSON bodies are persisted to a
//! private file under the app home and then handed to the service, which still
//! performs the full cross-check. Nothing is written to the database before
//! that validation passes.

use std::path::{Path, PathBuf};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::Value;
use silyn_app::{
    CandidateDetail, CandidateRecord, EvaluationAttachment, PolicyPatch, PromotionResult,
    RollbackResult,
};
use silyn_core::SilynError;
use silyn_core::ids::{AgentId, CandidateId, PromotionId};
use uuid::Uuid;

use crate::ApiState;
use crate::body::{parse_body, parse_id};
use crate::error::ApiError;

/// Directory under the app home holding API-submitted evaluation documents.
///
/// Files are private (mode `0600` on Unix), never overwritten (UUID names) and
/// kept after a successful attachment so `evaluation_runs.report_path` stays
/// resolvable for audit.
const EVALUATION_TEMP_DIR: &str = "tmp/evaluation";

/// `GET /v1/evolution/candidates?agent_id=`
pub(crate) async fn list_candidates(
    State(state): State<ApiState>,
    Query(query): Query<AgentQuery>,
) -> Result<Json<Vec<CandidateRecord>>, ApiError> {
    let agent_id = query.agent_id()?;
    Ok(Json(state.app().list_candidates(agent_id).await?))
}

/// `GET /v1/evolution/candidates/{id}?agent_id=`
pub(crate) async fn show_candidate(
    State(state): State<ApiState>,
    UrlPath(raw): UrlPath<String>,
    Query(query): Query<AgentQuery>,
) -> Result<Json<CandidateDetail>, ApiError> {
    let agent_id = query.agent_id()?;
    let candidate_id: CandidateId = parse_id(&raw, "candidate")?;
    Ok(Json(
        state.app().show_candidate(agent_id, candidate_id).await?,
    ))
}

/// `POST /v1/evolution/candidates`
pub(crate) async fn submit_candidate(
    State(state): State<ApiState>,
    body: Bytes,
) -> Result<(StatusCode, Json<CandidateRecord>), ApiError> {
    let body: SubmitBody = parse_body(&body)?;
    let candidate = state
        .app()
        .submit_candidate(body.agent_id, body.patch)
        .await?;
    Ok((StatusCode::CREATED, Json(candidate)))
}

/// `POST /v1/evolution/candidates/{id}/evaluate`
///
/// Body: `{agent_id, report, stats}`. Both documents are written under
/// `<home>/tmp/evaluation/` before the service reads them, so the same
/// validation path as `silyn evolution evaluate` runs unchanged.
pub(crate) async fn evaluate_candidate(
    State(state): State<ApiState>,
    UrlPath(raw): UrlPath<String>,
    body: Bytes,
) -> Result<Json<EvaluationAttachment>, ApiError> {
    let candidate_id: CandidateId = parse_id(&raw, "candidate")?;
    let body: EvaluateBody = parse_body(&body)?;

    let files =
        EvaluationFiles::create(state.app().home(), candidate_id, &body.report, &body.stats)?;
    let attachment = state
        .app()
        .attach_evaluation(
            body.agent_id,
            candidate_id,
            files.report_path(),
            Some(files.stats_path()),
        )
        .await;
    match attachment {
        Ok(attachment) => Ok(Json(attachment)),
        Err(error) => {
            // A rejected report is not evidence: remove the files so a failed
            // call leaves no stale documents behind.
            files.remove();
            Err(error.into())
        }
    }
}

/// `POST /v1/evolution/candidates/{id}/promote`
///
/// A gate rejection is a `200` with `decision: "reject"`, not an HTTP error: it
/// is a legitimate result of the promotion gate (the CLI maps it to exit 5).
pub(crate) async fn promote_candidate(
    State(state): State<ApiState>,
    UrlPath(raw): UrlPath<String>,
    body: Bytes,
) -> Result<Json<PromotionResult>, ApiError> {
    let candidate_id: CandidateId = parse_id(&raw, "candidate")?;
    let body: AgentBody = parse_body(&body)?;
    Ok(Json(
        state
            .app()
            .promote_candidate(body.agent_id, candidate_id)
            .await?,
    ))
}

/// `POST /v1/evolution/promotions/{id}/rollback`
pub(crate) async fn rollback_promotion(
    State(state): State<ApiState>,
    UrlPath(raw): UrlPath<String>,
    body: Bytes,
) -> Result<Json<RollbackResult>, ApiError> {
    let promotion_id: PromotionId = parse_id(&raw, "promotion")?;
    let body: AgentBody = parse_body(&body)?;
    Ok(Json(
        state
            .app()
            .rollback_promotion(body.agent_id, promotion_id)
            .await?,
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentQuery {
    #[serde(default)]
    agent_id: Option<String>,
}

impl AgentQuery {
    fn agent_id(&self) -> Result<AgentId, ApiError> {
        let raw = self
            .agent_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ApiError::schema("agent_id query parameter is required"))?;
        parse_id(raw, "agent")
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SubmitBody {
    agent_id: AgentId,
    patch: PolicyPatch,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvaluateBody {
    agent_id: AgentId,
    report: Value,
    stats: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentBody {
    agent_id: AgentId,
}

/// Report and statistics files written for one evaluation request.
struct EvaluationFiles {
    report: PathBuf,
    stats: PathBuf,
}

impl EvaluationFiles {
    fn create(
        home: &Path,
        candidate_id: CandidateId,
        report: &Value,
        stats: &Value,
    ) -> Result<Self, SilynError> {
        let directory = home.join(EVALUATION_TEMP_DIR);
        std::fs::create_dir_all(&directory).map_err(|error| {
            SilynError::Storage(format!(
                "cannot create evaluation directory {}: {error}",
                directory.display()
            ))
        })?;

        let stem = format!("{candidate_id}-{}", Uuid::now_v7());
        let files = Self {
            report: directory.join(format!("{stem}.report.json")),
            stats: directory.join(format!("{stem}.stats.json")),
        };
        write_private(&files.report, &serde_json::to_vec_pretty(report)?)?;
        write_private(&files.stats, &serde_json::to_vec_pretty(stats)?)?;
        Ok(files)
    }

    fn report_path(&self) -> &Path {
        &self.report
    }

    fn stats_path(&self) -> &Path {
        &self.stats
    }

    fn remove(self) {
        let _ = std::fs::remove_file(&self.report);
        let _ = std::fs::remove_file(&self.stats);
    }
}

/// Creates the file exclusively and privately, so a concurrent request can never
/// read or clobber another candidate's report.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), SilynError> {
    use std::io::Write;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let display = path.display().to_string();
    let mut file = options
        .open(path)
        .map_err(|error| SilynError::Storage(format!("cannot create {display}: {error}")))?;
    file.write_all(bytes)
        .and_then(|()| file.flush())
        .map_err(|error| SilynError::Storage(format!("cannot write {display}: {error}")))
}
