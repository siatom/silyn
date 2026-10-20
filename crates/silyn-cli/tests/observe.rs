//! End-to-end tests for run observability and cancellation.
//!
//! Each test uses its own temporary `SILYN_HOME` and that directory as the
//! working directory, so `dotenvy` cannot pick up a developer `.env` and the
//! tests never touch the real `.silyn` directory or the network.
//!
//! The read/control tests seed runs, journal events and ledger rows through
//! `silyn-storage` before the binary is invoked. `silyn run` itself is covered
//! by `tests/run.rs` (W3-03).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use chrono::Utc;
use serde_json::Value;
use silyn_core::event::{EventEnvelope, RunEvent};
use silyn_core::ids::{AgentId, PolicyVersionId, RunId, SessionId, StepId, TaskId};
use silyn_core::state::{RunStatus, TaskStatus};
use silyn_storage::{
    AgentRecord, AgentRepository, Journal, RunRecord, RunRepository, SessionRecord,
    SessionRepository, Storage, TaskRecord, TaskRepository, ToolCallRecord, ToolCallState,
    ToolLedger,
};
use tempfile::TempDir;

/// Run id that never exists in a sandbox.
const UNKNOWN_RUN: &str = "0192f0a0-0000-7000-8000-0000000000ff";

struct Sandbox {
    dir: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("tempdir"),
        }
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn signal_dir(&self) -> PathBuf {
        self.home().join("run")
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_silyn"));
        command
            .args(args)
            .current_dir(self.dir.path())
            .env("SILYN_HOME", self.home())
            .env_remove("SILYN_LLM_API_KEY_ENV")
            .env_remove("DEEPSEEK_API_KEY");
        command.output().expect("run silyn")
    }

    /// Opens the sandbox database, creating the home directory when missing.
    async fn storage(&self) -> Storage {
        fs::create_dir_all(self.home()).expect("create home");
        Storage::connect(self.home().join("silyn.db"))
            .await
            .expect("open storage")
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed with {:?}\nstderr: {}",
        output.status.code(),
        stderr(output)
    );
}

fn json_stdout(output: &Output) -> Value {
    assert_success(output);
    serde_json::from_str(&stdout(output))
        .unwrap_or_else(|error| panic!("stdout is not JSON: {error}\n{}", stdout(output)))
}

fn assert_exit_five(output: &Output, needle: &str) {
    assert_eq!(
        output.status.code(),
        Some(5),
        "expected exit 5\nstdout: {}\nstderr: {}",
        stdout(output),
        stderr(output)
    );
    assert!(
        stderr(output).contains(needle),
        "stderr must contain `{needle}`: {}",
        stderr(output)
    );
}

/// Inserts the agent/session/task/run chain so foreign keys hold, and returns
/// the run id.
async fn seed_run(storage: &Storage, status: RunStatus) -> RunId {
    let now = Utc::now();
    let agent_id = AgentId::new();
    AgentRepository::new(storage.pool())
        .insert(&AgentRecord {
            id: agent_id,
            owner_id: "local-user".to_owned(),
            profile_version: "profile-v1".to_owned(),
            active_policy_version: None,
            created_at: now,
        })
        .await
        .expect("insert agent");

    let session_id = SessionId::new();
    SessionRepository::new(storage.pool())
        .insert(&SessionRecord {
            id: session_id,
            agent_id,
            context_version: "ctx-v1".to_owned(),
            created_at: now,
        })
        .await
        .expect("insert session");

    let task_id = TaskId::new();
    TaskRepository::new(storage.pool())
        .insert(&TaskRecord {
            id: task_id,
            session_id,
            objective: "observe the run".to_owned(),
            status: TaskStatus::Created,
            created_at: now,
            updated_at: now,
        })
        .await
        .expect("insert task");

    let run_id = RunId::new();
    RunRepository::new(storage.pool())
        .insert(&RunRecord {
            id: run_id,
            task_id,
            policy_version: PolicyVersionId::new(),
            memory_snapshot_version: None,
            model_id: "mock".to_owned(),
            status,
            input_tokens: 0,
            output_tokens: 0,
            cost_usd: 0.0,
            started_at: None,
            finished_at: None,
            created_at: now,
        })
        .await
        .expect("insert run");
    run_id
}

async fn append_event(journal: &Journal<'_>, run_id: RunId, seq: u64, event: RunEvent) {
    journal
        .append(&EventEnvelope::new(run_id, seq, None, event))
        .await
        .expect("append event");
}

async fn begin_call(storage: &Storage, run_id: RunId, step_id: StepId, call_id: &str, tool: &str) {
    ToolLedger::new(storage.pool())
        .begin(&ToolCallRecord {
            run_id,
            step_id,
            tool_call_id: call_id.to_owned(),
            tool_name: tool.to_owned(),
            state: ToolCallState::Started,
            result_ref: None,
            created_at: Utc::now(),
        })
        .await
        .expect("begin call");
}

fn cancel_file(home: &Path, run_id: RunId) -> PathBuf {
    home.join("run").join(format!("{run_id}.cancel"))
}

#[tokio::test]
async fn runs_events_lists_events_in_seq_order() {
    let sandbox = Sandbox::new();
    let storage = sandbox.storage().await;
    let run_id = seed_run(&storage, RunStatus::Running).await;

    let journal = Journal::new(storage.pool());
    append_event(&journal, run_id, 1, RunEvent::RunStarted).await;
    append_event(
        &journal,
        run_id,
        2,
        RunEvent::StepCompleted {
            step_id: StepId::new(),
            seq: 1,
        },
    )
    .await;
    storage.close().await;

    let run_id_text = run_id.to_string();
    let value = json_stdout(&sandbox.run(&["runs", "events", &run_id_text, "--json"]));

    let events = value.as_array().expect("events are a JSON array");
    assert_eq!(events.len(), 2, "both events must be listed");
    assert_eq!(events[0]["seq"], 1);
    assert_eq!(events[1]["seq"], 2);
    assert_eq!(events[0]["aggregate_id"], run_id_text);
    assert_eq!(events[0]["event"]["type"], "run_started");
    assert_eq!(events[1]["event"]["type"], "step_completed");
    assert_eq!(events[0]["schema_version"], 1);

    // Human output is unpadded TSV in the documented column order.
    let output = sandbox.run(&["runs", "events", &run_id_text]);
    assert_success(&output);
    let text = stdout(&output);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{}", text);
    let columns: Vec<&str> = lines[0].split('\t').collect();
    assert_eq!(columns.len(), 4, "{}", text);
    assert_eq!(columns[0], "1");
    assert_eq!(columns[1], "run_started");
}

#[tokio::test]
async fn runs_events_unknown_run_exits_five() {
    let sandbox = Sandbox::new();

    let output = sandbox.run(&["runs", "events", UNKNOWN_RUN, "--json"]);

    assert_exit_five(&output, "not found");
    let error: Value = serde_json::from_str(&stderr(&output)).expect("JSON error object");
    assert_eq!(error["error"]["class"], "permanent");
    assert!(
        !sandbox.signal_dir().exists(),
        "listing events must not create the signal directory"
    );
}

#[tokio::test]
async fn runs_tools_exposes_ledger_states() {
    let sandbox = Sandbox::new();
    let storage = sandbox.storage().await;
    let run_id = seed_run(&storage, RunStatus::Running).await;

    let started_step = StepId::new();
    begin_call(&storage, run_id, started_step, "call-1", "fs.read").await;

    let unresolved_step = StepId::new();
    begin_call(&storage, run_id, unresolved_step, "call-2", "report.write").await;
    ToolLedger::new(storage.pool())
        .mark_indeterminate(run_id, unresolved_step, "call-2")
        .await
        .expect("mark indeterminate");
    storage.close().await;

    let run_id_text = run_id.to_string();
    let value = json_stdout(&sandbox.run(&["runs", "tools", &run_id_text, "--json"]));

    let calls = value.as_array().expect("ledger is a JSON array");
    assert_eq!(calls.len(), 2);
    let states: Vec<&str> = calls
        .iter()
        .filter_map(|call| call["state"].as_str())
        .collect();
    assert_eq!(states, vec!["started", "indeterminate"]);
    assert_eq!(calls[0]["tool_name"], "fs.read");
    assert_eq!(calls[1]["tool_name"], "report.write");
    assert_eq!(calls[1]["tool_call_id"], "call-2");
    assert_eq!(calls[0]["run_id"], run_id_text);

    // Human output exposes the same state column.
    let output = sandbox.run(&["runs", "tools", &run_id_text]);
    assert_success(&output);
    let text = stdout(&output);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{}", text);
    let columns: Vec<&str> = lines[1].split('\t').collect();
    assert_eq!(columns.len(), 5, "{}", text);
    assert_eq!(columns[1], "report.write");
    assert_eq!(columns[2], "indeterminate");
    assert_eq!(columns[3], "call-2");
    assert_eq!(columns[4], "-");
}

#[tokio::test]
async fn runs_tools_unknown_run_exits_five() {
    let sandbox = Sandbox::new();

    let output = sandbox.run(&["runs", "tools", UNKNOWN_RUN]);

    assert_exit_five(&output, "not found");
}

#[tokio::test]
async fn runs_cancel_creates_the_documented_file() {
    let sandbox = Sandbox::new();
    let storage = sandbox.storage().await;
    let run_id = seed_run(&storage, RunStatus::Running).await;
    storage.close().await;

    let run_id_text = run_id.to_string();
    let value = json_stdout(&sandbox.run(&["runs", "cancel", &run_id_text, "--json"]));

    let expected = cancel_file(&sandbox.home(), run_id);
    assert_eq!(value["run_id"], run_id_text);
    assert_eq!(
        value["path"],
        Path::new(&expected).to_string_lossy().as_ref()
    );
    assert_eq!(value["already_requested"], false);
    assert!(expected.is_file(), "cancel file must exist");
    assert_eq!(
        fs::metadata(&expected).expect("metadata").len(),
        0,
        "cancel file is the signal, not a payload"
    );
    let entries: Vec<PathBuf> = fs::read_dir(sandbox.signal_dir())
        .expect("signal directory")
        .map(|entry| entry.expect("entry").path())
        .collect();
    assert_eq!(entries, vec![expected]);

    // The CLI must not transition the run itself.
    let run = json_stdout(&sandbox.run(&["runs", "show", &run_id_text, "--json"]));
    assert_eq!(run["status"], "running");
}

#[tokio::test]
async fn runs_cancel_is_idempotent() {
    let sandbox = Sandbox::new();
    let storage = sandbox.storage().await;
    let run_id = seed_run(&storage, RunStatus::Created).await;
    storage.close().await;

    let run_id_text = run_id.to_string();
    let first = json_stdout(&sandbox.run(&["runs", "cancel", &run_id_text, "--json"]));
    let second = json_stdout(&sandbox.run(&["runs", "cancel", &run_id_text, "--json"]));

    assert_eq!(first["already_requested"], false);
    assert_eq!(second["already_requested"], true);
    assert_eq!(first["path"], second["path"]);

    let entries: Vec<PathBuf> = fs::read_dir(sandbox.signal_dir())
        .expect("signal directory")
        .map(|entry| entry.expect("entry").path())
        .collect();
    assert_eq!(entries, vec![cancel_file(&sandbox.home(), run_id)]);

    // Human output reports the request state of the second call.
    let output = sandbox.run(&["runs", "cancel", &run_id_text]);
    assert_success(&output);
    let text = stdout(&output);
    assert!(
        text.contains("already_requested: true"),
        "third call must report the existing request: {text}"
    );
}

#[tokio::test]
async fn runs_cancel_terminal_run_exits_five() {
    let sandbox = Sandbox::new();
    let storage = sandbox.storage().await;
    let run_id = seed_run(&storage, RunStatus::Completed).await;
    storage.close().await;

    let output = sandbox.run(&["runs", "cancel", &run_id.to_string(), "--json"]);

    assert_exit_five(&output, "already completed");
    let error: Value = serde_json::from_str(&stderr(&output)).expect("JSON error object");
    assert_eq!(error["error"]["class"], "permanent");
    assert!(
        !sandbox.signal_dir().exists(),
        "a terminal run must not get a cancel file"
    );
}

#[tokio::test]
async fn runs_cancel_unknown_run_exits_five() {
    let sandbox = Sandbox::new();

    let output = sandbox.run(&["runs", "cancel", UNKNOWN_RUN, "--json"]);

    assert_exit_five(&output, "not found");
    assert!(
        !sandbox.signal_dir().exists(),
        "an unknown run must not get a cancel file"
    );
}

#[tokio::test]
async fn run_still_exits_five_and_creates_no_run() {
    let sandbox = Sandbox::new();
    assert_success(&sandbox.run(&["init"]));

    let output = sandbox.run(&["run"]);

    // W3-03 turned `run` into a real command; the no-source case stays a
    // permanent error (exit 5) and must not create a run.
    assert_exit_five(&output, "requires exactly one of --task, --resume");
    let runs = json_stdout(&sandbox.run(&["runs", "list", "--json"]));
    assert_eq!(runs, Value::Array(Vec::new()));
}
