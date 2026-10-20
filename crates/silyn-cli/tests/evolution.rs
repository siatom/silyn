//! End-to-end tests for `silyn evolution`: policy bootstrap, candidate
//! submission, report attachment, low-risk auto-promotion, operator approval,
//! rollback and unknown-id handling.
//!
//! Every test uses its own temporary `SILYN_HOME` and that directory as the
//! working directory, so `dotenvy` cannot pick up a developer `.env`. Evidence
//! runs are `--offline`: the deterministic mock provider never reads an API key
//! and never touches the network. Evaluation reports are hand-written JSON; no
//! evaluator is invoked.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};
use silyn_core::ids::CandidateId;
use tempfile::TempDir;

/// Candidate id that never exists in a sandbox.
const UNKNOWN_CANDIDATE: &str = "0192f0a0-0000-7000-8000-0000000000cc";
/// Promotion id that never exists in a sandbox.
const UNKNOWN_PROMOTION: &str = "0192f0a0-0000-7000-8000-0000000000dd";
/// Agent id that never exists in a sandbox.
const UNKNOWN_AGENT: &str = "0192f0a0-0000-7000-8000-0000000000aa";

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

    fn run_with_stdin(&self, args: &[&str], input: &str) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_silyn"));
        command
            .args(args)
            .current_dir(self.dir.path())
            .env("SILYN_HOME", self.home())
            .env_remove("SILYN_LLM_API_KEY_ENV")
            .env_remove("SILYN_LLM_MODEL")
            .env_remove("DEEPSEEK_API_KEY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("spawn silyn");
        {
            use std::io::Write as _;
            child
                .stdin
                .as_mut()
                .expect("stdin")
                .write_all(input.as_bytes())
                .expect("write stdin");
        }
        child.wait_with_output().expect("wait silyn")
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn write_json(&self, name: &str, value: &Value) -> PathBuf {
        let path = self.path(name);
        std::fs::write(&path, serde_json::to_string_pretty(value).expect("json"))
            .expect("write json");
        path
    }

    fn create_agent(&self, owner: &str) -> String {
        let agent = json_stdout(&self.run(&["agent", "create", "--owner", owner, "--json"]));
        agent["id"].as_str().expect("agent id").to_owned()
    }

    fn create_session(&self, agent_id: &str) -> String {
        let session = json_stdout(&self.run(&["session", "create", "--agent", agent_id, "--json"]));
        session["id"].as_str().expect("session id").to_owned()
    }

    /// Runs one offline task and returns the completed run id.
    fn evidence_run(&self, agent_id: &str) -> String {
        let session = self.create_session(agent_id);
        let outcome = json_stdout(&self.run(&[
            "run",
            "--session",
            &session,
            "--objective",
            "count the distinct rows",
            "--offline",
            "--json",
        ]));
        assert_eq!(outcome["status"], "completed", "{outcome}");
        outcome["run_id"].as_str().expect("run id").to_owned()
    }

    fn init_policy(&self, agent_id: &str) -> Value {
        json_stdout(&self.run(&["evolution", "init", "--agent", agent_id, "--json"]))
    }

    fn submit(&self, agent_id: &str, patch: &Value) -> Output {
        let name = format!("patch-{}.json", CandidateId::new());
        let path = self.write_json(&name, patch);
        self.run(&[
            "evolution",
            "submit",
            "--agent",
            agent_id,
            "--patch",
            path.to_str().expect("patch path"),
            "--json",
        ])
    }

    /// Writes the report and its `<report>.stats.json` companion.
    fn evaluation_files(
        &self,
        candidate_id: &str,
        verdict: &str,
        point_pp: f64,
        ci_low_pp: f64,
        unauthorized_actions: u64,
    ) -> (PathBuf, PathBuf) {
        let name = format!("report-{candidate_id}.json");
        let report = report_document(candidate_id, verdict);
        let report_path = self.write_json(&name, &report);
        let stats_path = report_path.with_extension("stats.json");
        let stats = stats_document(verdict, point_pp, ci_low_pp, unauthorized_actions);
        std::fs::write(
            &stats_path,
            serde_json::to_string_pretty(&stats).expect("stats json"),
        )
        .expect("write stats");
        (report_path, stats_path)
    }

    fn evaluate(
        &self,
        agent_id: &str,
        candidate_id: &str,
        verdict: &str,
        point_pp: f64,
        ci_low_pp: f64,
        unauthorized_actions: u64,
    ) -> Value {
        let (report, _stats) = self.evaluation_files(
            candidate_id,
            verdict,
            point_pp,
            ci_low_pp,
            unauthorized_actions,
        );
        json_stdout(&self.run(&[
            "evolution",
            "evaluate",
            candidate_id,
            "--agent",
            agent_id,
            "--report",
            report.to_str().expect("report path"),
            "--json",
        ]))
    }

    fn show(&self, agent_id: &str, candidate_id: &str) -> Value {
        json_stdout(&self.run(&[
            "evolution",
            "show",
            candidate_id,
            "--agent",
            agent_id,
            "--json",
        ]))
    }
}

fn report_document(candidate_id: &str, verdict: &str) -> Value {
    json!({
        "schema_version": 1,
        "candidate_id": candidate_id,
        "suite_version": "csv-stats-v1",
        "model_id": "deterministic-baseline",
        "baseline_id": "B",
        "repeats": 1,
        "arms": {
            "A": { "successes": 1, "total": 6, "cost_usd": 0.0 },
            "B": { "successes": 2, "total": 6, "cost_usd": 0.0 },
            "C": { "successes": 4, "total": 6, "cost_usd": 0.0 },
        },
        "thresholds": {
            "min_gain_pp": 10.0,
            "max_cost_ratio": 2.0,
            "max_regression_pp": 5.0,
        },
        "verdict": verdict,
        "created_at": "2026-10-21T00:00:00Z",
    })
}

fn stats_document(
    verdict: &str,
    point_pp: f64,
    ci_low_pp: f64,
    unauthorized_actions: u64,
) -> Value {
    json!({
        "schema_version": 1,
        "suite_version": "csv-stats-v1",
        "split": "dev",
        "sealed": false,
        "repeats": 1,
        "seed": 42,
        "model_id": "deterministic-baseline",
        "baseline_id": "B",
        "backend": "subprocess",
        "bootstrap": {
            "resamples": 1000,
            "confidence_pct": 95.0,
            "seed": 1,
        },
        "thresholds": {
            "min_gain_pp": 10.0,
            "max_cost_ratio": 2.0,
            "max_regression_pp": 5.0,
            "max_cost_usd": 1.0,
        },
        "suite_digests": {
            "train": "00",
            "dev": "00",
            "test": "00",
        },
        "arms": {
            "A": {
                "label": "baseline",
                "solver": "deterministic",
                "model_id": "deterministic-baseline",
                "memory": false,
                "evolution": false,
                "cost_usd_per_call": 0.0,
                "budget": { "timeout_ms": 1000, "max_steps": 4, "max_cost_usd": 1.0 },
                "successes": 1,
                "total": 6,
                "success_rate_pp": 16.7,
                "cost_usd": 0.0,
                "duration_ms": 1.0,
                "latency_p50_ms": 1.0,
                "latency_p95_ms": 1.0,
                "tool_errors": 0,
                "unauthorized_actions": 0,
            },
            "B": {
                "label": "memory",
                "solver": "deterministic",
                "model_id": "deterministic-baseline",
                "memory": true,
                "evolution": false,
                "cost_usd_per_call": 0.0,
                "budget": { "timeout_ms": 1000, "max_steps": 4, "max_cost_usd": 1.0 },
                "successes": 2,
                "total": 6,
                "success_rate_pp": 33.3,
                "cost_usd": 0.0,
                "duration_ms": 1.0,
                "latency_p50_ms": 1.0,
                "latency_p95_ms": 1.0,
                "tool_errors": 0,
                "unauthorized_actions": 0,
            },
            "C": {
                "label": "policy",
                "solver": "deterministic",
                "model_id": "deterministic-baseline",
                "memory": true,
                "evolution": true,
                "cost_usd_per_call": 0.0,
                "budget": { "timeout_ms": 1000, "max_steps": 4, "max_cost_usd": 1.0 },
                "successes": 4,
                "total": 6,
                "success_rate_pp": 66.7,
                "cost_usd": 0.0,
                "duration_ms": 1.0,
                "latency_p50_ms": 1.0,
                "latency_p95_ms": 1.0,
                "tool_errors": 0,
                "unauthorized_actions": unauthorized_actions,
            },
        },
        "comparisons": {
            "b_minus_a": {
                "comparison": "B-A",
                "point_pp": (point_pp / 2.0),
                "ci_low_pp": (ci_low_pp / 2.0),
                "ci_high_pp": point_pp,
                "paired_tasks": 6,
                "resamples": 1000,
                "confidence_pct": 95.0,
                "seed": 1,
            },
            "c_minus_b": {
                "comparison": "C-B",
                "point_pp": (point_pp / 2.0),
                "ci_low_pp": (ci_low_pp / 2.0),
                "ci_high_pp": point_pp,
                "paired_tasks": 6,
                "resamples": 1000,
                "confidence_pct": 95.0,
                "seed": 1,
            },
            "c_minus_a": {
                "comparison": "C-A",
                "point_pp": point_pp,
                "ci_low_pp": ci_low_pp,
                "ci_high_pp": point_pp * 2.0,
                "paired_tasks": 6,
                "resamples": 1000,
                "confidence_pct": 95.0,
                "seed": 1,
            },
        },
        "verdict": verdict,
        "verdict_reason": "every pre-registered threshold is met",
        "unauthorized_actions": unauthorized_actions,
    })
}

fn patch_document(base_policy: &str, evidence_run: &str, risk_class: &str) -> Value {
    json!({
        "schema_version": 1,
        "candidate_id": CandidateId::new().to_string(),
        "base_policy_version": base_policy,
        "hypothesis": "Reordering the rules improves row-count accuracy.",
        "evidence_run_ids": [evidence_run],
        "expected_gain": { "metric": "success_rate", "delta_pp": 12.0 },
        "risk_class": risk_class,
        "operations": [{
            "op": "replace",
            "path": "/planner/rules",
            "value": [
                "Recompute row counts from distinct rows.",
                "Check units before reporting a count."
            ],
        }],
    })
}

/// Seeds an agent, its active base policy and one completed evidence run.
fn setup(sandbox: &Sandbox) -> (String, String, String) {
    let agent = sandbox.create_agent("local-user");
    let base = sandbox.init_policy(&agent);
    let base_id = base["id"].as_str().expect("base policy id").to_owned();
    let run = sandbox.evidence_run(&agent);
    (agent, base_id, run)
}

fn submitted_candidate(sandbox: &Sandbox, risk_class: &str) -> (String, String, Value) {
    let (agent, base, run) = setup(sandbox);
    let patch = patch_document(&base, &run, risk_class);
    let record = json_stdout(&sandbox.submit(&agent, &patch));
    let id = record["id"].as_str().expect("candidate id").to_owned();
    (agent, id, record)
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

fn audit_actions(detail: &Value) -> Vec<String> {
    detail["audit"]
        .as_array()
        .expect("audit is an array")
        .iter()
        .filter_map(|event| event["action"].as_str().map(str::to_owned))
        .collect()
}

fn as_path(path: &Path) -> &str {
    path.to_str().expect("utf-8 path")
}

#[test]
fn evolution_init_creates_active_policy() {
    let sandbox = Sandbox::new();
    let agent = sandbox.create_agent("local-user");

    let base = sandbox.init_policy(&agent);
    let base_id = base["id"].as_str().expect("version id").to_owned();
    assert_eq!(base["agent_id"], agent);
    assert_eq!(base["content"]["schema_version"], 1);
    assert_eq!(base["content"]["context"]["memory_budget_ratio"], 0.35);
    assert_eq!(base["content"]["context"]["retrieval"]["bm25_k"], 40);
    assert_eq!(base["content"]["context"]["retrieval"]["max_evidence"], 8);
    assert_eq!(
        base["content"]["planner"]["prompt_fragments"],
        json!([]),
        "the default planner is empty"
    );
    assert_eq!(
        base["content"]["tools"]["allowlist"],
        json!(["fs.read", "data.profile", "report.write"])
    );
    assert_eq!(
        base["content"]["verification"]["checkers"],
        json!(["csv-stats-oracle"])
    );
    assert_eq!(
        base["checksum"].as_str().expect("checksum").len(),
        64,
        "the version is content addressed"
    );

    // A second init keeps the active version instead of resetting it.
    let again = sandbox.init_policy(&agent);
    assert_eq!(again["id"], base_id);

    let list = json_stdout(&sandbox.run(&["evolution", "list", "--agent", &agent, "--json"]));
    assert_eq!(list["agent_id"], agent);
    assert_eq!(list["active_policy"]["id"], base_id);
    assert_eq!(list["latest_policy"]["id"], base_id);
    assert_eq!(list["candidates"], json!([]));

    // Human output stays readable.
    let human = sandbox.run(&["evolution", "list", "--agent", &agent]);
    assert_success(&human);
    assert!(stdout(&human).contains("active_policy:"));
    assert!(stdout(&human).contains("candidates: 0"));
}

#[test]
fn evolution_submit_valid_patch_is_static_checked() {
    let sandbox = Sandbox::new();
    let (agent, base, run) = setup(&sandbox);

    let patch = patch_document(&base, &run, "low");
    let record = json_stdout(&sandbox.submit(&agent, &patch));
    assert_eq!(record["state"], "static_checked");
    assert_eq!(record["risk_class"], "low");
    assert_eq!(record["base_policy_version"], base);
    assert_eq!(record["patch"]["evidence_run_ids"], json!([run]));
    assert_eq!(record["agent_id"], agent);

    let candidate_id = record["id"].as_str().expect("candidate id");

    // `--patch -` reads the same document from stdin.
    let from_stdin = patch_document(&base, &run, "low");
    let stdin_output = sandbox.run_with_stdin(
        &[
            "evolution",
            "submit",
            "--agent",
            &agent,
            "--patch",
            "-",
            "--json",
        ],
        &serde_json::to_string(&from_stdin).expect("patch json"),
    );
    let second = json_stdout(&stdin_output);
    assert_eq!(second["state"], "static_checked");

    let list = json_stdout(&sandbox.run(&["evolution", "list", "--agent", &agent, "--json"]));
    let candidates = list["candidates"].as_array().expect("candidate list");
    assert_eq!(candidates.len(), 2, "{candidates:?}");
    assert_eq!(candidates[0]["id"], candidate_id);
}

#[test]
fn evolution_forbidden_patch_exits_five() {
    let sandbox = Sandbox::new();
    let (agent, base, run) = setup(&sandbox);

    let mut patch = patch_document(&base, &run, "low");
    patch["operations"] = json!([{
        "op": "replace",
        "path": "/tools/allowlist",
        "value": ["fs.write"],
    }]);

    let output = sandbox.submit(&agent, &patch);
    assert_exit_five(&output, "patch rejected");

    let list = json_stdout(&sandbox.run(&["evolution", "list", "--agent", &agent, "--json"]));
    assert_eq!(
        list["candidates"],
        json!([]),
        "a static failure must not create a candidate"
    );
}

#[test]
fn evolution_evaluate_attaches_report() {
    let sandbox = Sandbox::new();
    let (agent, id, _record) = submitted_candidate(&sandbox, "low");

    let attachment = sandbox.evaluate(&agent, &id, "promote", 12.0, 4.0, 0);
    assert_eq!(attachment["state"], "evaluated");
    assert_eq!(attachment["suite_version"], "csv-stats-v1");
    assert_eq!(attachment["verdict"], "promote");
    assert_eq!(attachment["gain_pp"], 12.0);
    assert_eq!(attachment["ci_low_pp"], 4.0);
    assert_eq!(attachment["unauthorized_actions"], 0);
    assert_eq!(
        attachment["report_checksum"]
            .as_str()
            .expect("checksum")
            .len(),
        64
    );
    assert_eq!(attachment["report_sha256"].as_str().expect("sha").len(), 64);

    let detail = sandbox.show(&agent, &id);
    assert_eq!(detail["candidate"]["state"], "evaluated");
    assert_eq!(
        detail["evaluations"].as_array().expect("evaluations").len(),
        1
    );
    assert_eq!(
        detail["evaluations"][0]["id"], attachment["evaluation_id"],
        "the evaluation row is keyed by the sealed checksum"
    );

    // Attaching the same report twice is idempotent.
    let again = sandbox.evaluate(&agent, &id, "promote", 12.0, 4.0, 0);
    assert_eq!(again["evaluation_id"], attachment["evaluation_id"]);
    let detail = sandbox.show(&agent, &id);
    assert_eq!(
        detail["evaluations"].as_array().expect("evaluations").len(),
        1,
        "a re-attached report must not duplicate the row"
    );
}

#[test]
fn evolution_low_risk_promotes_with_audit() {
    let sandbox = Sandbox::new();
    let (agent, id, _record) = submitted_candidate(&sandbox, "low");
    sandbox.evaluate(&agent, &id, "promote", 12.0, 4.0, 0);

    let promoted =
        json_stdout(&sandbox.run(&["evolution", "promote", &id, "--agent", &agent, "--json"]));
    assert_eq!(promoted["decision"], "promote");
    assert_eq!(promoted["state"], "promoted");
    let promotion_id = promoted["promotion"]["id"]
        .as_str()
        .expect("promotion id")
        .to_owned();
    let to_version = promoted["to_version"]
        .as_str()
        .expect("to version")
        .to_owned();
    assert_ne!(promoted["from_version"], promoted["to_version"]);

    // The active pointer moved to the promoted immutable version.
    let list = json_stdout(&sandbox.run(&["evolution", "list", "--agent", &agent, "--json"]));
    assert_eq!(list["active_policy"]["id"], to_version);
    assert_eq!(list["latest_policy"]["id"], to_version);
    assert_eq!(
        list["active_policy"]["parent_version"],
        promoted["from_version"]
    );
    assert_eq!(
        list["active_policy"]["content"]["planner"]["rules"],
        json!([
            "Recompute row counts from distinct rows.",
            "Check units before reporting a count."
        ]),
        "the promoted artifact carries the patched rules"
    );
    assert_eq!(
        list["active_policy"]["content"]["tools"]["allowlist"],
        json!(["fs.read", "data.profile", "report.write"]),
        "a candidate can never touch the tool allowlist"
    );

    // The gate wrote its audit chain against the candidate.
    let detail = sandbox.show(&agent, &id);
    assert_eq!(detail["candidate"]["state"], "promoted");
    assert_eq!(detail["promotions"][0]["id"], promotion_id);
    let actions = audit_actions(&detail);
    assert!(
        actions.contains(&"promotion.rollback_drill".to_owned()),
        "missing drill audit: {actions:?}"
    );
    assert!(
        actions.contains(&"promotion.decide".to_owned()),
        "missing decision audit: {actions:?}"
    );
    assert!(
        actions.contains(&"promotion.promote".to_owned()),
        "missing promotion audit: {actions:?}"
    );

    // Promoting a promoted candidate again is a permanent error.
    let again = sandbox.run(&["evolution", "promote", &id, "--agent", &agent, "--json"]);
    assert_exit_five(&again, "approve it first");
}

#[test]
fn evolution_rollback_restores_and_blocks_second() {
    let sandbox = Sandbox::new();
    let (agent, id, _record) = submitted_candidate(&sandbox, "low");
    sandbox.evaluate(&agent, &id, "promote", 12.0, 4.0, 0);
    let promoted =
        json_stdout(&sandbox.run(&["evolution", "promote", &id, "--agent", &agent, "--json"]));
    let promotion_id = promoted["promotion"]["id"]
        .as_str()
        .expect("promotion id")
        .to_owned();
    let base_version = promoted["from_version"]
        .as_str()
        .expect("from version")
        .to_owned();
    let promoted_version = promoted["to_version"]
        .as_str()
        .expect("to version")
        .to_owned();

    let rolled_back = json_stdout(&sandbox.run(&[
        "evolution",
        "rollback",
        &promotion_id,
        "--agent",
        &agent,
        "--json",
    ]));
    assert_eq!(rolled_back["promotion_id"], promotion_id);
    assert_eq!(rolled_back["from_version"], promoted_version);
    assert_eq!(rolled_back["to_version"], base_version);

    let list = json_stdout(&sandbox.run(&["evolution", "list", "--agent", &agent, "--json"]));
    assert_eq!(list["active_policy"]["id"], base_version);

    let detail = sandbox.show(&agent, &id);
    assert_eq!(detail["candidate"]["state"], "rolled_back");
    assert!(
        !detail["promotions"][0]["rolled_back_at"].is_null(),
        "the promotion must be marked: {}",
        detail["promotions"][0]
    );
    assert!(
        audit_actions(&detail).contains(&"promotion.rollback".to_owned()),
        "missing rollback audit: {}",
        detail["audit"]
    );

    let twice = sandbox.run(&[
        "evolution",
        "rollback",
        &promotion_id,
        "--agent",
        &agent,
        "--json",
    ]);
    assert_exit_five(&twice, "already rolled back");
}

#[test]
fn evolution_high_risk_awaits_then_approves() {
    let sandbox = Sandbox::new();
    let (agent, id, _record) = submitted_candidate(&sandbox, "high");
    sandbox.evaluate(&agent, &id, "promote", 12.0, 4.0, 0);

    let pending =
        json_stdout(&sandbox.run(&["evolution", "promote", &id, "--agent", &agent, "--json"]));
    assert_eq!(pending["decision"], "await_approval");
    assert_eq!(pending["state"], "awaiting_approval");
    assert!(pending["promotion"].is_null());
    assert!(pending["to_version"].is_null());

    let list = json_stdout(&sandbox.run(&["evolution", "list", "--agent", &agent, "--json"]));
    assert_eq!(
        list["active_policy"]["id"], pending["from_version"],
        "a pending candidate must not move the pointer"
    );

    let approved = json_stdout(&sandbox.run(&[
        "evolution",
        "approve",
        &id,
        "--agent",
        &agent,
        "--as",
        "alice",
        "--json",
    ]));
    assert_eq!(approved["state"], "approved");

    let detail = sandbox.show(&agent, &id);
    let approve_row = detail["audit"]
        .as_array()
        .expect("audit")
        .iter()
        .find(|event| event["action"] == "evolution.approve")
        .expect("approval audit row");
    assert_eq!(approve_row["actor"], "alice");
    assert_eq!(approve_row["decision"], "allow");

    let promoted =
        json_stdout(&sandbox.run(&["evolution", "promote", &id, "--agent", &agent, "--json"]));
    assert_eq!(promoted["decision"], "promote");
    assert_eq!(promoted["state"], "promoted");
    assert_eq!(
        promoted["promotion"]["approved_by"], "alice",
        "the operator who approved is the one recorded on the promotion"
    );

    let list = json_stdout(&sandbox.run(&["evolution", "list", "--agent", &agent, "--json"]));
    assert_eq!(list["active_policy"]["id"], promoted["to_version"]);
}

#[test]
fn evolution_unknown_ids_exit_five() {
    let sandbox = Sandbox::new();
    let agent = sandbox.create_agent("local-user");
    sandbox.init_policy(&agent);

    for args in [
        vec![
            "evolution",
            "show",
            UNKNOWN_CANDIDATE,
            "--agent",
            &agent,
            "--json",
        ],
        vec![
            "evolution",
            "evaluate",
            UNKNOWN_CANDIDATE,
            "--agent",
            &agent,
            "--report",
            "missing.json",
            "--json",
        ],
        vec![
            "evolution",
            "approve",
            UNKNOWN_CANDIDATE,
            "--agent",
            &agent,
            "--as",
            "alice",
            "--json",
        ],
        vec![
            "evolution",
            "promote",
            UNKNOWN_CANDIDATE,
            "--agent",
            &agent,
            "--json",
        ],
        vec![
            "evolution",
            "rollback",
            UNKNOWN_PROMOTION,
            "--agent",
            &agent,
            "--json",
        ],
    ] {
        let output = sandbox.run(&args);
        assert_exit_five(&output, "not found");
    }

    let unknown_agent = sandbox.run(&["evolution", "init", "--agent", UNKNOWN_AGENT, "--json"]);
    assert_exit_five(&unknown_agent, "not found");
}

#[test]
fn evolution_usage_errors_exit_two() {
    let sandbox = Sandbox::new();

    // A candidate id that is not a UUID is a clap usage error.
    let output = sandbox.run(&["evolution", "show", "not-a-uuid"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));

    // `approve` requires the operator flag.
    let output = sandbox.run(&["evolution", "approve", UNKNOWN_CANDIDATE]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
}

#[test]
fn evolution_rejects_mismatched_report_candidate() {
    let sandbox = Sandbox::new();
    let (agent, id, _record) = submitted_candidate(&sandbox, "low");

    let (report_path, _stats) =
        sandbox.evaluation_files(&CandidateId::new().to_string(), "promote", 12.0, 4.0, 0);
    let output = sandbox.run(&[
        "evolution",
        "evaluate",
        &id,
        "--agent",
        &agent,
        "--report",
        as_path(&report_path),
        "--json",
    ]);
    assert_exit_five(&output, "names candidate");

    // The candidate was not moved.
    let detail = sandbox.show(&agent, &id);
    assert_eq!(detail["candidate"]["state"], "static_checked");
}

#[test]
fn evolution_rejects_missing_stats_companion() {
    let sandbox = Sandbox::new();
    let (agent, id, _record) = submitted_candidate(&sandbox, "low");

    let report_path = sandbox.write_json(
        &format!("report-{id}.json"),
        &report_document(&id, "promote"),
    );
    let output = sandbox.run(&[
        "evolution",
        "evaluate",
        &id,
        "--agent",
        &agent,
        "--report",
        as_path(&report_path),
        "--json",
    ]);
    assert_exit_five(&output, "statistics companion");

    // The explicit `--stats` path works.
    let stats_path = sandbox.write_json(
        &format!("explicit-stats-{id}.json"),
        &stats_document("promote", 12.0, 4.0, 0),
    );
    let output = sandbox.run(&[
        "evolution",
        "evaluate",
        &id,
        "--agent",
        &agent,
        "--report",
        as_path(&report_path),
        "--stats",
        as_path(&stats_path),
        "--json",
    ]);
    let attachment = json_stdout(&output);
    assert_eq!(attachment["state"], "evaluated");
}
