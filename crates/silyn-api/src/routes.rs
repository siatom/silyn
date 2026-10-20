//! Agent, session, task, run and memory routes.
//!
//! Every handler is a thin adapter: it parses the path/body into `silyn-core`
//! types, calls one [`App`] method and serializes the app record. Validation,
//! memory scoping, the cancel file and the run loop stay in `silyn-app`.

use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use serde::Deserialize;
use serde_json::{Value, json};
use silyn_app::{
    AgentRecord, App, BudgetSpec, CancelRequest, MEMORY_LIMIT, MEMORY_LIST_LIMIT, MemoryHitRecord,
    RunOptions, RunOutcome, RunRecord, RunRequest, SessionRecord, TaskRecord,
};
use silyn_core::event::EventEnvelope;
use silyn_core::ids::{AgentId, RunId, SessionId, TaskId};
use silyn_core::state::LifecycleState;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::body::{parse_body, parse_id, parse_optional_body};
use crate::error::ApiError;
use crate::{ApiState, EVENT_CHANNEL_CAPACITY, EVENT_POLL_INTERVAL};

/// Liveness probe. Public, no database access, no configuration detail.
pub(crate) async fn health() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "version": crate::VERSION,
    }))
}

/// `POST /v1/agents`
pub(crate) async fn create_agent(
    State(state): State<ApiState>,
    body: Bytes,
) -> Result<(StatusCode, Json<AgentRecord>), ApiError> {
    let body: CreateAgentBody = parse_body(&body)?;
    let agent = state.app().create_agent(&body.owner_id).await?;
    Ok((StatusCode::CREATED, Json(agent)))
}

/// `GET /v1/agents`
pub(crate) async fn list_agents(
    State(state): State<ApiState>,
) -> Result<Json<Vec<AgentRecord>>, ApiError> {
    Ok(Json(state.app().list_agents().await?))
}

/// `POST /v1/agents/{id}/sessions`
pub(crate) async fn create_session(
    State(state): State<ApiState>,
    Path(raw): Path<String>,
) -> Result<(StatusCode, Json<SessionRecord>), ApiError> {
    let agent_id: AgentId = parse_id(&raw, "agent")?;
    let session = state.app().create_session(agent_id).await?;
    Ok((StatusCode::CREATED, Json(session)))
}

/// `POST /v1/tasks`
pub(crate) async fn create_task(
    State(state): State<ApiState>,
    body: Bytes,
) -> Result<(StatusCode, Json<TaskRecord>), ApiError> {
    let body: CreateTaskBody = parse_body(&body)?;
    // Budgets apply to one execution, and a task row has no budget column. The
    // optional `budget` is accepted and validated for contract compatibility so
    // a malformed cap is reported at creation time, but the enforced limits are
    // the ones sent to `POST /v1/tasks/{id}/run`.
    if let Some(budget) = body.budget {
        budget.resolve()?;
    }
    let task = state
        .app()
        .create_task(body.session_id, &body.objective)
        .await?;
    Ok((StatusCode::CREATED, Json(task)))
}

/// `POST /v1/tasks/{id}/run`
///
/// Creates the run, executes it to a terminal state and returns the
/// [`RunOutcome`]. A failed or cancelled run is a `200` result, not an HTTP
/// error: the outcome carries the status, exactly like the CLI's exit code.
pub(crate) async fn run_task(
    State(state): State<ApiState>,
    Path(raw): Path<String>,
    body: Bytes,
) -> Result<Json<RunOutcome>, ApiError> {
    let task_id: TaskId = parse_id(&raw, "task")?;
    let request = parse_optional_body::<RunBody>(&body)?.into_request(RunSource::Task(task_id))?;
    Ok(Json(state.app().run(request).await?))
}

/// `POST /v1/runs/{id}/resume`
pub(crate) async fn resume_run(
    State(state): State<ApiState>,
    Path(raw): Path<String>,
    body: Bytes,
) -> Result<Json<RunOutcome>, ApiError> {
    let run_id: RunId = parse_id(&raw, "run")?;
    let request = parse_optional_body::<RunBody>(&body)?.into_request(RunSource::Resume(run_id))?;
    Ok(Json(state.app().run(request).await?))
}

/// `GET /v1/runs/{id}`
pub(crate) async fn get_run(
    State(state): State<ApiState>,
    Path(raw): Path<String>,
) -> Result<Json<RunRecord>, ApiError> {
    let run_id: RunId = parse_id(&raw, "run")?;
    Ok(Json(state.app().get_run(run_id).await?))
}

/// `POST /v1/runs/{id}/cancel`
///
/// Writes the documented cancel file through the app service. The runtime owns
/// the transition; this handler never changes run status.
pub(crate) async fn cancel_run(
    State(state): State<ApiState>,
    Path(raw): Path<String>,
) -> Result<Json<CancelRequest>, ApiError> {
    let run_id: RunId = parse_id(&raw, "run")?;
    Ok(Json(state.app().request_cancel(run_id).await?))
}

/// `GET /v1/runs/{id}/events`
///
/// Server-sent events: replay the journal, then poll every
/// [`EVENT_POLL_INTERVAL`] until the run is terminal or the client disconnects.
/// `id:` is the event sequence, `event:` the `RunEvent` wire type and `data:` the
/// full [`EventEnvelope`] JSON.
pub(crate) async fn run_events(
    State(state): State<ApiState>,
    Path(raw): Path<String>,
) -> Result<Sse<EventStream>, ApiError> {
    let run_id: RunId = parse_id(&raw, "run")?;
    // Resolve the run before the response starts so an unknown id is a 404 and
    // not an empty stream.
    state.app().get_run(run_id).await?;

    let (sender, receiver) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    let app = Arc::clone(state.app());
    tokio::spawn(async move {
        stream_events(app, run_id, sender).await;
    });

    Ok(Sse::new(ReceiverStream::new(receiver)))
}

/// `GET /v1/agents/{id}/memories?q=&limit=`
pub(crate) async fn search_memories(
    State(state): State<ApiState>,
    Path(raw): Path<String>,
    Query(query): Query<MemoryQuery>,
) -> Result<Json<Vec<MemoryHitRecord>>, ApiError> {
    let agent_id: AgentId = parse_id(&raw, "agent")?;
    let limit = query.limit()?;
    let hits = state
        .app()
        .search_memories(agent_id, query.query(), limit)
        .await?;
    Ok(Json(hits))
}

type EventStream = ReceiverStream<Result<Event, Infallible>>;

/// Polls the journal and forwards new events until the run is terminal.
///
/// Status is read *before* the events in each iteration, so a terminal status is
/// only observed after every event committed with it has been sent. The task
/// ends when the receiver is gone (client disconnect), on a read error, or when
/// the run is terminal.
async fn stream_events(
    app: Arc<App>,
    run_id: RunId,
    sender: mpsc::Sender<Result<Event, Infallible>>,
) {
    let mut last_seq = 0_u64;
    loop {
        let run = match app.get_run(run_id).await {
            Ok(run) => run,
            Err(_) => return,
        };
        let events = match app.list_run_events(run_id).await {
            Ok(events) => events,
            Err(_) => return,
        };

        for envelope in events {
            if envelope.seq <= last_seq {
                continue;
            }
            last_seq = envelope.seq;
            if sender.send(Ok(frame(&envelope))).await.is_err() {
                return;
            }
        }

        if run.status.is_terminal() {
            return;
        }
        tokio::time::sleep(EVENT_POLL_INTERVAL).await;
    }
}

/// One SSE frame: sequence as `id`, event type as `event`, envelope as `data`.
fn frame(envelope: &EventEnvelope) -> Event {
    Event::default()
        .id(envelope.seq.to_string())
        .event(event_type(envelope))
        .data(envelope_json(envelope))
}

/// Wire name of the event type, taken from the `RunEvent` serde tag.
fn event_type(envelope: &EventEnvelope) -> String {
    serde_json::to_value(&envelope.event)
        .ok()
        .and_then(|value| value.get("type").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

fn envelope_json(envelope: &EventEnvelope) -> String {
    serde_json::to_string(envelope).unwrap_or_else(|_| "{}".to_owned())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateAgentBody {
    owner_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateTaskBody {
    session_id: SessionId,
    objective: String,
    #[serde(default)]
    budget: Option<BudgetBody>,
}

/// One run source selected by the route.
#[derive(Debug, Clone, Copy)]
enum RunSource {
    Task(TaskId),
    Resume(RunId),
}

/// Options accepted by `POST /v1/tasks/{id}/run` and `POST /v1/runs/{id}/resume`.
///
/// Field names follow the CLI flags. An empty body means "all defaults".
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunBody {
    /// Use the deterministic offline provider (no API key, no network).
    #[serde(default)]
    offline: bool,
    /// Model id recorded on the run. Defaults to `SILYN_LLM_MODEL` or the
    /// provider default.
    #[serde(default)]
    model: Option<String>,
    /// Capabilities granted to this run. Empty means none.
    #[serde(default)]
    capabilities: Vec<String>,
    /// Hard limits for this run.
    #[serde(default)]
    budget: Option<BudgetBody>,
}

impl RunBody {
    fn into_request(self, source: RunSource) -> Result<RunRequest, ApiError> {
        let budget = self.budget.unwrap_or_default().resolve()?;
        let model = self
            .model
            .map(|model| model.trim().to_owned())
            .filter(|model| !model.is_empty())
            .unwrap_or_else(silyn_app::default_model);
        let options = RunOptions {
            model,
            capabilities: dedupe(self.capabilities),
            budget,
            ..RunOptions::default()
        };

        let (task_id, resume) = match source {
            RunSource::Task(task_id) => (Some(task_id), None),
            RunSource::Resume(run_id) => (None, Some(run_id)),
        };
        let request = RunRequest {
            task_id,
            session_id: None,
            objective: None,
            resume,
            options,
            offline: self.offline,
            expected_json: None,
        };
        // Validate before any storage access, so a contradictory request cannot
        // create a home directory, a task or a run.
        request.validate()?;
        Ok(request)
    }
}

/// Budget fields, named like the CLI flags.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct BudgetBody {
    #[serde(default)]
    max_steps: Option<u32>,
    #[serde(default)]
    max_input_tokens: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<u64>,
    #[serde(default)]
    max_cost_usd: Option<f64>,
    #[serde(default)]
    max_wall_secs: Option<u64>,
}

impl BudgetBody {
    /// Fills unset limits from [`BudgetSpec::default`].
    fn resolve(self) -> Result<BudgetSpec, ApiError> {
        let defaults = BudgetSpec::default();
        if let Some(cost) = self.max_cost_usd {
            if !cost.is_finite() || cost < 0.0 {
                return Err(ApiError::schema(
                    "budget.max_cost_usd must be a finite, non-negative number",
                ));
            }
        }
        Ok(BudgetSpec {
            max_steps: self.max_steps.unwrap_or(defaults.max_steps),
            max_input_tokens: self.max_input_tokens.unwrap_or(defaults.max_input_tokens),
            max_output_tokens: self.max_output_tokens.unwrap_or(defaults.max_output_tokens),
            max_cost_usd: self.max_cost_usd,
            max_wall_clock: Duration::from_secs(
                self.max_wall_secs
                    .unwrap_or(defaults.max_wall_clock.as_secs()),
            ),
            step_timeout: defaults.step_timeout,
        })
    }
}

/// Query string of `GET /v1/agents/{id}/memories`.
///
/// `limit` is parsed by hand so a non-numeric value is a JSON `400`, not an
/// extractor rejection with a plain-text body.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct MemoryQuery {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    limit: Option<String>,
}

impl MemoryQuery {
    fn query(&self) -> &str {
        self.q.as_deref().unwrap_or_default().trim()
    }

    fn limit(&self) -> Result<usize, ApiError> {
        let Some(raw) = self
            .limit
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return Ok(MEMORY_LIMIT);
        };
        let limit: usize = raw
            .parse()
            .map_err(|_| ApiError::schema(format!("limit must be an integer, got `{raw}`")))?;
        if limit == 0 {
            return Err(ApiError::schema("limit must be at least 1"));
        }
        Ok(limit.min(MEMORY_LIST_LIMIT))
    }
}

/// Drops repeated capability names while keeping the caller's order.
fn dedupe(names: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    names
        .into_iter()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .filter(|name| seen.insert(name.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_defaults_match_the_runtime() {
        let budget = BudgetBody::default().resolve().expect("default budget");
        assert_eq!(budget, BudgetSpec::default());
    }

    #[test]
    fn budget_rejects_a_non_finite_cost_cap() {
        let error = BudgetBody {
            max_cost_usd: Some(f64::NAN),
            ..BudgetBody::default()
        }
        .resolve()
        .expect_err("NaN cost");
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);

        let error = BudgetBody {
            max_cost_usd: Some(-1.0),
            ..BudgetBody::default()
        }
        .resolve()
        .expect_err("negative cost");
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn budget_overrides_only_the_named_limits() {
        let budget = BudgetBody {
            max_steps: Some(0),
            ..BudgetBody::default()
        }
        .resolve()
        .expect("budget");
        assert_eq!(budget.max_steps, 0);
        assert_eq!(
            budget.max_input_tokens,
            BudgetSpec::default().max_input_tokens
        );
    }

    #[test]
    fn capabilities_are_trimmed_and_deduped_in_order() {
        assert_eq!(
            dedupe(vec![
                " fs.read ".to_owned(),
                "fs.read".to_owned(),
                String::new(),
                "report.write".to_owned()
            ]),
            vec!["fs.read".to_owned(), "report.write".to_owned()]
        );
    }

    #[test]
    fn memory_limit_defaults_and_clamps() {
        assert_eq!(
            MemoryQuery::default().limit().expect("default"),
            MEMORY_LIMIT
        );

        let query = MemoryQuery {
            limit: Some("1".to_owned()),
            ..MemoryQuery::default()
        };
        assert_eq!(query.limit().expect("one"), 1);

        let query = MemoryQuery {
            limit: Some("100000".to_owned()),
            ..MemoryQuery::default()
        };
        assert_eq!(query.limit().expect("clamped"), MEMORY_LIST_LIMIT);

        for raw in ["0", "-1", "many"] {
            let query = MemoryQuery {
                limit: Some(raw.to_owned()),
                ..MemoryQuery::default()
            };
            assert!(query.limit().is_err(), "`{raw}` must be rejected");
        }
    }

    #[test]
    fn event_frames_use_the_serde_tag_and_sequence() {
        use silyn_core::event::{EventEnvelope, RunEvent};

        let envelope = EventEnvelope::new(RunId::new(), 3, None, RunEvent::RunStarted);
        assert_eq!(event_type(&envelope), "run_started");
        assert!(envelope_json(&envelope).contains("\"seq\":3"));
    }
}
