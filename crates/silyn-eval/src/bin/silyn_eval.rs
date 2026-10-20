//! `silyn-eval` CLI: runs arms, writes the report/statistics JSON and Markdown,
//! and hosts the isolated `worker` subcommand.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};
use silyn_core::SilynError;
use silyn_eval::arm::{Arms, ArmsConfig};
use silyn_eval::backend::TaskBudget;
use silyn_eval::evaluator::{DEFAULT_RUN_SEED, Evaluator, MAX_REPEATS};
use silyn_eval::generator::{SUITE_ID, split_sha256};
use silyn_eval::integrity::verify_suite_lock;
use silyn_eval::isolation::{SubprocessBackend, WorkerFault, run_worker};
use silyn_eval::manifest::SuiteManifest;
use silyn_eval::report::{EvaluationReport, ReportInput, render_markdown};
use silyn_eval::sealed::SealedTestSet;
use silyn_eval::statistics::{Bootstrap, EvaluationStatistics, StatsInput};
use silyn_eval::task::Split;
use silyn_eval::{ArmId, BaselineSolver, InProcessBackend, RunnerBackend};
use uuid::Uuid;

const DEFAULT_MANIFEST: &str = "eval/suites/csv-stats-v1/suite.toml";
const MODEL_ID: &str = "mock";
/// W6 experiment cap on agent steps per task.
const MAX_STEPS: u32 = 8;

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
    Run(RunArgs),
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
        Command::Run(args) => run(args),
        Command::Hashes(args) => hashes(args.manifest),
        Command::Worker(_) => unreachable!("worker is handled before execute"),
    }
}

fn run(args: RunArgs) -> Result<(), SilynError> {
    let RunArgs {
        suite,
        split,
        arm,
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
        out,
        stats_out,
        markdown,
        candidate_id,
    })
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
            model_reported: Some(&configs.a.model_id),
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
