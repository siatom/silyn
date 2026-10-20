//! The `--json` surface of every read command.
//!
//! Each test seeds one sandbox through the CLI itself (offline runs only) and
//! then asserts that every read command emits exactly one valid JSON document on
//! stdout. Errors keep their JSON shape on stderr.

use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::{Value, json};
use silyn_core::ids::CandidateId;
use tempfile::TempDir;

struct Sandbox {
    dir: TempDir,
}

/// Everything the read commands need, seeded once per test.
struct World {
    agent: String,
    run: String,
    candidate: String,
    report: PathBuf,
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

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
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

    fn write_json(&self, name: &str, value: &Value) -> PathBuf {
        let path = self.path(name);
        std::fs::write(&path, serde_json::to_string_pretty(value).expect("json"))
            .expect("write json");
        path
    }

    /// Seeds an agent with an active policy, one memory, one completed offline
    /// run and one static-checked candidate derived from that run.
    fn seed(&self) -> World {
        let agent = json_stdout(&self.run(&["agent", "create", "--owner", "local-user", "--json"]));
        let agent_id = agent["id"].as_str().expect("agent id").to_owned();

        let base = json_stdout(&self.run(&["evolution", "init", "--agent", &agent_id, "--json"]));
        let base_id = base["id"].as_str().expect("base policy id").to_owned();

        let session =
            json_stdout(&self.run(&["session", "create", "--agent", &agent_id, "--json"]));
        let session_id = session["id"].as_str().expect("session id").to_owned();

        let outcome = json_stdout(&self.run(&[
            "run",
            "--session",
            &session_id,
            "--objective",
            "count the distinct rows",
            "--offline",
            "--json",
        ]));
        let run_id = outcome["run_id"].as_str().expect("run id").to_owned();

        json_stdout(&self.run(&[
            "memory",
            "add",
            "--agent",
            &agent_id,
            "--summary",
            "deploy window",
            "--content",
            "Deploy window starts Monday.",
            "--json",
        ]));

        let patch = json!({
            "schema_version": 1,
            "candidate_id": CandidateId::new().to_string(),
            "base_policy_version": base_id,
            "hypothesis": "Reordering the rules improves row-count accuracy.",
            "evidence_run_ids": [run_id],
            "expected_gain": { "metric": "success_rate", "delta_pp": 12.0 },
            "risk_class": "low",
            "operations": [{
                "op": "replace",
                "path": "/planner/rules",
                "value": ["Recompute row counts from distinct rows."],
            }],
        });
        let patch_path = self.write_json("patch.json", &patch);
        let candidate = json_stdout(&self.run(&[
            "evolution",
            "submit",
            "--agent",
            &agent_id,
            "--patch",
            patch_path.to_str().expect("patch path"),
            "--json",
        ]));
        let candidate_id = candidate["id"].as_str().expect("candidate id").to_owned();

        World {
            agent: agent_id,
            run: run_id,
            candidate: candidate_id,
            report: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../eval/reports/w6-dev.json"),
        }
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
        "command failed with {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        stdout(output),
        stderr(output)
    );
}

fn json_stdout(output: &Output) -> Value {
    assert_success(output);
    serde_json::from_str(&stdout(output))
        .unwrap_or_else(|error| panic!("stdout is not JSON: {error}\n{}", stdout(output)))
}

fn assert_json_stdout(output: &Output, label: &str) -> Value {
    assert_success(output);
    let text = stdout(output);
    let value: Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("{label}: stdout is not JSON: {error}\n{text}"));
    assert!(!text.trim().is_empty(), "{label}: empty stdout");
    value
}

#[test]
fn every_read_command_emits_json() {
    let sandbox = Sandbox::new();
    let world = sandbox.seed();

    let report = world.report.to_string_lossy().into_owned();
    let commands: Vec<Vec<&str>> = vec![
        vec!["init", "--json"],
        vec!["doctor", "--json"],
        vec!["config", "--json"],
        vec!["agent", "list", "--json"],
        vec!["runs", "list", "--json"],
        vec!["runs", "show", &world.run, "--json"],
        vec!["runs", "events", &world.run, "--json"],
        vec!["runs", "tools", &world.run, "--json"],
        vec!["memory", "list", "--agent", &world.agent, "--json"],
        vec![
            "memory",
            "search",
            "--agent",
            &world.agent,
            "deploy",
            "--json",
        ],
        vec!["experience", "list", "--agent", &world.agent, "--json"],
        vec!["evolution", "list", "--agent", &world.agent, "--json"],
        vec![
            "evolution",
            "show",
            &world.candidate,
            "--agent",
            &world.agent,
            "--json",
        ],
        vec!["report", "run", &world.run, "--json"],
        vec!["report", "experiment", &report, "--json"],
    ];

    for args in commands {
        let label = args.join(" ");
        let output = sandbox.run(&args);
        assert_json_stdout(&output, &label);
        for text in [stdout(&output), stderr(&output)] {
            assert!(!text.contains("sk-"), "{label}: key-shaped text in {text}");
        }
    }
}

#[test]
fn read_command_errors_are_json_on_stderr() {
    let sandbox = Sandbox::new();
    let missing = sandbox.path("missing-report.json");
    let unknown_run = "0192f0a0-0000-7000-8000-0000000000ff";

    for args in [
        vec!["runs", "show", unknown_run, "--json"],
        vec!["runs", "events", unknown_run, "--json"],
        vec![
            "report",
            "experiment",
            missing.to_str().expect("path"),
            "--json",
        ],
    ] {
        let output = sandbox.run(&args);
        assert_eq!(
            output.status.code(),
            Some(5),
            "args {args:?}\nstderr: {}",
            stderr(&output)
        );
        assert!(stdout(&output).is_empty(), "args {args:?}");
        let error: Value = serde_json::from_str(&stderr(&output))
            .unwrap_or_else(|error| panic!("args {args:?}: stderr is not JSON: {error}"));
        assert_eq!(error["error"]["class"], "permanent", "args {args:?}");
        assert!(
            error["error"]["message"]
                .as_str()
                .is_some_and(|message| !message.is_empty()),
            "args {args:?}"
        );
    }
}
