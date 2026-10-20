//! `silyn-eval` CLI: runs arms, writes the report/statistics JSON and Markdown,
//! and hosts the isolated `worker` subcommand.
//!
//! Two solvers are available:
//! - `baseline` (default) is the deterministic CSV counter, run through isolated
//!   worker processes;
//! - `model` drives an OpenAI-compatible provider in-process with a declared
//!   A/B/C context, for the W6 experiment. It never bypasses the sealed-set
//!   check and never runs in the subprocess worker.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};
use silyn_core::SilynError;
use silyn_core::contracts::ModelProvider;
use silyn_eval::arm::{Arms, ArmsConfig};
use silyn_eval::backend::TaskBudget;
use silyn_eval::context::ArmContext;
use silyn_eval::evaluator::{ArmSolvers, DEFAULT_RUN_SEED, Evaluator, MAX_REPEATS};
use silyn_eval::generator::{SUITE_ID, split_sha256};
use silyn_eval::integrity::verify_suite_lock;
use silyn_eval::isolation::{SubprocessBackend, WorkerFault, run_worker};
use silyn_eval::manifest::SuiteManifest;
use silyn_eval::model_solver::{DEFAULT_MAX_TOKENS, MODEL_SOLVER_ID, ModelSolver};
use silyn_eval::report::{EvaluationReport, ReportInput, render_markdown};
use silyn_eval::sealed::SealedTestSet;
use silyn_eval::statistics::{Bootstrap, EvaluationStatistics, StatsInput};
use silyn_eval::task::Split;
use silyn_eval::{ArmId, BaselineSolver, InProcessBackend, RunnerBackend};
use silyn_llm::config::Price;
use silyn_llm::transport::ReqwestTransport;
use silyn_llm::{DEEPSEEK_BASE_URL, DEEPSEEK_MODEL, OpenAiCompatibleProvider, ProviderConfig};
use uuid::Uuid;

const DEFAULT_MANIFEST: &str = "eval/suites/csv-stats-v1/suite.toml";
const MODEL_ID: &str = "mock";
/// W6 experiment cap on agent steps per task.
const MAX_STEPS: u32 = 8;
/// Provider retries for a transient HTTP failure.
const MAX_RETRIES: u32 = 2;

#[derive(Debug, Parser)]
#[command(
    name = "silyn-eval",
    version,
    about = "Silyn evaluation harness (sealed csv-stats-v1 lab)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run one or all arms and write the report artifacts.
    Run(Box<RunArgs>),
    /// Print the split digests that `suite.toml` must record.
    Hashes(HashesArgs),
    #[command(hide = true)]
    /// Worker process: one TaskInput on stdin, one Submission on stdout.
    Worker(WorkerArgs),
}

#[derive(Debug, Args)]
struct RunArgs {
    /// Suite id declared by the manifest.
    #[arg(long, default_value = SUITE_ID)]
    suite: String,
    /// Split to evaluate. `test` needs `--allow-sealed` and a sealed suite.
    #[arg(long, value_enum, default_value_t = SplitArg::Dev)]
    split: SplitArg,
    /// Arm to run. A report needs all three.
    #[arg(long, value_enum, default_value_t = ArmArg::All)]
    arm: ArmArg,
    /// Solver under test: the deterministic baseline or a real model.
    #[arg(long, value_enum, default_value_t = SolverArg::Baseline)]
    solver: SolverArg,
    /// Declared A/B/C context file (JSON). Required only by `--solver model`.
    #[arg(long)]
    context: Option<PathBuf>,
    /// Provider model id. Defaults to the pinned DeepSeek model.
    #[arg(long)]
    model: Option<String>,
    /// Provider base URL. Defaults to the pinned DeepSeek endpoint.
    #[arg(long)]
    base_url: Option<String>,
    /// Environment variable that holds the provider API key.
    #[arg(long, default_value = "DEEPSEEK_API_KEY")]
    api_key_env: String,
    /// Maximum output tokens per model call.
    #[arg(long, default_value_t = DEFAULT_MAX_TOKENS)]
    max_tokens: u32,
    /// Sampling temperature for the model solver; omitted when unset.
    #[arg(long)]
    temperature: Option<f32>,
    /// Repeats per task (1-5); defaults to the manifest value.
    #[arg(long)]
    repeats: Option<u32>,
    /// Recorded seed for the run (provenance; the task stream is deterministic).
    #[arg(long, default_value_t = DEFAULT_RUN_SEED)]
    seed: u64,
    /// Schema-valid evaluation report path.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Statistics document path; defaults to `<out>` with a `.stats.json` suffix.
    #[arg(long)]
    stats_out: Option<PathBuf>,
    /// Markdown report path; requires `--out`.
    #[arg(long)]
    markdown: Option<PathBuf>,
    /// Suite manifest path.
    #[arg(long)]
    manifest: Option<PathBuf>,
    /// Candidate id; defaults to a fresh UUID v7.
    #[arg(long)]
    candidate_id: Option<Uuid>,
    /// Operator approval to run the sealed test split.
    #[arg(long)]
    allow_sealed: bool,
    /// Run tasks in this process. Debugging only; the default is isolation.
    #[arg(long)]
    in_process: bool,
    /// Wall-clock limit per task in milliseconds.
    #[arg(long, default_value_t = TaskBudget::DEFAULT_TIMEOUT_MS)]
    timeout_ms: u64,
}

#[derive(Debug, Args)]
struct HashesArgs {
    /// Suite manifest path; digests are compared against it.
    #[arg(long)]
    manifest: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct WorkerArgs {
    /// Injected fault, used by integration tests only.
    #[arg(long, hide = true)]
    fault: Option<String>,
    /// Print the worker sandbox on stderr, used by integration tests only.
    #[arg(long, hide = true)]
    diagnostics: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SplitArg {
    Train,
    Dev,
    Test,
}

impl From<SplitArg> for Split {
    fn from(value: SplitArg) -> Self {
        match value {
            SplitArg::Train => Self::Train,
            SplitArg::Dev => Self::Dev,
            SplitArg::Test => Self::Test,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ArmArg {
    A,
    B,
    C,
    All,
}

impl ArmArg {
    fn ids(self) -> Vec<ArmId> {
        match self {
            Self::A => vec![ArmId::A],
            Self::B => vec![ArmId::B],
            Self::C => vec![ArmId::C],
            Self::All => ArmId::ALL.to_vec(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SolverArg {
    /// Deterministic CSV counter (W5 reference), isolated per task.
    Baseline,
    /// OpenAI-compatible provider with a declared A/B/C context (W6).
    Model,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Command::Worker(args) = &cli.command {
        let fault = match args.fault.as_deref() {
            None => WorkerFault::None,
            Some(value) => match WorkerFault::parse(value) {
                Ok(fault) => fault,
                Err(error) => {
                    eprintln!("error: {error}");
                    return ExitCode::from(2);
                }
            },
        };
        return ExitCode::from(run_worker(fault, args.diagnostics) as u8);
    }

    match execute(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn execute(cli: Cli) -> Result<(), SilynError> {
    match cli.command {
        Command::Run(args) => run(*args),
        Command::Hashes(args) => hashes(args.manifest),
        Command::Worker(_) => unreachable!("worker is handled before execute"),
    }
}

/// Validated run request. Everything that can be rejected before the first call
/// (manifest, sealed access, repeats, budget, solver selection) is checked here.
struct Plan {
    split: Split,
    arm: ArmArg,
    out: Option<PathBuf>,
    stats_out: Option<PathBuf>,
    markdown: Option<PathBuf>,
    candidate_id: Option<Uuid>,
    manifest: SuiteManifest,
    manifest_path: PathBuf,
    evaluator: Evaluator,
    budget: TaskBudget,
    in_process: bool,
    solver: SolverArg,
    context: Option<PathBuf>,
    model: Option<String>,
    base_url: Option<String>,
    api_key_env: String,
    max_tokens: u32,
    temperature: Option<f32>,
}

fn run(args: RunArgs) -> Result<(), SilynError> {
    let plan = prepare(args)?;
    match plan.solver {
        SolverArg::Baseline => run_baseline(plan),
        SolverArg::Model => run_model(plan),
    }
}

fn prepare(args: RunArgs) -> Result<Plan, SilynError> {
    let RunArgs {
        suite,
        split,
        arm,
        solver,
        context,
        model,
        base_url,
        api_key_env,
        max_tokens,
        temperature,
        repeats,
        seed,
        out,
        stats_out,
        markdown,
        manifest,
        candidate_id,
        allow_sealed,
        in_process,
        timeout_ms,
    } = args;
    let split: Split = split.into();
    if solver == SolverArg::Model && !in_process {
        return Err(SilynError::Permanent(
            "the model solver runs in-process only; pass --in-process (the subprocess worker \
             keeps the deterministic baseline)"
                .to_owned(),
        ));
    }
    if solver == SolverArg::Baseline && context.is_some() {
        return Err(SilynError::Permanent(
            "--context is only read by --solver model".to_owned(),
        ));
    }

    let manifest_path = manifest.unwrap_or_else(|| PathBuf::from(DEFAULT_MANIFEST));
    let manifest = SuiteManifest::load(&manifest_path)?;
    manifest.validate()?;
    if manifest.suite_id != suite {
        return Err(SilynError::Permanent(format!(
            "manifest {} declares suite `{}`, not `{suite}`",
            manifest_path.display(),
            manifest.suite_id
        )));
    }

    if split.is_sealed() {
        if !allow_sealed {
            return Err(SilynError::PolicyDenied(
                "the test split is sealed; pass --allow-sealed after the suite is sealed and the \
                 operator approves the run"
                    .to_owned(),
            ));
        }
        if !manifest.sealed {
            return Err(SilynError::PolicyDenied(
                "the suite is not sealed (`sealed = false`); create a new sealed suite version"
                    .to_owned(),
            ));
        }
        // No sealed run against tampered artifacts.
        let suite_dir = manifest_path.parent().ok_or_else(|| {
            SilynError::Permanent(format!(
                "manifest {} has no suite directory",
                manifest_path.display()
            ))
        })?;
        verify_suite_lock(suite_dir)?;
    }

    let repeats = repeats.unwrap_or(u32::from(manifest.repeats));
    if !(1..=MAX_REPEATS).contains(&repeats) {
        return Err(SilynError::Permanent(format!(
            "repeats must be 1..={MAX_REPEATS}, got {repeats}"
        )));
    }
    let evaluator = Evaluator::with_seed(repeats, seed)?;
    let budget = TaskBudget::new(timeout_ms, MAX_STEPS, manifest.thresholds.max_cost_usd);
    budget.validate()?;

    Ok(Plan {
        split,
        arm,
        out,
        stats_out,
        markdown,
        candidate_id,
        manifest,
        manifest_path,
        evaluator,
        budget,
        in_process,
        solver,
        context,
        model,
        base_url,
        api_key_env,
        max_tokens,
        temperature,
    })
}

fn run_baseline(plan: Plan) -> Result<(), SilynError> {
    let Plan {
        split,
        arm,
        out,
        stats_out,
        markdown,
        candidate_id,
        manifest,
        manifest_path,
        evaluator,
        budget,
        in_process,
        ..
    } = plan;
    let configs = ArmsConfig::baseline(MODEL_ID, budget);
    configs.validate_equal_budget()?;

    let baseline = BaselineSolver;
    let backend: Box<dyn RunnerBackend + '_> = if in_process {
        Box::new(InProcessBackend::new(&baseline))
    } else {
        Box::new(SubprocessBackend::current_exe()?)
    };

    if split.is_sealed() {
        let set = SealedTestSet::load_for_manifest(&manifest)?;
        let ids = arm.ids();
        if ids.len() != ArmId::ALL.len() {
            let outcome = evaluator.run_sealed(backend.as_ref(), configs.get(ids[0]), &set);
            println!(
                "arm {}: {}/{} (sealed split, {} tasks)",
                ids[0],
                outcome.successes(),
                outcome.total(),
                set.len()
            );
            return Ok(());
        }
        let arms = Arms {
            a: evaluator.run_sealed(backend.as_ref(), &configs.a, &set),
            b: evaluator.run_sealed(backend.as_ref(), &configs.b, &set),
            c: evaluator.run_sealed(backend.as_ref(), &configs.c, &set),
        };
        return finish(RunRecord {
            manifest: &manifest,
            manifest_path: &manifest_path,
            split,
            sealed: true,
            evaluator,
            configs: &configs,
            arms: &arms,
            backend: backend.name(),
            model_reported: Some(MODEL_ID.to_owned()),
            out,
            stats_out,
            markdown,
            candidate_id,
        });
    }

    let ids = arm.ids();
    if ids.len() != ArmId::ALL.len() {
        let config = configs.get(ids[0]);
        let outcome = evaluator.run_split(backend.as_ref(), config, split)?;
        println!(
            "arm {}: {}/{} ({} split, {} tasks)",
            ids[0],
            outcome.successes(),
            outcome.total(),
            split,
            outcome.task_results().len()
        );
        if out.is_some() || stats_out.is_some() || markdown.is_some() {
            return Err(SilynError::Permanent(
                "a partial arm run writes no artifacts; use --arm all for a report".to_owned(),
            ));
        }
        return Ok(());
    }

    let arms = Arms {
        a: evaluator.run_split(backend.as_ref(), &configs.a, split)?,
        b: evaluator.run_split(backend.as_ref(), &configs.b, split)?,
        c: evaluator.run_split(backend.as_ref(), &configs.c, split)?,
    };
    finish(RunRecord {
        manifest: &manifest,
        manifest_path: &manifest_path,
        split,
        sealed: false,
        evaluator,
        configs: &configs,
        arms: &arms,
        backend: backend.name(),
        model_reported: Some(MODEL_ID.to_owned()),
        out,
        stats_out,
        markdown,
        candidate_id,
    })
}

/// Runs the real model solver: one solver per arm, one shared provider, one
/// declared context file. The CLI is the only caller that reads the API key.
fn run_model(plan: Plan) -> Result<(), SilynError> {
    let Plan {
        split,
        arm,
        out,
        stats_out,
        markdown,
        candidate_id,
        manifest,
        manifest_path,
        evaluator,
        budget,
        context,
        model,
        base_url,
        api_key_env,
        max_tokens,
        temperature,
        ..
    } = plan;

    let loaded = match &context {
        Some(path) => Some(ArmContext::load(path)?),
        None => None,
    };
    let model = model.unwrap_or_else(|| DEEPSEEK_MODEL.to_owned());
    let base_url = base_url.unwrap_or_else(|| DEEPSEEK_BASE_URL.to_owned());
    let api_key = read_api_key(&api_key_env)?;
    let price = read_price()?;
    let timeout = Duration::from_millis(budget.timeout_ms);
    let provider_config = ProviderConfig {
        base_url,
        api_key: api_key.clone(),
        model: model.clone(),
        timeout,
        max_retries: MAX_RETRIES,
        price,
    };
    let transport = ReqwestTransport::new(timeout)?;
    let provider: Arc<dyn ModelProvider> =
        Arc::new(OpenAiCompatibleProvider::new(provider_config, transport));

    // One solver per arm, identical model, budget, timeout and price. Only the
    // declared context differs.
    let mut models = Vec::with_capacity(ArmId::ALL.len());
    for id in ArmId::ALL {
        let arm_context = loaded
            .as_ref()
            .map(|loaded| loaded.for_arm(id))
            .unwrap_or_default();
        models.push(
            ModelSolver::new(Arc::clone(&provider), model.clone())?
                .with_solver_id(MODEL_SOLVER_ID)
                .with_context(arm_context)
                .with_timeout(timeout)
                .with_max_tokens(max_tokens)
                .with_temperature(temperature)
                .with_price(price)
                .with_secret(api_key.clone()),
        );
    }

    let mut configs = ArmsConfig::for_solver(MODEL_SOLVER_ID, &model, budget)
        .with_price_configured(price.is_some());
    if let Some(loaded) = &loaded {
        for id in [ArmId::B, ArmId::C] {
            configs = configs.with_context(id, loaded.record(id));
        }
    }
    // Fairness is enforced, not assumed: same solver, model, budget and price.
    configs.validate_equal_budget()?;

    let solvers = ArmSolvers {
        a: &models[0],
        b: &models[1],
        c: &models[2],
    };
    let ids = arm.ids();
    let backend = "in_process";

    if ids.len() != ArmId::ALL.len() {
        let id = ids[0];
        let solver = solvers.get(id);
        let in_process = InProcessBackend::new(solver);
        if split.is_sealed() {
            let set = SealedTestSet::load_for_manifest(&manifest)?;
            let outcome = evaluator.run_sealed(&in_process, configs.get(id), &set);
            println!(
                "arm {id}: {}/{} (sealed split, {} tasks, solver {MODEL_SOLVER_ID})",
                outcome.successes(),
                outcome.total(),
                set.len()
            );
            return Ok(());
        }
        let outcome = evaluator.run_split(&in_process, configs.get(id), split)?;
        println!(
            "arm {id}: {}/{} ({} split, {} tasks, solver {MODEL_SOLVER_ID})",
            outcome.successes(),
            outcome.total(),
            split,
            outcome.task_results().len()
        );
        if out.is_some() || stats_out.is_some() || markdown.is_some() {
            return Err(SilynError::Permanent(
                "a partial arm run writes no artifacts; use --arm all for a report".to_owned(),
            ));
        }
        return Ok(());
    }

    let arms = if split.is_sealed() {
        let set = SealedTestSet::load_for_manifest(&manifest)?;
        evaluator.run_sealed_matrix_with(&configs, &solvers, &set)?
    } else {
        evaluator.run_matrix_with(&configs, &solvers, split)?
    };
    // The evaluator's `price * attempts` model cannot know token usage; replace
    // it with the spend the solvers actually metered.
    let arms = Arms {
        a: arms.a.with_cost_usd(models[0].total_cost_usd()),
        b: arms.b.with_cost_usd(models[1].total_cost_usd()),
        c: arms.c.with_cost_usd(models[2].total_cost_usd()),
    };
    let model_reported = models[0].reported_model().or_else(|| Some(model.clone()));
    println!(
        "model {model}: {} calls, {} input / {} output tokens, measured cost ${:.6}{}",
        models[0].calls() + models[1].calls() + models[2].calls(),
        models[0].tokens().0 + models[1].tokens().0 + models[2].tokens().0,
        models[0].tokens().1 + models[1].tokens().1 + models[2].tokens().1,
        arms.a.cost_usd() + arms.b.cost_usd() + arms.c.cost_usd(),
        if price.is_some() {
            ""
        } else {
            " (no provider price table configured)"
        },
    );

    finish(RunRecord {
        manifest: &manifest,
        manifest_path: &manifest_path,
        split,
        sealed: split.is_sealed(),
        evaluator,
        configs: &configs,
        arms: &arms,
        backend,
        model_reported,
        out,
        stats_out,
        markdown,
        candidate_id,
    })
}

fn read_api_key(name: &str) -> Result<String, SilynError> {
    let key = std::env::var(name)
        .map_err(|_| SilynError::Permanent(format!("missing API key variable {name}")))?;
    if key.trim().is_empty() {
        return Err(SilynError::Permanent(format!(
            "API key variable {name} is empty"
        )));
    }
    Ok(key)
}

/// Optional price table. Both variables must be present; otherwise the run is
/// recorded as unpriced (`price_configured = false`) and spends 0.0.
fn read_price() -> Result<Option<Price>, SilynError> {
    let input = std::env::var("SILYN_LLM_INPUT_PRICE_PER_M").ok();
    let output = std::env::var("SILYN_LLM_OUTPUT_PRICE_PER_M").ok();
    match (input, output) {
        (Some(input), Some(output)) => Ok(Some(Price {
            input_per_million: input.parse().map_err(|_| {
                SilynError::Schema("SILYN_LLM_INPUT_PRICE_PER_M must be a number".to_owned())
            })?,
            output_per_million: output.parse().map_err(|_| {
                SilynError::Schema("SILYN_LLM_OUTPUT_PRICE_PER_M must be a number".to_owned())
            })?,
        })),
        _ => Ok(None),
    }
}

/// One completed matrix, ready to be written as report artifacts.
struct RunRecord<'a> {
    manifest: &'a SuiteManifest,
    manifest_path: &'a Path,
    split: Split,
    sealed: bool,
    evaluator: Evaluator,
    configs: &'a ArmsConfig,
    arms: &'a Arms,
    backend: &'a str,
    model_reported: Option<String>,
    out: Option<PathBuf>,
    stats_out: Option<PathBuf>,
    markdown: Option<PathBuf>,
    candidate_id: Option<Uuid>,
}

fn finish(record: RunRecord<'_>) -> Result<(), SilynError> {
    let RunRecord {
        manifest,
        manifest_path,
        split,
        sealed,
        evaluator,
        configs,
        arms,
        backend,
        model_reported,
        out,
        stats_out,
        markdown,
        candidate_id,
    } = record;
    if markdown.is_some() && out.is_none() {
        return Err(SilynError::Permanent(
            "--markdown requires --out so the Markdown and JSON describe the same run".to_owned(),
        ));
    }
    let stats = EvaluationStatistics::build(StatsInput {
        manifest,
        evaluator,
        split,
        sealed,
        backend,
        configs,
        arms,
        bootstrap: Bootstrap::default(),
    });
    let report = out.as_ref().map(|_| {
        EvaluationReport::build(ReportInput {
            candidate_id: candidate_id.unwrap_or_else(Uuid::now_v7),
            manifest,
            arms,
            repeats: evaluator.repeats(),
            model_id: &configs.a.model_id,
            model_reported: model_reported.as_deref(),
            baseline_id: &configs.baseline_id(),
            created_at: chrono::Utc::now(),
        })
    });

    if let Some(path) = stats_out.or_else(|| out.as_ref().map(|path| stats_path(path))) {
        write_text(&path, &format!("{}\n", stats.to_json_pretty()?))?;
        println!("wrote {}", path.display());
    }
    if let (Some(path), Some(report)) = (out.as_ref(), report.as_ref()) {
        write_text(path, &format!("{}\n", report.to_json_pretty()?))?;
        println!(
            "wrote {} (verdict: {}, detail: {}, A {}/{}, B {}/{}, C {}/{})",
            path.display(),
            report.verdict.as_str(),
            stats.verdict.as_str(),
            arms.a.successes(),
            arms.a.total(),
            arms.b.successes(),
            arms.b.total(),
            arms.c.successes(),
            arms.c.total(),
        );
    }
    if let (Some(path), Some(report)) = (markdown.as_ref(), report.as_ref()) {
        write_text(path, &render_markdown(report, &stats))?;
        println!("wrote {}", path.display());
    }

    let gain = &stats.comparisons.c_minus_a;
    println!(
        "suite {} split {split} ({}) via {}; C-A gain {} pp, CI [{}, {}] ({} resamples, seed {}); \
         report {}",
        manifest.version,
        if sealed { "sealed" } else { "open" },
        backend,
        gain.point_pp,
        gain.ci_low_pp,
        gain.ci_high_pp,
        gain.resamples,
        gain.seed,
        manifest_path.display(),
    );
    Ok(())
}

fn stats_path(out: &Path) -> PathBuf {
    out.with_extension("stats.json")
}

fn write_text(path: &Path, contents: &str) -> Result<(), SilynError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|error| {
                SilynError::Permanent(format!("create {}: {error}", parent.display()))
            })?;
        }
    }
    std::fs::write(path, contents)
        .map_err(|error| SilynError::Permanent(format!("write {}: {error}", path.display())))
}

fn hashes(manifest_path: Option<PathBuf>) -> Result<(), SilynError> {
    let path = manifest_path.unwrap_or_else(|| PathBuf::from(DEFAULT_MANIFEST));
    let manifest = SuiteManifest::load(&path)?;
    manifest.validate()?;

    println!("suite {SUITE_ID}");
    let mut stale: Vec<&str> = Vec::new();
    for split in Split::ALL {
        let digest = split_sha256(split)?;
        println!("{} {} {}", split.as_str(), split.count(), digest);
        if digest != manifest.split_entry(split).sha256 {
            stale.push(split.as_str());
        }
    }
    if stale.is_empty() {
        println!("manifest: matched ({})", path.display());
        Ok(())
    } else {
        Err(SilynError::Permanent(format!(
            "{} records stale digests for {stale:?}; create a new suite version instead of editing \
             the manifest in place",
            path.display()
        )))
    }
}
