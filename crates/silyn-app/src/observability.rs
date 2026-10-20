//! Run observability and cancellation.
//!
//! These are the read and control operations an operator needs while a run is
//! pending or in flight: the journal events of a run, its tool-call ledger and a
//! cross-process cancellation request.
//!
//! Cancellation is a file signal, not a state transition. The CLI writes
//! `<home>/run/<run_id>.cancel` and the runtime loop (W3-01) owns polling that
//! file and moving the run to `Cancelled`. This crate must never update run
//! status itself, and it must not depend on `silyn-runtime`: the path shape is a
//! string contract verified by tests.

use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::path::PathBuf;

use silyn_core::SilynError;
use silyn_core::event::EventEnvelope;
use silyn_core::ids::RunId;
use silyn_core::state::{LifecycleState, RunStatus};
use silyn_storage::{Journal, ToolCallRecord, ToolLedger};

use crate::app::App;

/// Directory under the app home that carries cross-process run signals.
pub const RUN_SIGNAL_DIR: &str = "run";

/// Result of requesting cancellation of a run.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CancelRequest {
    /// Run the request refers to.
    pub run_id: RunId,
    /// Cancel file the runtime watches.
    pub path: PathBuf,
    /// True when a request already existed; the call is idempotent.
    pub already_requested: bool,
}

impl App {
    /// Lists the journal events of a run in sequence order.
    ///
    /// The run must exist; an existing run with no events yields an empty
    /// vector instead of an error.
    pub async fn list_run_events(&self, run_id: RunId) -> Result<Vec<EventEnvelope>, SilynError> {
        self.get_run(run_id).await?;
        Journal::new(self.storage().pool())
            .list_for_aggregate(run_id)
            .await
    }

    /// Lists the tool-call ledger of a run, oldest first.
    ///
    /// `Indeterminate` rows are reported as such: a caller must never treat them
    /// as success or as a safe replay.
    pub async fn list_tool_calls(&self, run_id: RunId) -> Result<Vec<ToolCallRecord>, SilynError> {
        self.get_run(run_id).await?;
        ToolLedger::new(self.storage().pool())
            .list_by_run(run_id)
            .await
    }

    /// Requests cancellation of a non-terminal run by writing the cancel file.
    ///
    /// Unknown and terminal runs are permanent errors. Writing twice is
    /// idempotent and reports `already_requested: true`, so retries and repeated
    /// operator commands cannot create duplicate signals or fail spuriously.
    pub async fn request_cancel(&self, run_id: RunId) -> Result<CancelRequest, SilynError> {
        let run = self.get_run(run_id).await?;
        if run.status.is_terminal() {
            return Err(SilynError::Permanent(format!(
                "run {run_id} is already {} and cannot be cancelled",
                status_label(run.status)
            )));
        }

        let path = self.cancel_request_path(run_id);
        let directory = self.home().join(RUN_SIGNAL_DIR);
        std::fs::create_dir_all(&directory).map_err(|error| {
            SilynError::Storage(format!(
                "cannot create signal directory {}: {error}",
                directory.display()
            ))
        })?;

        // `create_new` makes the check and the write one atomic step, so two
        // concurrent cancel requests still leave exactly one file.
        let already_requested = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => false,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => true,
            Err(error) => {
                return Err(SilynError::Storage(format!(
                    "cannot create cancel file {}: {error}",
                    path.display()
                )));
            }
        };

        Ok(CancelRequest {
            run_id,
            path,
            already_requested,
        })
    }

    /// Cancel file for a run: `<home>/run/<run_id>.cancel`.
    ///
    /// This is the frozen path contract shared with the runtime. It performs no
    /// filesystem access.
    #[must_use]
    pub fn cancel_request_path(&self, run_id: RunId) -> PathBuf {
        self.home()
            .join(RUN_SIGNAL_DIR)
            .join(format!("{run_id}.cancel"))
    }
}

/// Names a run status the way the JSON wire format does.
fn status_label(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Created => "created",
        RunStatus::Running => "running",
        RunStatus::WaitingApproval => "waiting_approval",
        RunStatus::WaitingTool => "waiting_tool",
        RunStatus::Verifying => "verifying",
        RunStatus::Completed => "completed",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
    }
}

#[cfg(test)]
mod tests {
    use silyn_core::state::RunStatus;

    use super::*;

    async fn app_in_tempdir() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().expect("tempdir");
        let app = App::open(dir.path().join("home")).await.expect("open app");
        (dir, app)
    }

    #[tokio::test]
    async fn cancel_path_is_home_run_run_id_cancel() {
        let (dir, app) = app_in_tempdir().await;
        let run_id = RunId::new();

        let path = app.cancel_request_path(run_id);

        assert_eq!(
            path,
            dir.path()
                .join("home")
                .join("run")
                .join(format!("{run_id}.cancel"))
        );
        assert!(
            !path.parent().expect("parent").exists(),
            "building the path must not touch the filesystem"
        );
    }

    #[tokio::test]
    async fn terminal_statuses_are_reported_by_name() {
        assert_eq!(status_label(RunStatus::Completed), "completed");
        assert_eq!(status_label(RunStatus::Failed), "failed");
        assert_eq!(status_label(RunStatus::Cancelled), "cancelled");
        assert!(RunStatus::Cancelled.is_terminal());
        assert!(!RunStatus::Running.is_terminal());
    }

    #[tokio::test]
    async fn observations_of_an_unknown_run_are_permanent() {
        let (_dir, app) = app_in_tempdir().await;
        let run_id = RunId::new();

        for error in [
            app.list_run_events(run_id).await.expect_err("unknown run"),
            app.list_tool_calls(run_id).await.expect_err("unknown run"),
            app.request_cancel(run_id).await.expect_err("unknown run"),
        ] {
            assert!(matches!(error, SilynError::Permanent(_)));
            assert!(error.to_string().contains("not found"), "{error}");
        }
    }
}
