//! Offline tests for the W6 model solver and the declared A/B/C contexts.
//!
//! No network is used: responses come from `MockProvider`, or from
//! `OpenAiCompatibleProvider` over an in-memory `MockTransport`.

mod common;

use std::process::Command;
use std::sync::Arc;

use silyn_core::contracts::{ModelProvider, ModelResponse};
use silyn_eval::arm::{ArmId, ArmsConfig};
use silyn_eval::backend::{TaskBudget, TaskFailureKind};
use silyn_eval::context::ArmContext;
use silyn_eval::evaluator::{ArmSolvers, Evaluator, Solver};
use silyn_eval::generator::OBJECTIVE;
use silyn_eval::model_solver::{MODEL_SOLVER_ID, ModelSolver, sanitize_error_text};
use silyn_eval::statistics::{Bootstrap, EvaluationStatistics, StatsInput};
use silyn_eval::{InProcessBackend, Split, Submission, TaskInput};
use silyn_llm::{
    HttpResponse, MockProvider, MockTransport, OpenAiCompatibleProvider, Price, ProviderConfig,
};

const MEMORY_NOTE: &str = "Check units before computing aggregates.";
const POLICY_NOTE: &str = "Always inspect the header before counting rows.";

fn context_json() -> String {
    format!(
        r#"{{
            "memory": ["{MEMORY_NOTE}"],
            "policy": ["{POLICY_NOTE}"],
            "provenance": {{
                "memory_source": "silyn-memory:agent-1",
                "policy_version": "00000000-0000-7000-8000-000000000001",
                "notes": "declared before the run"
            }}
        }}"#
    )
}

fn declared_context() -> ArmContext {
    ArmContext::from_json(&context_json()).expect("valid context")
}

fn solver_with(context: ArmContext) -> ModelSolver {
    let provider = Arc::new(MockProvider::new(Vec::new()));
    ModelSolver::new(provider, "deepseek-chat")
        .expect("model solver")
        .with_context(context)
}

fn task_input() -> TaskInput {
    TaskInput {
        objective: OBJECTIVE.to_owned(),
        csv: "id,mass_kg,quantity,timestamp\n1,72.4,1200,2026-03-01T09:30:00Z\n".to_owned(),
    }
}

fn response(content: impl Into<String>, model: &str) -> ModelResponse {
    ModelResponse {
        content: content.into(),
        model: model.to_owned(),
        input_tokens: 10,
        output_tokens: 20,
    }
}

fn provider_with(responses: Vec<ModelResponse>) -> Arc<MockProvider> {
    Arc::new(MockProvider::new(responses))
}

fn request_text(request: &silyn_core::contracts::ModelRequest) -> String {
    request
        .messages
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn context_prompts_differ_by_arm() {
    let context = declared_context();
    let a = solver_with(context.for_arm(ArmId::A));
    let b = solver_with(context.for_arm(ArmId::B));
    let c = solver_with(context.for_arm(ArmId::C));

    let (prompt_a, prompt_b, prompt_c) = (a.system_prompt(), b.system_prompt(), c.system_prompt());

    // A carries no context at all: not the text and not the section headings.
    assert!(!prompt_a.contains(MEMORY_NOTE));
    assert!(!prompt_a.contains(POLICY_NOTE));
    assert!(!prompt_a.to_ascii_lowercase().contains("memory"));
    assert!(!prompt_a.to_ascii_lowercase().contains("policy"));

    // B carries memory only.
    assert!(prompt_b.contains(MEMORY_NOTE));
    assert!(!prompt_b.contains(POLICY_NOTE));
    assert!(prompt_b.contains("Memory:"));
    assert!(!prompt_b.contains("Policy:"));

    // C carries both, in the untrusted reference block.
    assert!(prompt_c.contains(MEMORY_NOTE));
    assert!(prompt_c.contains(POLICY_NOTE));
    assert!(prompt_c.contains("<reference-notes>"));

    // Every arm still describes the CSV task and its output schema.
    for prompt in [&prompt_a, &prompt_b, &prompt_c] {
        assert!(prompt.contains("row_count"));
        assert!(prompt.contains("missing_total"));
        assert!(prompt.contains("duplicate_rows"));
        assert!(prompt.contains("unit"));
    }

    // Objective and CSV travel in the user prompt for every arm.
    let input = task_input();
    for solver in [&a, &b, &c] {
        let user = solver.user_prompt(&input);
        assert!(user.contains(OBJECTIVE));
        assert!(user.contains(&input.csv));
        let request = solver.request(&input);
        assert_eq!(request.messages.len(), 2);
        assert_eq!(request.model, "deepseek-chat");
    }
}

#[test]
fn solver_parses_submission() {
    let oracle = common::oracle(4, 2, 1);
    let submission = common::correct_submission(oracle);
    let content = serde_json::to_string(&submission).expect("submission json");
    // Two calls: one direct, one through the evaluator.
    let provider = provider_with(vec![
        response(content.clone(), "deepseek-chat"),
        response(content, "deepseek-chat"),
    ]);
    let solver = ModelSolver::new(provider, "deepseek-chat").expect("solver");

    let solved = solver.solve(&task_input()).expect("valid submission");
    assert_eq!(solved, submission);

    // The same solver scores as a success through the in-process backend.
    let evaluator = Evaluator::new(1).expect("evaluator");
    let configs = ArmsConfig::baseline("mock", TaskBudget::new(1_000, 8, 2.0));
    let backend = InProcessBackend::new(&solver);
    let outcome = evaluator.run_tasks(&[common::task("task-000", oracle)], &backend, &configs.a);
    assert_eq!(outcome.successes(), 1, "failures: {:?}", outcome.failures());
    assert_eq!(outcome.tool_errors(), 0);
}

#[test]
fn solver_rejects_non_json() {
    let provider = provider_with(vec![
        response("row_count=4; missing_total=2", "deepseek-chat"),
        response("{\"row_count\": 4}", "deepseek-chat"),
        response(
            "{\"row_count\":4,\"missing_total\":2,\"duplicate_rows\":1,\"unit\":\"kg\"}\n{}",
            "deepseek-chat",
        ),
    ]);
    let solver = ModelSolver::new(provider, "deepseek-chat").expect("solver");

    // Malformed content, missing keys and trailing data are all task failures.
    for _ in 0..3 {
        let error = solver.solve(&task_input()).expect_err("must not parse");
        assert!(
            error.to_string().contains("not one JSON submission"),
            "unexpected error: {error}"
        );
    }
    // A provider call was made and billed even though the answer was unusable.
    assert_eq!(solver.calls(), 3);

    // Through the evaluator it is a solver error, never a crash.
    let provider = provider_with(vec![response("not json", "deepseek-chat")]);
    let solver = ModelSolver::new(provider, "deepseek-chat").expect("solver");
    let evaluator = Evaluator::new(1).expect("evaluator");
    let configs = ArmsConfig::baseline("mock", TaskBudget::new(1_000, 8, 2.0));
    let backend = InProcessBackend::new(&solver);
    let outcome = evaluator.run_tasks(
        &[common::task("task-000", common::oracle(4, 2, 1))],
        &backend,
        &configs.a,
    );
    assert_eq!(outcome.successes(), 0);
    assert_eq!(outcome.failures()[0].kind, TaskFailureKind::SolverError);
}

#[test]
fn solver_records_model_id() {
    let provider = provider_with(vec![response(
        "{\"row_count\":4,\"missing_total\":2,\"duplicate_rows\":1,\"unit\":\"kg\"}",
        "deepseek-chat-2026-10",
    )]);
    let solver = ModelSolver::new(provider.clone(), "deepseek-chat")
        .expect("solver")
        .with_solver_id(MODEL_SOLVER_ID);
    assert_eq!(solver.model(), "deepseek-chat");
    assert_eq!(solver.solver_id(), MODEL_SOLVER_ID);
    assert_eq!(solver.reported_model(), None);

    solver.solve(&task_input()).expect("valid submission");
    assert_eq!(
        solver.reported_model().as_deref(),
        Some("deepseek-chat-2026-10")
    );

    // The provider saw the pinned model id in the request.
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].model, "deepseek-chat");
    assert_eq!(requests[0].max_tokens, Some(1024));
}

#[test]
fn no_api_key_in_errors() {
    let api_key = "sk-live0123456789abcdef0123456789";
    let transport = MockTransport::new(vec![HttpResponse {
        status: 401,
        body: format!(
            "unauthorized: invalid key {api_key}; echo {}",
            "x".repeat(1000)
        ),
    }]);
    let config = ProviderConfig {
        base_url: "https://api.example.test/v1".to_owned(),
        api_key: api_key.to_owned(),
        model: "deepseek-chat".to_owned(),
        timeout: std::time::Duration::from_secs(5),
        max_retries: 0,
        price: None,
    };
    let provider: Arc<dyn ModelProvider> =
        Arc::new(OpenAiCompatibleProvider::new(config, transport));
    let solver = ModelSolver::new(provider, "deepseek-chat")
        .expect("solver")
        .with_secret(api_key);

    let error = solver.solve(&task_input()).expect_err("provider must fail");
    let text = error.to_string();
    assert!(!text.contains(api_key), "API key leaked: {text}");
    assert!(!text.contains("sk-live"), "key shape leaked: {text}");
    assert!(text.contains("[redacted]"), "not redacted: {text}");
    assert!(
        text.chars().count() <= 300,
        "error was not truncated: {} chars",
        text.chars().count()
    );

    // The scrubber is also safe when applied directly.
    let sanitized = sanitize_error_text(&format!("key {api_key}"), &[]);
    assert!(!sanitized.contains("sk-live"));
}

#[test]
fn equal_budget_preserved() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("context.json");
    std::fs::write(&path, context_json()).expect("write context");
    let loaded = ArmContext::load(&path).expect("load context");

    let budget = TaskBudget::new(1_000, 8, 2.0);
    let mut configs = ArmsConfig::for_solver(MODEL_SOLVER_ID, "deepseek-chat", budget)
        .with_price_configured(true);
    for id in [ArmId::B, ArmId::C] {
        configs = configs.with_context(id, loaded.record(id));
    }
    configs.validate_equal_budget().expect("fair arms");

    // Contexts differ; the solver, model, budget and price do not.
    assert!(configs.a.context.is_none());
    assert!(configs.b.context.is_some());
    assert!(configs.c.context.is_some());
    assert_eq!(
        configs.b.context.as_ref().expect("B").sha256,
        loaded.for_arm(ArmId::B).sha256()
    );
    assert_eq!(
        configs.c.context.as_ref().expect("C").sha256,
        loaded.for_arm(ArmId::C).sha256()
    );
    for id in ArmId::ALL {
        let config = configs.get(id);
        assert_eq!(config.solver, MODEL_SOLVER_ID);
        assert_eq!(config.model_id, "deepseek-chat");
        assert_eq!(config.budget, budget);
        assert_eq!(config.cost_usd_per_call, 0.0);
        assert!(config.price_configured);
        assert_eq!(config.label, id.label());
    }

    // Extra budget on one arm is refused.
    let mut rigged = configs.clone();
    rigged.b.budget = TaskBudget::new(2_000, 8, 2.0);
    assert!(rigged.validate_equal_budget().is_err());

    // The statistics document records the solver, model, label and context hash.
    let evaluator = Evaluator::new(1).expect("evaluator");
    let arms = silyn_eval::Arms {
        a: common::scripted_arm(&evaluator, &configs.a, 0, 2, 0),
        b: common::scripted_arm(&evaluator, &configs.b, 1, 2, 0),
        c: common::scripted_arm(&evaluator, &configs.c, 2, 2, 0),
    };
    let stats = EvaluationStatistics::build(StatsInput {
        manifest: &common::manifest(),
        evaluator,
        split: Split::Dev,
        sealed: false,
        backend: "in_process",
        configs: &configs,
        arms: &arms,
        bootstrap: Bootstrap::default(),
    });
    assert_eq!(stats.arms.a.solver, MODEL_SOLVER_ID);
    assert_eq!(stats.arms.a.model_id, "deepseek-chat");
    assert_eq!(stats.arms.b.label, ArmId::B.label());
    assert!(stats.arms.a.context.is_none());
    let recorded = stats.arms.b.context.as_ref().expect("B context recorded");
    assert_eq!(recorded.source_sha256, loaded.source_sha256);
    assert_eq!(recorded.memory_entries, 1);
    assert_eq!(recorded.policy_entries, 0);
    assert_eq!(recorded.provenance.memory_source, "silyn-memory:agent-1");
}

#[test]
fn context_file_validation() {
    let context = declared_context();
    context.validate().expect("valid context");

    // Extra and missing fields are rejected by the closed schema.
    assert!(
        ArmContext::from_json(
            r#"{"memory": [], "policy": [], "provenance":
               {"memory_source": "s", "policy_version": "p"}, "tools": ["fs.read"]}"#
        )
        .is_err()
    );
    assert!(
        ArmContext::from_json(
            r#"{"memory": [], "provenance": {"memory_source": "s", "policy_version": "p"}}"#
        )
        .is_err()
    );

    // Bounds: 20 entries and 2000 bytes per entry.
    let mut too_many = context.clone();
    too_many.policy = vec!["note".to_owned(); 21];
    assert!(too_many.validate().is_err());

    let mut too_long = context.clone();
    too_long.memory = vec!["x".repeat(2001)];
    assert!(too_long.validate().is_err());

    let max = ArmContext {
        memory: vec!["x".repeat(2000); 20],
        policy: Vec::new(),
        provenance: context.provenance.clone(),
    };
    max.validate().expect("bounds are inclusive");

    // Control characters are rejected.
    let mut control = context.clone();
    control.policy = vec!["tab\there".to_owned()];
    assert!(control.validate().is_err());

    // A context file never carries a secret.
    let mut secret = context.clone();
    secret.provenance.notes = Some("sk-0123456789abcdef0123456789abcdef".to_owned());
    let error = secret.validate().expect_err("secret must be rejected");
    assert!(error.to_string().contains("API key"));

    // A file on disk is hashed and validated.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("context.json");
    std::fs::write(&path, context_json()).expect("write");
    let loaded = ArmContext::load(&path).expect("load");
    assert_eq!(loaded.source_sha256.len(), 64);
    assert_eq!(loaded.context, context);

    std::fs::write(&path, "{\"memory\": [], \"policy\": []}").expect("write bad");
    assert!(ArmContext::load(&path).is_err());
}

#[test]
fn per_arm_task_order() {
    let context = declared_context();
    let tasks = Split::Dev.count() as usize;
    let wrong = serde_json::to_string(&common::wrong_submission()).expect("json");

    let mut contexts = Vec::new();
    for id in ArmId::ALL {
        contexts.push(context.for_arm(id));
    }
    let providers: Vec<Arc<MockProvider>> = (0..3)
        .map(|_| {
            provider_with(
                (0..tasks)
                    .map(|_| response(wrong.clone(), "deepseek-chat"))
                    .collect(),
            )
        })
        .collect();
    let models: Vec<ModelSolver> = contexts
        .iter()
        .zip(&providers)
        .map(|(context, provider)| {
            ModelSolver::new(
                Arc::clone(provider) as Arc<dyn ModelProvider>,
                "deepseek-chat",
            )
            .expect("solver")
            .with_context(context.clone())
        })
        .collect();
    let solvers = ArmSolvers {
        a: &models[0],
        b: &models[1],
        c: &models[2],
    };
    let configs = ArmsConfig::for_solver(
        MODEL_SOLVER_ID,
        "deepseek-chat",
        TaskBudget::new(1_000, 8, 2.0),
    );
    let evaluator = Evaluator::new(1).expect("evaluator");

    let arms = evaluator
        .run_matrix_with(&configs, &solvers, Split::Dev)
        .expect("matrix");

    // Paired order: same task ids, same attempts, same user prompts per position.
    let order = |outcome: &silyn_eval::ArmOutcome| {
        outcome
            .task_results()
            .iter()
            .map(|result| result.task_id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(order(&arms.a), order(&arms.b));
    assert_eq!(order(&arms.b), order(&arms.c));
    assert_eq!(arms.a.total(), tasks as u32);
    assert_eq!(arms.b.total(), arms.a.total());
    assert_eq!(arms.c.total(), arms.a.total());

    // Each arm talked to its own provider, in the same task order.
    for provider in &providers {
        assert_eq!(provider.requests().len(), tasks);
    }
    let users: Vec<Vec<String>> = providers
        .iter()
        .map(|provider| {
            provider
                .requests()
                .iter()
                .map(|request| request.messages[1].content.clone())
                .collect()
        })
        .collect();
    assert_eq!(users[0], users[1]);
    assert_eq!(users[1], users[2]);

    // Each provider saw only its own arm's context.
    for request in providers[0].requests() {
        let text = request_text(&request);
        assert!(!text.contains(MEMORY_NOTE));
        assert!(!text.contains(POLICY_NOTE));
    }
    for request in providers[1].requests() {
        let text = request_text(&request);
        assert!(text.contains(MEMORY_NOTE));
        assert!(!text.contains(POLICY_NOTE));
    }
    for request in providers[2].requests() {
        let text = request_text(&request);
        assert!(text.contains(MEMORY_NOTE));
        assert!(text.contains(POLICY_NOTE));
    }
}

#[test]
fn provider_error_is_a_task_failure_and_cost_follows_price() {
    // A permanent provider error becomes a solver error, not a panic.
    let transport = MockTransport::new(vec![HttpResponse {
        status: 400,
        body: "bad request".to_owned(),
    }]);
    let config = ProviderConfig {
        base_url: "https://api.example.test/v1".to_owned(),
        api_key: "test-key".to_owned(),
        model: "deepseek-chat".to_owned(),
        timeout: std::time::Duration::from_secs(5),
        max_retries: 0,
        price: None,
    };
    let provider: Arc<dyn ModelProvider> =
        Arc::new(OpenAiCompatibleProvider::new(config, transport));
    let solver = ModelSolver::new(provider, "deepseek-chat").expect("solver");
    assert!(!solver.price_configured());
    let evaluator = Evaluator::new(1).expect("evaluator");
    let configs = ArmsConfig::baseline("mock", TaskBudget::new(1_000, 8, 2.0));
    let backend = InProcessBackend::new(&solver);
    let outcome = evaluator.run_tasks(
        &[common::task("task-000", common::oracle(4, 2, 1))],
        &backend,
        &configs.a,
    );
    assert_eq!(outcome.successes(), 0);
    assert_eq!(outcome.failures()[0].kind, TaskFailureKind::SolverError);
    assert_eq!(solver.total_cost_usd(), 0.0);

    // With a price table the solver meters the reported token usage.
    let price = Price {
        input_per_million: 1.0,
        output_per_million: 2.0,
    };
    let provider = provider_with(vec![ModelResponse {
        content: serde_json::to_string(&common::wrong_submission()).expect("json"),
        model: "deepseek-chat".to_owned(),
        input_tokens: 1_000_000,
        output_tokens: 500_000,
    }]);
    let solver = ModelSolver::new(provider, "deepseek-chat")
        .expect("solver")
        .with_price(Some(price));
    assert!(solver.price_configured());
    solver.solve(&task_input()).expect("valid submission");
    assert_eq!(solver.calls(), 1);
    assert!((solver.total_cost_usd() - 2.0).abs() < 1e-9);
    assert!((solver.cost_usd_per_call() - 2.0).abs() < 1e-9);
    assert_eq!(solver.tokens(), (1_000_000, 500_000));
}

#[test]
fn sealed_matrix_with_uses_per_arm_solvers() {
    // The sealed path runs offline too: the model solver never sees the oracle,
    // and every arm still gets the same tasks in the same order.
    let manifest = common::manifest();
    let set = silyn_eval::SealedTestSet::load_for_manifest(&manifest).expect("sealed set");
    let tasks = set.len();
    let content = serde_json::to_string(&common::wrong_submission()).expect("json");
    let providers: Vec<Arc<MockProvider>> = (0..3)
        .map(|_| {
            provider_with(
                (0..tasks)
                    .map(|_| response(content.clone(), "deepseek-chat"))
                    .collect(),
            )
        })
        .collect();
    let models: Vec<ModelSolver> = providers
        .iter()
        .map(|provider| {
            ModelSolver::new(
                Arc::clone(provider) as Arc<dyn ModelProvider>,
                "deepseek-chat",
            )
            .expect("solver")
        })
        .collect();
    let solvers = ArmSolvers {
        a: &models[0],
        b: &models[1],
        c: &models[2],
    };
    let configs = ArmsConfig::for_solver(
        MODEL_SOLVER_ID,
        "deepseek-chat",
        TaskBudget::new(1_000, 8, 2.0),
    );
    let evaluator = Evaluator::new(1).expect("evaluator");
    let arms = evaluator
        .run_sealed_matrix_with(&configs, &solvers, &set)
        .expect("sealed matrix");
    assert_eq!(arms.a.total(), tasks as u32);
    assert_eq!(arms.b.total(), arms.a.total());
    assert_eq!(arms.c.total(), arms.a.total());
    for provider in &providers {
        assert_eq!(provider.requests().len(), tasks);
    }
}

#[test]
fn cli_model_run_requires_in_process_and_sealed_approval() {
    let exe = common::worker_exe();
    let root = common::repo_root();
    let unset = "SILYN_EVAL_TEST_UNSET_KEY";

    let run = |args: &[&str]| {
        Command::new(&exe)
            .current_dir(&root)
            .args(args)
            .env_remove(unset)
            .output()
            .expect("run silyn-eval")
    };

    // The model solver never runs in the subprocess worker.
    let denied = run(&["run", "--solver", "model", "--model", "deepseek-chat"]);
    assert!(!denied.status.success());
    assert!(
        String::from_utf8_lossy(&denied.stderr).contains("--in-process"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&denied.stderr)
    );

    // A missing key is reported by variable name, never by value.
    let missing = run(&[
        "run",
        "--solver",
        "model",
        "--in-process",
        "--model",
        "deepseek-chat",
        "--api-key-env",
        unset,
    ]);
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains(unset),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&missing.stderr)
    );

    // The sealed split still needs the operator flag, for the model too.
    let sealed = run(&[
        "run",
        "--solver",
        "model",
        "--in-process",
        "--split",
        "test",
        "--api-key-env",
        unset,
    ]);
    assert!(!sealed.status.success());
    assert!(String::from_utf8_lossy(&sealed.stderr).contains("--allow-sealed"));

    // A baseline run does not read a context file.
    let context = root.join("eval/suites/csv-stats-v1/suite.toml");
    let baseline = run(&[
        "run",
        "--context",
        context.to_str().expect("path"),
        "--arm",
        "a",
    ]);
    assert!(!baseline.status.success());
}

#[test]
fn submission_type_is_unchanged_by_the_solver() {
    // Guard: the model solver must not add fields to the frozen submission.
    let submission = Submission {
        row_count: 1,
        missing_total: 0,
        duplicate_rows: 0,
        unit: silyn_eval::Unit::Kg,
    };
    let json = serde_json::to_string(&submission).expect("json");
    assert_eq!(
        json,
        r#"{"row_count":1,"missing_total":0,"duplicate_rows":0,"unit":"kg"}"#
    );
}
