//! End-to-end tests for the direct `silyn "<objective>"` form and its config.
//!
//! Every test uses a temporary `SILYN_HOME` and that directory as the working
//! directory, so no developer `.env` is picked up. Runs are offline through the
//! flag or through the config file, never over the network.

use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

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

    fn config_path(&self) -> PathBuf {
        self.home().join("config.toml")
    }

    fn write_config(&self, text: &str) {
        std::fs::create_dir_all(self.home()).expect("home");
        std::fs::write(self.config_path(), text).expect("config");
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_silyn"))
            .args(args)
            .current_dir(self.dir.path())
            .env("SILYN_HOME", self.home())
            .env_remove("SILYN_LLM_API_KEY_ENV")
            .env_remove("SILYN_LLM_MODEL")
            .env_remove("DEEPSEEK_API_KEY")
            .output()
            .expect("run silyn")
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn json_stdout(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "command failed with {:?}\nstderr: {}",
        output.status.code(),
        stderr(output)
    );
    serde_json::from_str(&stdout(output)).expect("JSON stdout")
}

#[test]
fn prompt_runs_offline_and_reuses_the_agent() {
    let sandbox = Sandbox::new();

    let first = json_stdout(&sandbox.run(&["profile the csv", "--offline", "--json"]));
    assert_eq!(first["status"], "completed");
    assert_eq!(first["final_answer"], OFFLINE_ANSWER);

    let second = json_stdout(&sandbox.run(&["profile the csv again", "--offline", "--json"]));
    assert_eq!(second["status"], "completed");
    assert_ne!(first["run_id"], second["run_id"]);

    let agents = json_stdout(&sandbox.run(&["agent", "list", "--json"]));
    assert_eq!(agents.as_array().expect("agents").len(), 1, "{agents}");

    let runs = json_stdout(&sandbox.run(&["runs", "list", "--json"]));
    assert_eq!(runs.as_array().expect("runs").len(), 2, "{runs}");
}

#[test]
fn human_prompt_prints_the_answer_on_stdout() {
    let sandbox = Sandbox::new();

    let output = sandbox.run(&["say hello", "--offline"]);

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output).trim(), OFFLINE_ANSWER);
    assert!(
        stderr(&output).starts_with("silyn: run "),
        "stderr: {}",
        stderr(&output)
    );
}

#[test]
fn config_file_supplies_run_defaults() {
    let sandbox = Sandbox::new();
    sandbox.write_config(
        r#"
[run]
offline = true
model = "config-model"
capabilities = ["fs.read"]
"#,
    );

    let outcome = json_stdout(&sandbox.run(&["summarize the csv", "--json"]));
    assert_eq!(outcome["status"], "completed");

    let runs = json_stdout(&sandbox.run(&["runs", "list", "--json"]));
    let run = &runs.as_array().expect("runs")[0];
    assert_eq!(run["model_id"], "config-model", "{run}");
}

#[test]
fn flags_override_the_config_file() {
    let sandbox = Sandbox::new();
    sandbox.write_config(
        r#"
[run]
offline = true
model = "config-model"
"#,
    );

    let outcome =
        json_stdout(&sandbox.run(&["summarize the csv", "--model", "flag-model", "--json"]));
    assert_eq!(outcome["status"], "completed");

    let runs = json_stdout(&sandbox.run(&["runs", "list", "--json"]));
    let run = &runs.as_array().expect("runs")[0];
    assert_eq!(run["model_id"], "flag-model", "{run}");
}

#[test]
fn invalid_config_is_a_permanent_error() {
    let sandbox = Sandbox::new();
    sandbox.write_config("[run]\nmax_stpes = 3\n");

    let output = sandbox.run(&["say hello"]);

    assert_eq!(output.status.code(), Some(5), "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains("config file"),
        "stderr: {}",
        stderr(&output)
    );
    assert!(stdout(&output).is_empty());
}

#[test]
fn missing_explicit_config_is_a_permanent_error() {
    let sandbox = Sandbox::new();
    let missing = sandbox.dir.path().join("nope.toml");
    let missing = missing.to_string_lossy().into_owned();

    let output = sandbox.run(&["--config", &missing, "doctor"]);

    assert_eq!(output.status.code(), Some(5), "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains("nope.toml"),
        "stderr: {}",
        stderr(&output)
    );
}

#[test]
fn config_command_reports_the_loaded_file() {
    let sandbox = Sandbox::new();
    sandbox.write_config("[run]\noffline = true\n");

    let value = json_stdout(&sandbox.run(&["config", "--json"]));
    assert_eq!(value["config_loaded"], true);
    assert_eq!(value["run"]["offline"], true);
    assert_eq!(
        value["config_file"],
        sandbox.config_path().to_string_lossy().as_ref()
    );
}
