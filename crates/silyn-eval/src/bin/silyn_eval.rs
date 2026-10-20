//! `silyn-eval` CLI: runs the baseline arm and writes an `EvaluationReport`.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use silyn_core::SilynError;
use silyn_eval::generator::{SUITE_ID, split_sha256};
use silyn_eval::manifest::SuiteManifest;
use silyn_eval::report::EvaluationReport;
use silyn_eval::task::Split;
use silyn_eval::{BaselineSolver, Evaluator};
use uuid::Uuid;

const DEFAULT_MANIFEST: &str = "eval/suites/csv-stats-v1/suite.toml";
const BASELINE_ID: &str = "arm-a-only";
const MODEL_ID: &str = "mock";

#[derive(Debug, Parser)]
#[command(
    name = "silyn-eval",
    version,
    about = "Silyn evaluation harness (csv-stats-v1 skeleton)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the baseline arm on train or dev and write an evaluation report.
    Run {
        /// Suite id declared by the manifest.
        #[arg(long, default_value = SUITE_ID)]
        suite: String,
        /// Split to evaluate. The test split stays sealed until W5.
        #[arg(long, value_enum, default_value_t = SplitArg::Dev)]
        split: SplitArg,
        /// Report path.
        #[arg(long)]
        out: PathBuf,
        /// Suite manifest path.
        #[arg(long)]
        manifest: Option<PathBuf>,
        /// Candidate id; defaults to a fresh UUID v7.
        #[arg(long)]
        candidate_id: Option<Uuid>,
    },
    /// Print the split digests that `suite.toml` must record.
    Hashes {
        /// Suite manifest path (only used to resolve the suite directory).
        #[arg(long)]
        manifest: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
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

fn main() -> ExitCode {
    match execute(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn execute(cli: Cli) -> Result<(), SilynError> {
    match cli.command {
        Command::Run {
            suite,
            split,
            out,
            manifest,
            candidate_id,
        } => run(suite, split.into(), &out, manifest, candidate_id),
        Command::Hashes { manifest } => hashes(manifest),
    }
}

fn run(
    suite: String,
    split: Split,
    out: &std::path::Path,
    manifest_path: Option<PathBuf>,
    candidate_id: Option<Uuid>,
) -> Result<(), SilynError> {
    if split.is_sealed() {
        return Err(SilynError::PolicyDenied(
            "the test split is sealed until W5; evaluate train or dev".to_owned(),
        ));
    }

    let path = manifest_path.unwrap_or_else(|| PathBuf::from(DEFAULT_MANIFEST));
    let manifest = SuiteManifest::load(&path)?;
    manifest.validate()?;
    if manifest.suite_id != suite {
        return Err(SilynError::Permanent(format!(
            "manifest {} declares suite `{}`, not `{suite}`",
            path.display(),
            manifest.suite_id
        )));
    }

    let evaluator = Evaluator::new(u32::from(manifest.repeats))?;
    let arms = evaluator.run_matrix(&BaselineSolver, split)?;
    let report = EvaluationReport::build(
        candidate_id.unwrap_or_else(Uuid::now_v7),
        &manifest,
        arms,
        MODEL_ID,
        Some(MODEL_ID),
        BASELINE_ID,
    );

    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|error| {
                SilynError::Permanent(format!("create {}: {error}", parent.display()))
            })?;
        }
    }
    std::fs::write(out, format!("{}\n", report.to_json_pretty()?))
        .map_err(|error| SilynError::Permanent(format!("write {}: {error}", out.display())))?;

    println!(
        "wrote {} (verdict: {}, A {}/{}, B {}/{}, C {}/{})",
        out.display(),
        verdict_label(report.verdict),
        report.arms.a.successes,
        report.arms.a.total,
        report.arms.b.successes,
        report.arms.b.total,
        report.arms.c.successes,
        report.arms.c.total,
    );
    Ok(())
}

fn hashes(manifest_path: Option<PathBuf>) -> Result<(), SilynError> {
    if let Some(path) = manifest_path {
        // Parse early so a broken manifest cannot be sealed silently.
        SuiteManifest::load(&path)?.validate()?;
    }
    println!("suite {SUITE_ID}");
    for split in Split::ALL {
        println!(
            "{} {} {}",
            split.as_str(),
            split.count(),
            split_sha256(split)?
        );
    }
    Ok(())
}

fn verdict_label(verdict: silyn_eval::Verdict) -> &'static str {
    match verdict {
        silyn_eval::Verdict::Promote => "promote",
        silyn_eval::Verdict::Reject => "reject",
        silyn_eval::Verdict::AwaitApproval => "await_approval",
        silyn_eval::Verdict::Invalid => "invalid",
    }
}
