//! End-to-end tests for `silyn run` and `silyn run --resume`.
//!
//! Every test uses its own temporary `SILYN_HOME` and that directory as the
//! working directory, so `dotenvy` cannot pick up a developer `.env`. All runs
//! are `--offline`: the deterministic mock provider never reads an API key and
//! never touches the network.

use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

/// Run id that never exists in a sandbox.
const UNKNOWN_RUN: &str = "0192f0a0-0000-7000-8000-0000000000ff";
/// Task id that never exists in a sandbox.
const UNKNOWN_TASK: &str = "0192f0a0-0000-7000-8000-0000000000fe";
/// Final answer of the offline provider.
const OFFLINE_ANSWER: &str = "offline mock run";

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

    fn database(&self) -> PathBuf {
        self.home().join("silyn.db")
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with_env(args, &[])
    }

    fn run_with_env(&self, args: &[&str], envs: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_silyn"));
        command
            .args(args)
            .current_dir(self.dir.path())
            .env("SILYN_HOME", self.home())
            .env_remove("SILYN_LLM_API_KEY_ENV")
            .env_remove("SILYN_LLM_MODEL")
            .env_remove("DEEPSEEK_API_KEY");
        for (key, value) in envs {
            command.env(key, value);
        }
        command.output().expect("run silyn")
    }

    /// Creates an agent and a session; returns the session id.
    fn create_session(&self) -> String {
        let agent = json_stdout(&self.run(&["agent", "create", "--owner", "local-user", "--json"]));
        let agent_id = agent["id"].as_str().expect("agent id").to_owned();
        let session =
            json_stdout(&self.run(&["session", "create", "--agent", &agent_id, "--json"]));
        session["id"].as_str().expect("session id").to_owned()
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

/// Event type names of one run's journal, oldest first.
fn event_types(sandbox: &Sandbox, run_id: &str) -> Vec<String> {
    let events = json_stdout(&sandbox.run(&["runs", "events", run_id, "--json"]));
    events
        .as_array()
        .expect("events are a JSON array")
        .iter()
        .filter_map(|envelope| envelope["event"]["type"].as_str().map(str::to_owned))
        .collect()
}

#[test]
fn run_offline_completes() {
    let sandbox = Sandbox::new();
    let session_id = sandbox.create_session();

    let value = json_stdout(&sandbox.run(&[
        "run",
        "--session",
        &session_id,
        "--objective",
        "summarize the csv",
        "--offline",
        "--json",
    ]));

    assert_eq!(value["status"], "completed");
    assert_eq!(value["outcome"], "unknown");
    assert_eq!(value["final_answer"], OFFLINE_ANSWER);
    assert_eq!(value["steps"], 1);
    assert_eq!(value["reason"], Value::Null);
    assert_eq!(value["input_tokens"], 0);
    assert_eq!(value["output_tokens"], 0);
    assert_eq!(value["cost_usd"], 0.0);

    let run_id = value["run_id"].as_str().expect("run id").to_owned();
    let run = json_stdout(&sandbox.run(&["runs", "show", &run_id, "--json"]));
    assert_eq!(run["id"], run_id);
    assert_eq!(run["status"], "completed");
    assert_eq!(run["model_id"], "deepseek-chat");
    assert!(run["started_at"].is_string(), "started_at is recorded");
    assert!(run["finished_at"].is_string(), "finished_at is recorded");

    // Human mode reports the same terminal state and final answer.
    let output = sandbox.run(&[
        "run",
        "--session",
        &session_id,
        "--objective",
        "write one sentence",
        "--offline",
    ]);
    assert_success(&output);
    let text = stdout(&output);
    assert!(text.contains("status: completed"), "{text}");
    assert!(text.contains("outcome: unknown"), "{text}");
    assert!(
        text.contains(&format!("final_answer: {OFFLINE_ANSWER}")),
        "{text}"
    );
}

#[test]
fn run_records_lifecycle_events() {
    let sandbox = Sandbox::new();
    let session_id = sandbox.create_session();

    let value = json_stdout(&sandbox.run(&[
        "run",
        "--session",
        &session_id,
        "--objective",
        "summarize the csv",
        "--offline",
        "--json",
    ]));
    let run_id = value["run_id"].as_str().expect("run id").to_owned();

    let events = json_stdout(&sandbox.run(&["runs", "events", &run_id, "--json"]));
    let events = events.as_array().expect("events are a JSON array");
    let types = event_types(&sandbox, &run_id);
    assert_eq!(
        types,
        vec![
            "run_created",
            "run_started",
            "run_verified",
            "run_completed"
        ],
        "{types:?}"
    );

    let verified = events
        .iter()
        .find(|envelope| envelope["event"]["type"] == "run_verified")
        .expect("run_verified event");
    assert_eq!(verified["event"]["data"]["outcome"], "unknown");
    assert_eq!(verified["aggregate_id"], run_id);
}

#[test]
fn run_budget_failure_exits_five() {
    let sandbox = Sandbox::new();
    let session_id = sandbox.create_session();

    let output = sandbox.run(&[
        "run",
        "--session",
        &session_id,
        "--objective",
        "summarize the csv",
        "--offline",
        "--max-steps",
        "0",
        "--json",
    ]);

    // A failed run is a result, not a CLI error: the outcome stays on stdout.
    assert_eq!(output.status.code(), Some(5), "stderr: {}", stderr(&output));
    assert!(stderr(&output).is_empty(), "stderr: {}", stderr(&output));

    let value: Value = serde_json::from_str(&stdout(&output)).expect("JSON outcome");
    assert_eq!(value["status"], "failed");
    let reason = value["reason"].as_str().expect("failure reason");
    assert!(reason.contains("budget exhausted"), "{reason}");
    assert!(reason.contains("step budget exceeded"), "{reason}");

    let run_id = value["run_id"].as_str().expect("run id").to_owned();
    let run = json_stdout(&sandbox.run(&["runs", "show", &run_id, "--json"]));
    assert_eq!(run["status"], "failed");

    let types = event_types(&sandbox, &run_id);
    assert!(types.contains(&"run_failed".to_owned()), "{types:?}");
    assert!(!types.contains(&"run_completed".to_owned()), "{types:?}");
}

#[test]
fn run_resume_unknown_run_exits_five() {
    let sandbox = Sandbox::new();

    let output = sandbox.run(&["run", "--resume", UNKNOWN_RUN, "--offline", "--json"]);

    assert_exit_five(&output, "not found");
    let error: Value = serde_json::from_str(&stderr(&output)).expect("JSON error object");
    assert_eq!(error["error"]["class"], "permanent");

    let runs = json_stdout(&sandbox.run(&["runs", "list", "--json"]));
    assert_eq!(runs, Value::Array(Vec::new()), "no run may be created");
}

#[test]
fn run_resume_terminal_run_exits_five() {
    let sandbox = Sandbox::new();
    let session_id = sandbox.create_session();

    let value = json_stdout(&sandbox.run(&[
        "run",
        "--session",
        &session_id,
        "--objective",
        "summarize the csv",
        "--offline",
        "--json",
    ]));
    let run_id = value["run_id"].as_str().expect("run id").to_owned();

    let output = sandbox.run(&["run", "--resume", &run_id, "--offline", "--json"]);
    assert_exit_five(&output, "already completed");

    // The run is untouched and no second run exists.
    let runs = json_stdout(&sandbox.run(&["runs", "list", "--json"]));
    let runs = runs.as_array().expect("runs are a JSON array");
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0]["id"], run_id);
}

#[test]
fn run_task_unknown_exits_five() {
    let sandbox = Sandbox::new();

    let output = sandbox.run(&["run", "--task", UNKNOWN_TASK, "--offline", "--json"]);

    assert_exit_five(&output, "not found");
    let runs = json_stdout(&sandbox.run(&["runs", "list", "--json"]));
    assert_eq!(runs, Value::Array(Vec::new()), "no run may be created");
}

#[test]
fn run_task_executes_an_existing_task() {
    let sandbox = Sandbox::new();
    let session_id = sandbox.create_session();
    let task = json_stdout(&sandbox.run(&[
        "task",
        "create",
        "--session",
        &session_id,
        "--objective",
        "summarize the csv",
        "--json",
    ]));
    let task_id = task["id"].as_str().expect("task id").to_owned();

    let value = json_stdout(&sandbox.run(&["run", "--task", &task_id, "--offline", "--json"]));

    assert_eq!(value["status"], "completed");
    assert_eq!(value["outcome"], "unknown");
    let run_id = value["run_id"].as_str().expect("run id").to_owned();
    let run = json_stdout(&sandbox.run(&["runs", "show", &run_id, "--json"]));
    assert_eq!(run["task_id"], task_id);
    assert_eq!(run["status"], "completed");
}

#[test]
fn run_without_a_source_exits_five_and_creates_no_database() {
    let sandbox = Sandbox::new();

    // Human mode: a permanent error on stderr, exit 5.
    let output = sandbox.run(&["run"]);
    assert_eq!(output.status.code(), Some(5), "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains("requires exactly one of --task, --resume"),
        "stderr: {}",
        stderr(&output)
    );
    assert!(
        !sandbox.database().exists(),
        "an invalid request must not open storage"
    );

    // JSON mode reports the same error class as every other permanent error.
    let output = sandbox.run(&["run", "--json"]);
    assert_eq!(output.status.code(), Some(5));
    let error: Value = serde_json::from_str(&stderr(&output)).expect("JSON error object");
    assert_eq!(error["error"]["class"], "permanent");
    assert!(
        error["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("requires exactly one of --task, --resume")),
        "{error}"
    );
    assert!(!sandbox.database().exists());
}

#[test]
fn run_offline_never_prints_api_key() {
    let sandbox = Sandbox::new();
    let session_id = sandbox.create_session();
    let secret = "sk-test-secret-value";

    // A key is present and the base URL is unreachable: offline mode must
    // neither read the key nor open a connection.
    let output = sandbox.run_with_env(
        &[
            "run",
            "--session",
            &session_id,
            "--objective",
            "summarize the csv",
            "--offline",
            "--json",
        ],
        &[
            ("DEEPSEEK_API_KEY", secret),
            ("SILYN_LLM_BASE_URL", "http://127.0.0.1:1/v1"),
        ],
    );

    assert_success(&output);
    assert_eq!(json_stdout(&output)["status"], "completed");
    assert!(
        !stdout(&output).contains(secret),
        "secret leaked to stdout: {}",
        stdout(&output)
    );
    assert!(
        !stderr(&output).contains(secret),
        "secret leaked to stderr: {}",
        stderr(&output)
    );
}

#[test]
fn run_rejects_conflicting_sources() {
    let sandbox = Sandbox::new();

    let output = sandbox.run(&[
        "run",
        "--resume",
        UNKNOWN_RUN,
        "--session",
        "0192f0a0-0000-7000-8000-000000000001",
        "--objective",
        "summarize the csv",
        "--offline",
    ]);

    assert_exit_five(&output, "cannot be combined");
    assert!(
        !sandbox.database().exists(),
        "an invalid request must not open storage"
    );
}

#[test]
fn run_groups_usage_errors_exit_two() {
    let sandbox = Sandbox::new();

    for args in [
        vec!["run", "--session", "not-a-uuid", "--objective", "x"],
        vec!["run", "--resume", "not-a-uuid"],
        vec!["run", "--task", "not-a-uuid"],
        // `--session` and `--objective` are only valid together.
        vec!["run", "--session", "0192f0a0-0000-7000-8000-000000000001"],
    ] {
        let output = sandbox.run(&args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "args {args:?}\nstderr: {}",
            stderr(&output)
        );
        assert!(stdout(&output).is_empty(), "args {args:?}");
    }
}
