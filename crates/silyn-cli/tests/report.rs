//! End-to-end tests for `silyn report run` and `silyn report experiment`.
//!
//! Every test uses its own temporary `SILYN_HOME` and that directory as the
//! working directory, so `dotenvy` cannot pick up a developer `.env`. Runs are
//! `--offline`: the deterministic mock provider never reads an API key and never
//! touches the network. Experiment reports are the committed W6 artifacts under
//! `eval/reports`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

/// Run id that never exists in a sandbox.
const UNKNOWN_RUN: &str = "0192f0a0-0000-7000-8000-0000000000ff";
/// A key-shaped canary: it must never appear in any report output.
const SECRET: &str = "sk-report-canary-value";

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

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
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

    fn create_session(&self) -> (String, String) {
        let agent = json_stdout(&self.run(&["agent", "create", "--owner", "local-user", "--json"]));
        let agent_id = agent["id"].as_str().expect("agent id").to_owned();
        let session =
            json_stdout(&self.run(&["session", "create", "--agent", &agent_id, "--json"]));
        let session_id = session["id"].as_str().expect("session id").to_owned();
        (agent_id, session_id)
    }

    /// Runs one offline task and returns its run id.
    fn offline_run(&self, session_id: &str) -> String {
        let outcome = json_stdout(&self.run(&[
            "run",
            "--session",
            session_id,
            "--objective",
            "summarize the csv",
            "--offline",
            "--json",
        ]));
        assert_eq!(outcome["status"], "completed", "{outcome}");
        outcome["run_id"].as_str().expect("run id").to_owned()
    }

    /// Runs one offline task that exhausts the step budget; the run fails but
    /// the command still prints its outcome.
    fn failed_run(&self, session_id: &str) -> String {
        let output = self.run(&[
            "run",
            "--session",
            session_id,
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
        outcome["run_id"].as_str().expect("run id").to_owned()
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

/// Path of one committed W6 report inside the repository.
fn report_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../eval/reports")
        .join(name)
}

fn as_str(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Renders a JSON scalar the way the Markdown renderer does.
fn scalar(value: &Value) -> String {
    match value {
        Value::Null => "-".to_owned(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[test]
fn report_run_json_contains_run_record_and_events() {
    let sandbox = Sandbox::new();
    let (_agent, session) = sandbox.create_session();
    let run_id = sandbox.offline_run(&session);

    let report = json_stdout(&sandbox.run(&["report", "run", &run_id, "--json"]));

    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["kind"], "run_report");
    assert_eq!(report["run"]["id"], run_id);
    assert_eq!(report["status"], "completed");
    assert!(report["run"]["status"] == "completed", "{report}");
    assert_eq!(report["terminal"], true);
    assert_eq!(report["outcome"], "unknown");
    assert_eq!(report["failure"], Value::Null);
    assert_eq!(report["usage"]["input_tokens"], 0);
    assert_eq!(report["usage"]["output_tokens"], 0);
    assert_eq!(report["usage"]["cost_usd"], 0.0);

    // The journal is bundled in sequence order and summarised by type.
    let events = report["events"].as_array().expect("events array");
    assert_eq!(report["event_count"], events.len());
    assert_eq!(events.len(), 4, "{events:?}");
    assert_eq!(events[0]["seq"], 1);
    assert_eq!(events[0]["event"]["type"], "run_created");
    let types: Vec<&str> = report["event_types"]
        .as_array()
        .expect("event_types")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(
        types,
        vec![
            "run_created",
            "run_started",
            "run_verified",
            "run_completed"
        ]
    );

    // The offline run makes no tool call, so the ledger is empty but present.
    assert_eq!(report["tool_calls"], Value::Array(Vec::new()));
    assert_eq!(report["tool_call_count"], 0);
    assert_eq!(report["tool_summary"]["started"], 0);
    assert_eq!(report["tool_summary"]["succeeded"], 0);
    assert_eq!(report["tool_summary"]["failed"], 0);
    assert_eq!(report["tool_summary"]["indeterminate"], 0);

    // Memory references are omitted rather than reported as an empty claim.
    assert!(
        !report
            .as_object()
            .expect("object")
            .contains_key("memory_refs"),
        "{report}"
    );
}

#[test]
fn report_run_records_the_failure_reason() {
    let sandbox = Sandbox::new();
    let (_agent, session) = sandbox.create_session();
    let run_id = sandbox.failed_run(&session);

    let report = json_stdout(&sandbox.run(&["report", "run", &run_id, "--json"]));

    assert_eq!(report["status"], "failed");
    assert_eq!(report["terminal"], true);
    assert_eq!(report["outcome"], Value::Null);
    let reason = report["failure"]["reason"]
        .as_str()
        .expect("failure reason from run_failed");
    assert!(reason.contains("budget exhausted"), "{reason}");
    assert!(
        report["event_types"]
            .as_array()
            .expect("event_types")
            .iter()
            .any(|kind| kind == "run_failed"),
        "{report}"
    );
}

#[test]
fn report_run_markdown_mirrors_json() {
    let sandbox = Sandbox::new();
    let (_agent, session) = sandbox.create_session();
    let run_id = sandbox.offline_run(&session);

    let report = json_stdout(&sandbox.run(&["report", "run", &run_id, "--json"]));
    let markdown = stdout(&sandbox.run(&["report", "run", &run_id, "--format", "markdown"]));

    assert!(
        markdown.contains(&format!("# Run report: {}", scalar(&report["run"]["id"]))),
        "{markdown}"
    );
    for pointer in [
        "/status",
        "/terminal",
        "/outcome",
        "/usage/input_tokens",
        "/usage/output_tokens",
        "/usage/cost_usd",
    ] {
        let expected = scalar(report.pointer(pointer).expect("field"));
        let label = pointer.trim_start_matches('/').replace('/', ".");
        assert!(
            markdown.contains(&expected),
            "markdown is missing {label}={expected}: {markdown}"
        );
    }
    assert!(
        markdown.contains(&format!("- status: {}", scalar(&report["status"]))),
        "{markdown}"
    );
    assert!(
        markdown.contains(&format!("- outcome: {}", scalar(&report["outcome"]))),
        "{markdown}"
    );
    assert!(
        markdown.contains(&format!(
            "| input_tokens | {} |",
            scalar(&report["usage"]["input_tokens"])
        )),
        "{markdown}"
    );
    assert!(
        markdown.contains(&format!("## Events ({})", scalar(&report["event_count"]))),
        "{markdown}"
    );
    assert!(
        markdown.contains(&format!(
            "## Tool calls ({})",
            scalar(&report["tool_call_count"])
        )),
        "{markdown}"
    );
    // Every journal event in the JSON has a row in the Markdown table.
    for envelope in report["events"].as_array().expect("events") {
        let seq = scalar(&envelope["seq"]);
        let kind = scalar(&envelope["event"]["type"]);
        assert!(
            markdown.contains(&format!("| {seq} | {kind} |")),
            "missing event row {seq}/{kind}: {markdown}"
        );
    }
}

#[test]
fn report_run_out_writes_the_payload() {
    let sandbox = Sandbox::new();
    let (_agent, session) = sandbox.create_session();
    let run_id = sandbox.offline_run(&session);
    let out = sandbox.path("run-report.json");

    let metadata =
        json_stdout(&sandbox.run(&["report", "run", &run_id, "--out", &as_str(&out), "--json"]));
    assert_eq!(metadata["path"], as_str(&out));
    assert_eq!(metadata["format"], "json");
    assert!(metadata["bytes"].as_u64().expect("bytes") > 0);

    let written: Value =
        serde_json::from_str(&std::fs::read_to_string(&out).expect("read out")).expect("out JSON");
    assert_eq!(written["run"]["id"], run_id);
    assert_eq!(written["status"], "completed");

    // Markdown output goes to the file too; stdout only confirms the write.
    let markdown_path = sandbox.path("run-report.md");
    let output = sandbox.run(&[
        "report",
        "run",
        &run_id,
        "--format",
        "markdown",
        "--out",
        &as_str(&markdown_path),
    ]);
    assert_success(&output);
    assert!(
        stdout(&output).contains(&format!("wrote {}", markdown_path.display())),
        "{}",
        stdout(&output)
    );
    let markdown = std::fs::read_to_string(&markdown_path).expect("read markdown");
    assert!(markdown.starts_with(&format!("# Run report: {run_id}")));
}

#[test]
fn report_run_unknown_run_exits_five() {
    let sandbox = Sandbox::new();

    let output = sandbox.run(&["report", "run", UNKNOWN_RUN, "--json"]);

    assert_exit_five(&output, "not found");
    let error: Value = serde_json::from_str(&stderr(&output)).expect("JSON error object");
    assert_eq!(error["error"]["class"], "permanent");
}

#[test]
fn report_experiment_json_reads_w6_reports() {
    let sandbox = Sandbox::new();
    let path = report_path("w6-dev.json");

    // No `--stats`: the sibling `w6-dev.stats.json` is discovered.
    let report = json_stdout(&sandbox.run(&["report", "experiment", &as_str(&path), "--json"]));

    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["kind"], "experiment_report");
    assert_eq!(report["model_id"], "deepseek-chat");
    assert_eq!(report["model_reported"], "deepseek-flash");
    assert_eq!(report["suite"]["version"], "csv-stats-v1.1");
    assert_eq!(report["suite"]["split"], "dev");
    assert_eq!(report["suite"]["sealed"], false);
    assert_eq!(report["verdict"], "reject");
    assert_eq!(
        report["reason"],
        "the policy arm regresses against the memory arm"
    );
    assert_eq!(report["unauthorized_actions"], 0);
    assert_eq!(report["arm_count"], 3);

    let arms = report["arms"].as_array().expect("arms array");
    let ids: Vec<&str> = arms.iter().filter_map(|arm| arm["id"].as_str()).collect();
    assert_eq!(ids, vec!["A", "B", "C"]);
    assert_eq!(arms[0]["successes"], 6);
    assert_eq!(arms[0]["total"], 10);
    assert_eq!(arms[0]["success_rate_pp"], 60.0);
    assert_eq!(arms[1]["label"], "memory-only");
    assert_eq!(arms[2]["label"], "memory-and-promoted-policy");

    // C-A carries the point estimate and the bootstrap interval.
    let c_minus_a = &report["comparisons"]["c_minus_a"];
    assert_eq!(c_minus_a["comparison"], "C-A");
    assert_eq!(c_minus_a["point_pp"], 0.0);
    assert_eq!(c_minus_a["ci_low_pp"], 0.0);
    assert_eq!(c_minus_a["ci_high_pp"], 0.0);
    assert_eq!(c_minus_a["paired_tasks"], 10);

    // Digests are copied from the statistics companion, not recomputed.
    assert_eq!(
        report["suite"]["digests"]["dev"],
        "f4abda23f50c22e75f8577ced0d66bc8364dddfd17f3d15738ef8f4a9f497f13"
    );
    assert_eq!(report["thresholds_source"], "stats");
    assert_eq!(report["thresholds"]["min_gain_pp"], 10.0);
    assert_eq!(report["thresholds"]["max_cost_usd"], 1.0);
}

#[test]
fn report_experiment_honours_an_explicit_stats_file() {
    let sandbox = Sandbox::new();
    let report = report_path("w6-test.json");
    let stats = report_path("w6-test.stats.json");

    let bundle = json_stdout(&sandbox.run(&[
        "report",
        "experiment",
        &as_str(&report),
        "--stats",
        &as_str(&stats),
        "--json",
    ]));

    assert_eq!(bundle["suite"]["split"], "test");
    assert_eq!(bundle["suite"]["sealed"], true);
    assert_eq!(bundle["comparisons"]["c_minus_a"]["point_pp"], -30.0);
    assert_eq!(bundle["comparisons"]["c_minus_a"]["ci_low_pp"], -60.0);
}

#[test]
fn report_experiment_without_stats_uses_the_report_only() {
    let sandbox = Sandbox::new();
    let source = report_path("w6-dev.json");
    let copy = sandbox.path("report-only.json");
    std::fs::copy(&source, &copy).expect("copy report");

    let bundle = json_stdout(&sandbox.run(&["report", "experiment", &as_str(&copy), "--json"]));

    assert_eq!(bundle["stats"], Value::Null);
    assert_eq!(bundle["suite"]["split"], Value::Null);
    assert_eq!(bundle["suite"]["digests"], Value::Null);
    assert_eq!(bundle["comparisons"], serde_json::json!({}));
    assert_eq!(bundle["reason"], Value::Null);
    assert_eq!(bundle["thresholds_source"], "report");
    assert_eq!(bundle["arm_count"], 3);
    // Rates are computed from the counts the report does carry.
    assert_eq!(bundle["arms"][1]["success_rate_pp"], 70.0);
}

#[test]
fn report_experiment_markdown_mirrors_json() {
    let sandbox = Sandbox::new();
    let path = report_path("w6-dev.json");

    let report = json_stdout(&sandbox.run(&["report", "experiment", &as_str(&path), "--json"]));
    let markdown = stdout(&sandbox.run(&[
        "report",
        "experiment",
        &as_str(&path),
        "--format",
        "markdown",
    ]));

    assert!(
        markdown.contains(&format!(
            "# Experiment report: {}",
            scalar(&report["suite"]["version"])
        )),
        "{markdown}"
    );
    for pointer in [
        "/model_id",
        "/model_reported",
        "/verdict",
        "/reason",
        "/suite/split",
        "/unauthorized_actions",
    ] {
        let expected = scalar(report.pointer(pointer).expect("field"));
        assert!(
            markdown.contains(&expected),
            "markdown is missing {pointer}={expected}: {markdown}"
        );
    }
    assert!(
        markdown.contains(&format!("## Arms ({})", scalar(&report["arm_count"]))),
        "{markdown}"
    );

    for arm in report["arms"].as_array().expect("arms") {
        let id = scalar(&arm["id"]);
        let label = scalar(&arm["label"]);
        let successes = scalar(&arm["successes"]);
        let total = scalar(&arm["total"]);
        let rate = scalar(&arm["success_rate_pp"]);
        assert!(
            markdown.contains(&format!(
                "| {id} | {label} | {successes} | {total} | {rate} |"
            )),
            "missing arm row {id}: {markdown}"
        );
    }

    for comparison in report["comparisons"]
        .as_object()
        .expect("comparisons")
        .values()
    {
        let name = scalar(&comparison["comparison"]);
        let point = scalar(&comparison["point_pp"]);
        let low = scalar(&comparison["ci_low_pp"]);
        let high = scalar(&comparison["ci_high_pp"]);
        let paired = scalar(&comparison["paired_tasks"]);
        assert!(
            markdown.contains(&format!("| {name} | {point} | {low} | {high} | {paired} |")),
            "missing comparison row {name}: {markdown}"
        );
    }
}

#[test]
fn report_experiment_unknown_path_exits_five() {
    let sandbox = Sandbox::new();
    let missing = sandbox.path("missing-report.json");

    let output = sandbox.run(&["report", "experiment", &as_str(&missing), "--json"]);

    assert_exit_five(&output, "cannot read evaluation report");
    let error: Value = serde_json::from_str(&stderr(&output)).expect("JSON error object");
    assert_eq!(error["error"]["class"], "permanent");
}

#[test]
fn report_experiment_rejects_mismatched_suite_versions() {
    let sandbox = Sandbox::new();
    let report = sandbox.path("report.json");
    std::fs::write(
        &report,
        serde_json::to_string_pretty(&serde_json::json!({
            "schema_version": 1,
            "candidate_id": "0192f0a0-0000-7000-8000-0000000000cc",
            "suite_version": "csv-stats-v1.1",
            "model_id": "deepseek-chat",
            "baseline_id": "B",
            "repeats": 1,
            "arms": {},
            "thresholds": {},
            "verdict": "reject",
        }))
        .expect("report json"),
    )
    .expect("write report");
    let stats = sandbox.path("stats.json");
    std::fs::write(
        &stats,
        serde_json::to_string_pretty(&serde_json::json!({
            "schema_version": 1,
            "suite_version": "csv-stats-v0",
            "verdict": "reject",
        }))
        .expect("stats json"),
    )
    .expect("write stats");

    let output = sandbox.run(&[
        "report",
        "experiment",
        &as_str(&report),
        "--stats",
        &as_str(&stats),
        "--json",
    ]);

    assert_exit_five(&output, "does not match the report");
}

#[test]
fn report_output_never_contains_secrets() {
    let sandbox = Sandbox::new();
    let (_agent, session) = sandbox.create_session();
    let run_id = sandbox.offline_run(&session);
    let report = report_path("w6-dev.json");

    // A key is present in the environment; neither encoding may echo it or any
    // key-shaped text.
    for args in [
        vec!["report", "run", &run_id, "--json"],
        vec!["report", "run", &run_id, "--format", "markdown"],
        vec!["report", "experiment", &as_str(&report), "--json"],
        vec![
            "report",
            "experiment",
            &as_str(&report),
            "--format",
            "markdown",
        ],
    ] {
        let output = sandbox.run_with_env(&args, &[("DEEPSEEK_API_KEY", SECRET)]);
        assert_success(&output);
        for text in [stdout(&output), stderr(&output)] {
            assert!(!text.contains(SECRET), "secret leaked: {text}");
            assert!(!text.contains("sk-"), "key-shaped text leaked: {text}");
        }
    }
}

#[test]
fn report_usage_errors_exit_two() {
    let sandbox = Sandbox::new();

    for args in [
        vec!["report", "run", "not-a-uuid"],
        vec!["report", "run", UNKNOWN_RUN, "--format", "yaml"],
        vec!["report", "experiment"],
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
