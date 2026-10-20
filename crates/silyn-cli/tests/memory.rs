//! End-to-end tests for `silyn memory` and `silyn experience`.
//!
//! Every test uses its own temporary `SILYN_HOME` and that directory as the
//! working directory, so `dotenvy` cannot pick up a developer `.env`. Runs are
//! `--offline`: the deterministic mock provider never reads an API key and never
//! touches the network.

use std::path::PathBuf;
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

    fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_silyn"));
        command
            .args(args)
            .current_dir(self.dir.path())
            .env("SILYN_HOME", self.home())
            .env_remove("SILYN_LLM_API_KEY_ENV")
            .env_remove("SILYN_LLM_MODEL")
            .env_remove("DEEPSEEK_API_KEY");
        command.output().expect("run silyn")
    }

    fn create_agent(&self, owner: &str) -> String {
        let agent = json_stdout(&self.run(&["agent", "create", "--owner", owner, "--json"]));
        agent["id"].as_str().expect("agent id").to_owned()
    }

    fn create_session(&self, agent_id: &str) -> String {
        let session = json_stdout(&self.run(&["session", "create", "--agent", agent_id, "--json"]));
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

#[test]
fn memory_add_list_search_delete_round_trip() {
    let sandbox = Sandbox::new();
    let agent = sandbox.create_agent("local-user");

    let record = json_stdout(&sandbox.run(&[
        "memory",
        "add",
        "--agent",
        &agent,
        "--summary",
        "deploy window rule",
        "--content",
        "Deploy window starts Monday and needs two approvals.",
        "--scope",
        "agent",
        "--provenance",
        "user_statement",
        "--json",
    ]));
    let memory_id = record["id"].as_str().expect("memory id").to_owned();
    assert_eq!(record["agent_id"], agent);
    assert_eq!(record["summary"], "deploy window rule");
    assert_eq!(record["provenance"], "user_statement");
    assert_eq!(record["scope"], "agent");
    assert_eq!(record["status"], "active");

    let listed = json_stdout(&sandbox.run(&["memory", "list", "--agent", &agent, "--json"]));
    let listed = listed.as_array().expect("memory list is an array");
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0]["id"], memory_id);

    let hits =
        json_stdout(&sandbox.run(&["memory", "search", "--agent", &agent, "deploy", "--json"]));
    let hits = hits.as_array().expect("search is an array");
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0]["id"], memory_id);

    // Another agent never sees the first agent's memory.
    let other = sandbox.create_agent("other-user");
    let hits =
        json_stdout(&sandbox.run(&["memory", "search", "--agent", &other, "deploy", "--json"]));
    assert_eq!(hits, Value::Array(Vec::new()));

    let deleted =
        json_stdout(&sandbox.run(&["memory", "delete", "--agent", &agent, &memory_id, "--json"]));
    assert_eq!(deleted["memory_id"], memory_id);
    assert_eq!(deleted["deleted"], true);

    let hits =
        json_stdout(&sandbox.run(&["memory", "search", "--agent", &agent, "deploy", "--json"]));
    assert_eq!(
        hits,
        Value::Array(Vec::new()),
        "soft-deleted memory disappears"
    );
    let listed = json_stdout(&sandbox.run(&["memory", "list", "--agent", &agent, "--json"]));
    assert_eq!(listed, Value::Array(Vec::new()));
}

#[test]
fn experience_list_shows_provisional_after_failed_run() {
    let sandbox = Sandbox::new();
    let agent = sandbox.create_agent("local-user");
    let session = sandbox.create_session(&agent);

    let output = sandbox.run(&[
        "run",
        "--session",
        &session,
        "--objective",
        "summarize the csv",
        "--offline",
        "--max-steps",
        "0",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(5), "stderr: {}", stderr(&output));
    let outcome: Value = serde_json::from_str(&stdout(&output)).expect("run outcome JSON");
    assert_eq!(outcome["status"], "failed");

    let experiences =
        json_stdout(&sandbox.run(&["experience", "list", "--agent", &agent, "--json"]));
    let experiences = experiences.as_array().expect("experience list is an array");
    assert_eq!(experiences.len(), 1, "{experiences:?}");
    assert_eq!(experiences[0]["evidence_status"], "provisional");
    assert_eq!(
        experiences[0]["lesson"],
        "Verify the result before reporting it."
    );
    assert_eq!(experiences[0]["agent_id"], agent);
}

#[test]
fn memory_delete_of_a_foreign_memory_exits_five() {
    let sandbox = Sandbox::new();
    let owner = sandbox.create_agent("local-user");
    let other = sandbox.create_agent("other-user");

    let record = json_stdout(&sandbox.run(&[
        "memory",
        "add",
        "--agent",
        &owner,
        "--summary",
        "private note",
        "--content",
        "owner only",
        "--json",
    ]));
    let memory_id = record["id"].as_str().expect("memory id").to_owned();

    let output = sandbox.run(&["memory", "delete", "--agent", &other, &memory_id, "--json"]);
    assert_eq!(output.status.code(), Some(5), "stderr: {}", stderr(&output));
    let error: Value = serde_json::from_str(&stderr(&output)).expect("JSON error object");
    assert_eq!(error["error"]["class"], "permanent");

    // The owner still sees it.
    let hits =
        json_stdout(&sandbox.run(&["memory", "search", "--agent", &owner, "private", "--json"]));
    assert_eq!(hits.as_array().expect("hits").len(), 1);
}

#[test]
fn memory_usage_errors_exit_two() {
    let sandbox = Sandbox::new();
    let agent = sandbox.create_agent("local-user");

    for args in [
        vec![
            "memory",
            "add",
            "--agent",
            &agent,
            "--summary",
            " ",
            "--content",
            "x",
        ],
        vec![
            "memory",
            "add",
            "--agent",
            &agent,
            "--summary",
            "x",
            "--content",
            " ",
        ],
        vec![
            "memory",
            "add",
            "--agent",
            &agent,
            "--summary",
            "x",
            "--content",
            "y",
            "--scope",
            "user",
        ],
        vec!["memory", "delete", "--agent", &agent, "not-a-uuid"],
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
