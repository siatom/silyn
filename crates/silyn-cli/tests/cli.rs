//! End-to-end tests for the CLI surface.
//!
//! Each test runs in its own temporary `SILYN_HOME` and uses that temporary
//! directory as the working directory. `dotenvy` therefore cannot pick up a
//! developer `.env`, and the tests never touch the real `.silyn` directory.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

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
            .env_remove("DEEPSEEK_API_KEY");
        for (key, value) in envs {
            command.env(key, value);
        }
        command.output().expect("run silyn")
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

fn assert_uuid_shaped(value: &str) {
    assert_eq!(value.len(), 36, "not a UUID: {value}");
    assert_eq!(value.matches('-').count(), 4, "not a UUID: {value}");
    assert!(
        value.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
        "not a UUID: {value}"
    );
}

#[test]
fn init_creates_database() {
    let sandbox = Sandbox::new();
    assert!(!sandbox.database().exists());

    let value = json_stdout(&sandbox.run(&["init", "--json"]));

    assert_eq!(value["home"], sandbox.home().to_string_lossy().as_ref());
    assert_eq!(
        value["database"],
        sandbox.database().to_string_lossy().as_ref()
    );
    assert!(sandbox.home().is_dir(), "home directory must exist");
    assert!(sandbox.database().is_file(), "database file must exist");

    // The human-readable form reports the same paths.
    let output = sandbox.run(&["init"]);
    assert_success(&output);
    let database = sandbox.database().to_string_lossy().into_owned();
    assert!(stdout(&output).contains(&database), "{}", stdout(&output));
}

#[test]
fn init_is_idempotent() {
    let sandbox = Sandbox::new();
    assert_success(&sandbox.run(&["init"]));
    assert_success(&sandbox.run(&["init"]));
    assert!(sandbox.database().is_file());
}

#[test]
fn agent_json_output_round_trips() {
    let sandbox = Sandbox::new();

    let created =
        json_stdout(&sandbox.run(&["agent", "create", "--owner", "local-user", "--json"]));
    let id = created["id"].as_str().expect("agent id").to_owned();
    assert_uuid_shaped(&id);
    assert_eq!(created["owner_id"], "local-user");
    assert_eq!(created["profile_version"], "profile-v1");
    assert!(created["created_at"].is_string(), "created_at is required");

    let listed = json_stdout(&sandbox.run(&["agent", "list", "--json"]));
    let agents = listed.as_array().expect("agent list is an array");
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0]["id"], id);

    // Human output of `create` is the bare id, so scripts can capture it.
    let output = sandbox.run(&["agent", "create", "--owner", "second-user"]);
    assert_success(&output);
    let printed = stdout(&output).trim().to_owned();
    assert_uuid_shaped(&printed);
    assert_ne!(printed, id);
}

#[test]
fn session_and_task_creation_returns_ids() {
    let sandbox = Sandbox::new();

    let created =
        json_stdout(&sandbox.run(&["agent", "create", "--owner", "local-user", "--json"]));
    let agent_id = created["id"].as_str().expect("agent id").to_owned();

    let session = json_stdout(&sandbox.run(&["session", "create", "--agent", &agent_id, "--json"]));
    let session_id = session["id"].as_str().expect("session id").to_owned();
    assert_uuid_shaped(&session_id);
    assert_eq!(session["agent_id"], agent_id);
    assert_eq!(session["context_version"], "ctx-v1");

    let task = json_stdout(&sandbox.run(&[
        "task",
        "create",
        "--session",
        &session_id,
        "--objective",
        "summarize the csv",
        "--json",
    ]));
    assert_uuid_shaped(task["id"].as_str().expect("task id"));
    assert_eq!(task["session_id"], session_id);
    assert_eq!(task["objective"], "summarize the csv");
    assert_eq!(task["status"], "created");

    // Human output of `session create` and `task create` is the bare id.
    let output = sandbox.run(&["session", "create", "--agent", &agent_id]);
    assert_success(&output);
    assert_uuid_shaped(stdout(&output).trim());
}

#[test]
fn runs_list_returns_json_array() {
    let sandbox = Sandbox::new();

    let value = json_stdout(&sandbox.run(&["runs", "list", "--json"]));
    assert_eq!(value, Value::Array(Vec::new()));
    assert!(sandbox.database().is_file(), "runs list opens the database");

    // The table form stays script-safe even with no rows.
    let output = sandbox.run(&["runs", "list"]);
    assert_success(&output);
    assert!(stdout(&output).starts_with("ID"), "{}", stdout(&output));
}

#[test]
fn run_exits_permanent_without_creating_a_run() {
    let sandbox = Sandbox::new();

    let output = sandbox.run(&["run"]);

    assert_eq!(output.status.code(), Some(5), "stderr: {}", stderr(&output));
    // W3-03 replaced the "runtime loop is not available" placeholder with the
    // real command; a request without a source is still a permanent error and
    // still must not open storage.
    assert!(
        stderr(&output).contains("requires exactly one of --task, --resume"),
        "stderr: {}",
        stderr(&output)
    );
    assert!(
        !sandbox.database().exists(),
        "`run` must not create storage for an invalid request"
    );
}

#[test]
fn missing_run_is_a_permanent_error() {
    let sandbox = Sandbox::new();
    assert_success(&sandbox.run(&["init"]));

    let output = sandbox.run(&["runs", "show", "0192f0a0-0000-7000-8000-000000000000"]);

    assert_eq!(output.status.code(), Some(5), "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains("not found"),
        "stderr: {}",
        stderr(&output)
    );

    // JSON mode reports the error class on stderr, not on stdout.
    let output = sandbox.run(&[
        "runs",
        "show",
        "0192f0a0-0000-7000-8000-000000000000",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(5));
    let error: Value = serde_json::from_str(&stderr(&output)).expect("JSON error object");
    assert_eq!(error["error"]["class"], "permanent");
    assert!(
        error["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("not found"))
    );
}

#[test]
fn unknown_parent_ids_exit_five() {
    let sandbox = Sandbox::new();
    assert_success(&sandbox.run(&["init"]));

    let output = sandbox.run(&[
        "session",
        "create",
        "--agent",
        "0192f0a0-0000-7000-8000-000000000001",
    ]);
    assert_eq!(output.status.code(), Some(5), "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains("not found"),
        "stderr: {}",
        stderr(&output)
    );

    let output = sandbox.run(&[
        "task",
        "create",
        "--session",
        "0192f0a0-0000-7000-8000-000000000002",
        "--objective",
        "do the thing",
    ]);
    assert_eq!(output.status.code(), Some(5), "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains("not found"),
        "stderr: {}",
        stderr(&output)
    );
}

#[test]
fn usage_errors_exit_two() {
    let sandbox = Sandbox::new();

    for args in [
        vec!["agent", "create"],
        vec!["agent", "create", "--owner", "  "],
        vec!["runs", "show", "not-a-uuid"],
        vec!["session", "create", "--agent", "not-a-uuid"],
        vec!["nonsense"],
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

    assert!(
        !sandbox.home().exists(),
        "usage errors must not create the home directory"
    );
}

#[test]
fn doctor_json_never_prints_secrets() {
    let sandbox = Sandbox::new();
    let secret = "sk-test-secret-value";

    let output = sandbox.run_with_env(&["doctor", "--json"], &[("DEEPSEEK_API_KEY", secret)]);

    let value = json_stdout(&output);
    assert_eq!(value["llm"]["api_key_present"], true);
    assert_eq!(value["llm"]["api_key_env"], "DEEPSEEK_API_KEY");
    assert!(!stdout(&output).contains(secret), "secret leaked to stdout");
    assert!(!stderr(&output).contains(secret), "secret leaked to stderr");
}

#[test]
fn config_json_reports_non_secret_settings() {
    let sandbox = Sandbox::new();
    let value = json_stdout(&sandbox.run(&["config", "--json"]));
    assert_eq!(value["llm"]["api_key_present"], false);
    assert_eq!(
        value["home"],
        Path::new(&sandbox.home()).to_string_lossy().as_ref()
    );
}
