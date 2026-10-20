//! Silyn CLI. Kept scriptable on purpose: stable exit codes and `--json` output.
//!
//! Exit codes (see `docs/gates/w2-03.md` and `docs/gates/w3-03.md`):
//!
//! | code | meaning |
//! |------|---------|
//! | 0 | success, including a run that completed |
//! | 2 | usage error (argument parsing) |
//! | 3 | run cancelled, or a policy denial |
//! | 4 | transient failure, safe to retry |
//! | 5 | permanent failure or indeterminate outcome, do not retry blindly |
//!
//! This binary contains no business logic: every command delegates to
//! `silyn-app` and only formats results.

use std::collections::HashSet;
use std::env;
use std::io::{Read as _, Write as _};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use silyn_app::{
    AgentRecord, App, BudgetSpec, CandidateDetail, CandidateRecord, EvaluationAttachment,
    ExperienceRecord, MemoryHitRecord, MemoryScope, PolicyInventory, PolicyPatch,
    PolicyVersionRecord, PromotionResult, Provenance, RollbackResult, RunOptions, RunOutcome,
    RunRecord, RunRequest, SessionRecord, TaskRecord, ToolCallRecord, ToolCallState,
};
use silyn_core::SilynError;
use silyn_core::contracts::PromotionDecision;
use silyn_core::error::ErrorClass;
use silyn_core::event::{RunEvent, VerifiedOutcome};
use silyn_core::ids::{AgentId, CandidateId, PromotionId, RunId, SessionId, TaskId};
use silyn_core::state::{CandidateState, LifecycleState, RunStatus};

/// Exit code for a policy denial and for a cancelled run.
const EXIT_CANCELLED: u8 = 3;
/// Exit code for a transient failure.
const EXIT_TRANSIENT: u8 = 4;
/// Exit code for a permanent failure. Also used for indeterminate outcomes so
/// they can never be mistaken for success.
const EXIT_PERMANENT: u8 = 5;

#[derive(Debug, Parser)]
#[command(
    name = "silyn",
    version,
    about = "Silyn - a self-evolving AI agent",
    disable_help_subcommand = true
)]
struct Cli {
    /// Emit machine-readable JSON.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create the data directory and database.
    Init,
    /// Manage agent identities.
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Manage conversation threads.
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Manage tasks.
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },
    /// Manage agent-scoped memories.
    Memory {
        #[command(subcommand)]
        command: MemoryCommand,
    },
    /// Inspect provisional experiences extracted from runs.
    Experience {
        #[command(subcommand)]
        command: ExperienceCommand,
    },
    /// Inspect and operate the policy evolution pipeline.
    Evolution {
        #[command(subcommand)]
        command: EvolutionCommand,
    },
    /// Inspect execution runs.
    Runs {
        #[command(subcommand)]
        command: RunsCommand,
    },
    /// Render a run or an evaluation experiment as JSON or Markdown.
    Report {
        #[command(subcommand)]
        command: ReportCommand,
    },
    /// Execute a task, or resume an existing run, through the runtime loop.
    Run(RunArgs),
    /// Serve the local HTTP API until Ctrl-C.
    Serve {
        /// Bind address; defaults to SILYN_API_ADDR or 127.0.0.1:7788.
        #[arg(long)]
        addr: Option<String>,
        /// Environment variable holding the bearer token.
        #[arg(long, default_value = "SILYN_API_TOKEN")]
        token_env: String,
    },
    /// Print build, platform and configuration diagnostics.
    Doctor,
    /// Print resolved non-secret configuration.
    Config,
}

/// Arguments for `silyn run`.
///
/// Exactly one source is required: `--task`, `--resume`, or `--session` with
/// `--objective`. The app layer validates the combination, so a missing source
/// is a permanent error (exit 5) instead of a clap usage error.
#[derive(Debug, clap::Args)]
struct RunArgs {
    /// Task id (UUID) to execute.
    #[arg(long, value_parser = parse_task_id)]
    task: Option<TaskId>,
    /// Session id (UUID) used to create the task. Requires `--objective`.
    #[arg(long, value_parser = parse_session_id, requires = "objective")]
    session: Option<SessionId>,
    /// Objective of the task created for `--session`. Requires `--session`.
    #[arg(long, value_parser = parse_objective, requires = "session")]
    objective: Option<String>,
    /// Resume a created or interrupted run.
    #[arg(long, value_parser = parse_run_id)]
    resume: Option<RunId>,
    /// Use the deterministic offline provider: no API key, no network.
    #[arg(long)]
    offline: bool,
    /// Model id recorded on the run.
    #[arg(long)]
    model: Option<String>,
    /// Maximum number of model steps.
    #[arg(long)]
    max_steps: Option<u32>,
    /// Maximum wall-clock time in seconds.
    #[arg(long)]
    max_wall_secs: Option<u64>,
    /// Maximum input tokens across the run.
    #[arg(long)]
    max_input_tokens: Option<u64>,
    /// Maximum output tokens across the run.
    #[arg(long)]
    max_output_tokens: Option<u64>,
    /// Maximum cost in USD. Requires a configured price table.
    #[arg(long)]
    max_cost_usd: Option<f64>,
    /// Capability granted to the run (repeatable). Defaults to none.
    #[arg(long = "capability", value_name = "NAME")]
    capabilities: Vec<String>,
    /// Expected JSON object. Enables expected-answer verification.
    #[arg(long, value_name = "JSON", value_parser = parse_expected_json)]
    expect_json: Option<serde_json::Value>,
}

#[derive(Debug, Subcommand)]
enum AgentCommand {
    /// Create an agent identity.
    Create {
        /// Owner label recorded on the agent. Not an authentication decision.
        #[arg(long, value_parser = parse_owner)]
        owner: String,
    },
    /// List agent identities, newest first.
    List,
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    /// Start a conversation thread for an agent.
    Create {
        /// Agent id (UUID).
        #[arg(long, value_parser = parse_agent_id)]
        agent: AgentId,
    },
}

#[derive(Debug, Subcommand)]
enum TaskCommand {
    /// Record a task objective. The runtime schedules it in W3.
    Create {
        /// Session id (UUID).
        #[arg(long, value_parser = parse_session_id)]
        session: SessionId,
        /// What the agent should accomplish.
        #[arg(long, value_parser = parse_objective)]
        objective: String,
    },
}

#[derive(Debug, Subcommand)]
enum MemoryCommand {
    /// Store one memory for an agent.
    Add {
        /// Agent id (UUID).
        #[arg(long, value_parser = parse_agent_id)]
        agent: AgentId,
        /// One-line summary used for retrieval and prompt rendering.
        #[arg(long, value_parser = parse_memory_text)]
        summary: String,
        /// Full memory text.
        #[arg(long, value_parser = parse_memory_text)]
        content: String,
        /// Retrieval scope. Defaults to `agent`.
        #[arg(long, value_enum, default_value = "agent")]
        scope: MemoryScopeArg,
        /// Where the text came from. Defaults to `user_statement`.
        #[arg(long, value_enum, default_value = "user_statement")]
        provenance: ProvenanceArg,
    },
    /// List active memories of an agent, newest first.
    List {
        /// Agent id (UUID).
        #[arg(long, value_parser = parse_agent_id)]
        agent: AgentId,
    },
    /// Full-text search over one agent's memories.
    Search {
        /// Agent id (UUID).
        #[arg(long, value_parser = parse_agent_id)]
        agent: AgentId,
        /// Query terms.
        query: String,
        /// Maximum number of hits.
        #[arg(long, default_value_t = 8)]
        limit: usize,
    },
    /// Soft-delete one memory (kept for audit, hidden from retrieval).
    Delete {
        /// Agent id (UUID).
        #[arg(long, value_parser = parse_agent_id)]
        agent: AgentId,
        /// Memory id (UUID).
        #[arg(value_parser = parse_memory_id)]
        id: silyn_core::ids::MemoryId,
    },
}

#[derive(Debug, Subcommand)]
enum ExperienceCommand {
    /// List experiences of one agent.
    List {
        /// Agent id (UUID).
        #[arg(long, value_parser = parse_agent_id)]
        agent: AgentId,
    },
}

/// `silyn evolution`: submit, evaluate, approve, promote and roll back policy
/// candidates. All state changes go through `silyn-app`.
#[derive(Debug, Subcommand)]
enum EvolutionCommand {
    /// Create and activate the default base policy of an agent.
    Init {
        /// Agent id (UUID).
        #[arg(long, value_parser = parse_agent_id)]
        agent: AgentId,
    },
    /// Show the agent's active policy and candidates.
    List {
        /// Agent id (UUID).
        #[arg(long, value_parser = parse_agent_id)]
        agent: AgentId,
    },
    /// Show one candidate with its evaluations, promotions and audit trail.
    Show {
        /// Candidate id (UUID).
        #[arg(value_parser = parse_candidate_id)]
        id: CandidateId,
        /// Agent id (UUID).
        #[arg(long, value_parser = parse_agent_id)]
        agent: AgentId,
    },
    /// Submit a `PolicyPatch` JSON document (a path, or `-` for stdin).
    Submit {
        /// Agent id (UUID).
        #[arg(long, value_parser = parse_agent_id)]
        agent: AgentId,
        /// Patch file, or `-` to read standard input.
        #[arg(long, value_name = "FILE|-")]
        patch: String,
    },
    /// Attach an evaluation report and its statistics companion.
    Evaluate {
        /// Candidate id (UUID).
        #[arg(value_parser = parse_candidate_id)]
        id: CandidateId,
        /// Agent id (UUID).
        #[arg(long, value_parser = parse_agent_id)]
        agent: AgentId,
        /// `EvaluationReport` JSON document.
        #[arg(long, value_name = "FILE")]
        report: PathBuf,
        /// Statistics companion; defaults to `<report>.stats.json`.
        #[arg(long, value_name = "FILE")]
        stats: Option<PathBuf>,
    },
    /// Local operator approval of a candidate awaiting approval.
    Approve {
        /// Candidate id (UUID).
        #[arg(value_parser = parse_candidate_id)]
        id: CandidateId,
        /// Agent id (UUID).
        #[arg(long, value_parser = parse_agent_id)]
        agent: AgentId,
        /// Operator label recorded in the audit trail.
        #[arg(long = "as", value_name = "OPERATOR", value_parser = parse_operator)]
        operator: String,
    },
    /// Run the promotion gate and promote when the decision allows it.
    Promote {
        /// Candidate id (UUID).
        #[arg(value_parser = parse_candidate_id)]
        id: CandidateId,
        /// Agent id (UUID).
        #[arg(long, value_parser = parse_agent_id)]
        agent: AgentId,
    },
    /// Restore the policy version a promotion replaced.
    Rollback {
        /// Promotion id (UUID).
        #[arg(value_parser = parse_promotion_id)]
        id: PromotionId,
        /// Agent id (UUID).
        #[arg(long, value_parser = parse_agent_id)]
        agent: AgentId,
    },
}

/// CLI-facing memory scope names (`agent`, `session`).
#[derive(Debug, Clone, Copy, ValueEnum)]
enum MemoryScopeArg {
    Agent,
    Session,
}

impl From<MemoryScopeArg> for MemoryScope {
    fn from(value: MemoryScopeArg) -> Self {
        match value {
            MemoryScopeArg::Agent => Self::Agent,
            MemoryScopeArg::Session => Self::Session,
        }
    }
}

/// CLI-facing provenance names, matching the growth wire format.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProvenanceArg {
    #[value(name = "user_statement")]
    UserStatement,
    #[value(name = "external_evidence")]
    ExternalEvidence,
    #[value(name = "model_inference")]
    ModelInference,
    #[value(name = "tool_output")]
    ToolOutput,
}

impl From<ProvenanceArg> for Provenance {
    fn from(value: ProvenanceArg) -> Self {
        match value {
            ProvenanceArg::UserStatement => Self::UserStatement,
            ProvenanceArg::ExternalEvidence => Self::ExternalEvidence,
            ProvenanceArg::ModelInference => Self::ModelInference,
            ProvenanceArg::ToolOutput => Self::ToolOutput,
        }
    }
}

#[derive(Debug, Subcommand)]
enum RunsCommand {
    /// List recent runs, newest first.
    List,
    /// Show one run by id.
    Show {
        /// Run id (UUID).
        #[arg(value_parser = parse_run_id)]
        id: RunId,
    },
    /// List the journal events of one run in sequence order.
    Events {
        /// Run id (UUID).
        #[arg(value_parser = parse_run_id)]
        id: RunId,
    },
    /// List the tool-call ledger of one run, oldest first.
    Tools {
        /// Run id (UUID).
        #[arg(value_parser = parse_run_id)]
        id: RunId,
    },
    /// Request cancellation of a run. The runtime performs the transition.
    Cancel {
        /// Run id (UUID).
        #[arg(value_parser = parse_run_id)]
        id: RunId,
    },
}

/// `silyn report`: read-only rendering of a run or an evaluation experiment.
#[derive(Debug, Subcommand)]
enum ReportCommand {
    /// Bundle one run: record, journal events, tool ledger, outcome and usage.
    Run(ReportRunArgs),
    /// Render an `EvalReport` and its statistics companion.
    Experiment(ReportExperimentArgs),
}

/// Payload encoding of a report. `--json` always forces JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ReportFormat {
    Json,
    Markdown,
}

impl ReportFormat {
    /// Wire name used in the `--out` metadata document.
    fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Markdown => "markdown",
        }
    }
}

#[derive(Debug, clap::Args)]
struct ReportRunArgs {
    /// Run id (UUID).
    #[arg(value_parser = parse_run_id)]
    id: RunId,
    /// Payload encoding. `--json` forces `json`.
    #[arg(long, value_enum, default_value = "json")]
    format: ReportFormat,
    /// Write the payload to this file instead of stdout.
    #[arg(long, value_name = "PATH")]
    out: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
struct ReportExperimentArgs {
    /// `EvalReport` JSON document.
    #[arg(value_name = "REPORT")]
    report: PathBuf,
    /// Statistics companion; defaults to `<report>.stats.json` when it exists.
    #[arg(long, value_name = "FILE")]
    stats: Option<PathBuf>,
    /// Payload encoding. `--json` forces `json`.
    #[arg(long, value_enum, default_value = "json")]
    format: ReportFormat,
    /// Write the payload to this file instead of stdout.
    #[arg(long, value_name = "PATH")]
    out: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> ExitCode {
    // Local development only. CI and production read real environment variables.
    dotenvy::dotenv().ok();

    let cli = Cli::parse();
    match cli.command {
        Some(command) => match dispatch(command, cli.json).await {
            Ok(code) => code,
            Err(error) => report(&error, cli.json),
        },
        None => print_help(),
    }
}

async fn dispatch(command: Command, json: bool) -> Result<ExitCode, SilynError> {
    let result = match command {
        Command::Init => init(json).await,
        Command::Agent { command } => match command {
            AgentCommand::Create { owner } => agent_create(&owner, json).await,
            AgentCommand::List => agent_list(json).await,
        },
        Command::Session { command } => match command {
            SessionCommand::Create { agent } => session_create(agent, json).await,
        },
        Command::Task { command } => match command {
            TaskCommand::Create { session, objective } => {
                task_create(session, &objective, json).await
            }
        },
        Command::Memory { command } => match command {
            MemoryCommand::Add {
                agent,
                summary,
                content,
                scope,
                provenance,
            } => memory_add(agent, &summary, &content, scope, provenance, json).await,
            MemoryCommand::List { agent } => memory_list(agent, json).await,
            MemoryCommand::Search {
                agent,
                query,
                limit,
            } => memory_search(agent, &query, limit, json).await,
            MemoryCommand::Delete { agent, id } => memory_delete(agent, id, json).await,
        },
        Command::Experience { command } => match command {
            ExperienceCommand::List { agent } => experience_list(agent, json).await,
        },
        // `evolution promote` can legitimately end in a rejection, so the
        // handler owns its exit code.
        Command::Evolution { command } => return evolution(command, json).await,
        Command::Runs { command } => match command {
            RunsCommand::List => runs_list(json).await,
            RunsCommand::Show { id } => runs_show(id, json).await,
            RunsCommand::Events { id } => runs_events(id, json).await,
            RunsCommand::Tools { id } => runs_tools(id, json).await,
            RunsCommand::Cancel { id } => runs_cancel(id, json).await,
        },
        Command::Report { command } => report_command(command, json).await,
        // `run` maps the run outcome to a stable exit code, so it owns the code.
        Command::Run(args) => return run(args, json).await,
        Command::Serve { addr, token_env } => return serve(addr, &token_env, json).await,
        Command::Doctor => doctor(json),
        Command::Config => config(json),
    };

    result.map(|()| ExitCode::SUCCESS)
}

fn print_help() -> ExitCode {
    let mut command = Cli::command();
    if let Err(error) = command.print_help() {
        eprintln!("failed to print help: {error}");
        return ExitCode::from(EXIT_PERMANENT);
    }
    println!();
    ExitCode::SUCCESS
}

/// Renders an error on stderr and maps its class to a stable exit code.
fn report(error: &SilynError, json: bool) -> ExitCode {
    if json {
        let payload = serde_json::json!({
            "error": {
                "class": class_name(error.class()),
                "message": error.to_string(),
            },
        });
        match serde_json::to_string_pretty(&payload) {
            Ok(text) => eprintln!("{text}"),
            Err(_) => eprintln!("error: {error}"),
        }
    } else {
        eprintln!("error: {error}");
    }

    match error.class() {
        ErrorClass::PolicyDenied => ExitCode::from(EXIT_CANCELLED),
        ErrorClass::Transient => ExitCode::from(EXIT_TRANSIENT),
        // An indeterminate side effect must never look retryable or successful.
        ErrorClass::Permanent | ErrorClass::Indeterminate => ExitCode::from(EXIT_PERMANENT),
    }
}

fn class_name(class: ErrorClass) -> &'static str {
    match class {
        ErrorClass::Transient => "transient",
        ErrorClass::Permanent => "permanent",
        ErrorClass::PolicyDenied => "policy_denied",
        ErrorClass::Indeterminate => "indeterminate",
    }
}

fn home_dir() -> PathBuf {
    env::var("SILYN_HOME").map_or_else(|_| PathBuf::from(".silyn"), PathBuf::from)
}

/// Serves the local HTTP API. The listener is bound before the JSON line is
/// printed, so a test or desktop shell can read the effective address.
async fn serve(addr: Option<String>, token_env: &str, json: bool) -> Result<ExitCode, SilynError> {
    let raw = addr
        .or_else(|| env::var("SILYN_API_ADDR").ok())
        .unwrap_or_else(|| "127.0.0.1:7788".to_owned());
    let addr: SocketAddr = raw
        .parse()
        .map_err(|error| SilynError::Permanent(format!("invalid API address `{raw}`: {error}")))?;
    let token = env::var(token_env)
        .ok()
        .filter(|value| !value.trim().is_empty());
    let app = Arc::new(App::open(home_dir()).await?);

    if json {
        println!(
            "{}",
            serde_json::json!({
                "addr": addr.to_string(),
                "token_required": token.is_some(),
            })
        );
        std::io::stdout().flush().map_err(|error| {
            SilynError::Storage(format!("cannot flush serve announcement: {error}"))
        })?;
    }

    silyn_api::serve(app, addr, token).await?;
    Ok(ExitCode::SUCCESS)
}

struct ResolvedConfig {
    provider: String,
    base_url: String,
    model: String,
    api_key_env: String,
    api_key_present: bool,
    eval_provider: String,
    home: PathBuf,
}

impl ResolvedConfig {
    fn from_env() -> Self {
        let api_key_env =
            env::var("SILYN_LLM_API_KEY_ENV").unwrap_or_else(|_| "DEEPSEEK_API_KEY".to_owned());
        let api_key_present = env::var(&api_key_env).is_ok_and(|value| !value.trim().is_empty());

        Self {
            provider: env::var("SILYN_LLM_PROVIDER")
                .unwrap_or_else(|_| "openai-compatible".to_owned()),
            base_url: env::var("SILYN_LLM_BASE_URL")
                .unwrap_or_else(|_| silyn_llm::DEEPSEEK_BASE_URL.to_owned()),
            model: env::var("SILYN_LLM_MODEL")
                .unwrap_or_else(|_| silyn_llm::DEEPSEEK_MODEL.to_owned()),
            api_key_env,
            api_key_present,
            eval_provider: env::var("SILYN_EVAL_PROVIDER").unwrap_or_else(|_| "mock".to_owned()),
            home: home_dir(),
        }
    }

    fn database(&self) -> PathBuf {
        self.home.join("silyn.db")
    }
}

fn doctor(json: bool) -> Result<(), SilynError> {
    let config = ResolvedConfig::from_env();
    let platform = format!("{}-{}", env::consts::OS, env::consts::ARCH);

    if json {
        let payload = serde_json::json!({
            "version": silyn_app::VERSION,
            "platform": platform,
            "home": config.home,
            "database": config.database(),
            "llm": {
                "provider": config.provider,
                "base_url": config.base_url,
                "model": config.model,
                "api_key_env": config.api_key_env,
                "api_key_present": config.api_key_present,
            },
            "eval": { "provider": config.eval_provider },
        });
        return print_json(&payload);
    }

    println!("{}", silyn_app::version_line());
    println!("platform: {platform}");
    println!("home: {}", config.home.display());
    println!("database: {}", config.database().display());
    println!("llm.provider: {}", config.provider);
    println!("llm.base_url: {}", config.base_url);
    println!("llm.model: {}", config.model);
    println!("llm.api_key_env: {}", config.api_key_env);
    println!(
        "llm.api_key: {}",
        if config.api_key_present {
            "present"
        } else {
            "missing"
        }
    );
    println!("eval.provider: {}", config.eval_provider);
    Ok(())
}

fn config(json: bool) -> Result<(), SilynError> {
    let config = ResolvedConfig::from_env();

    if json {
        let payload = serde_json::json!({
            "llm": {
                "provider": config.provider,
                "base_url": config.base_url,
                "model": config.model,
                "api_key_env": config.api_key_env,
                "api_key_present": config.api_key_present,
            },
            "eval": { "provider": config.eval_provider },
            "home": config.home,
        });
        return print_json(&payload);
    }

    println!("llm.provider={}", config.provider);
    println!("llm.base_url={}", config.base_url);
    println!("llm.model={}", config.model);
    println!("llm.api_key_env={}", config.api_key_env);
    println!(
        "llm.api_key={}",
        if config.api_key_present {
            "present"
        } else {
            "missing"
        }
    );
    println!("eval.provider={}", config.eval_provider);
    println!("home={}", config.home.display());
    Ok(())
}

async fn init(json: bool) -> Result<(), SilynError> {
    let app = App::open(home_dir()).await?;
    let report = app.init_report();

    if json {
        return print_json(&serde_json::json!({
            "home": report.home,
            "database": report.database,
        }));
    }

    println!("home: {}", report.home.display());
    println!("database: {}", report.database.display());
    Ok(())
}

async fn agent_create(owner: &str, json: bool) -> Result<(), SilynError> {
    let app = App::open(home_dir()).await?;
    let record = app.create_agent(owner).await?;
    print_agent(&record, json)
}

async fn agent_list(json: bool) -> Result<(), SilynError> {
    let app = App::open(home_dir()).await?;
    let agents = app.list_agents().await?;

    if json {
        return print_json(&serde_json::to_value(&agents)?);
    }

    let rows: Vec<Vec<String>> = agents
        .iter()
        .map(|agent| {
            vec![
                agent.id.to_string(),
                agent.owner_id.clone(),
                agent.profile_version.clone(),
                agent.created_at.to_rfc3339(),
            ]
        })
        .collect();
    print_table(&["ID", "OWNER", "PROFILE", "CREATED"], &rows);
    Ok(())
}

async fn session_create(agent: AgentId, json: bool) -> Result<(), SilynError> {
    let app = App::open(home_dir()).await?;
    let record = app.create_session(agent).await?;
    print_session(&record, json)
}

async fn task_create(session: SessionId, objective: &str, json: bool) -> Result<(), SilynError> {
    let app = App::open(home_dir()).await?;
    let record = app.create_task(session, objective).await?;
    print_task(&record, json)
}

async fn memory_add(
    agent: AgentId,
    summary: &str,
    content: &str,
    scope: MemoryScopeArg,
    provenance: ProvenanceArg,
    json: bool,
) -> Result<(), SilynError> {
    let app = App::open(home_dir()).await?;
    let record = app
        .add_memory(agent, summary, content, scope.into(), provenance.into())
        .await?;

    if json {
        return print_json(&serde_json::to_value(&record)?);
    }
    println!("{}", record.id);
    Ok(())
}

async fn memory_list(agent: AgentId, json: bool) -> Result<(), SilynError> {
    let app = App::open(home_dir()).await?;
    let memories = app.list_memories(agent).await?;

    if json {
        return print_json(&serde_json::to_value(&memories)?);
    }
    print_memory_rows(&memories);
    Ok(())
}

async fn memory_search(
    agent: AgentId,
    query: &str,
    limit: usize,
    json: bool,
) -> Result<(), SilynError> {
    let app = App::open(home_dir()).await?;
    let memories = app.search_memories(agent, query, limit).await?;

    if json {
        return print_json(&serde_json::to_value(&memories)?);
    }
    print_memory_rows(&memories);
    Ok(())
}

async fn memory_delete(
    agent: AgentId,
    id: silyn_core::ids::MemoryId,
    json: bool,
) -> Result<(), SilynError> {
    let app = App::open(home_dir()).await?;
    app.delete_memory(agent, id).await?;

    if json {
        return print_json(&serde_json::json!({
            "memory_id": id,
            "deleted": true,
        }));
    }
    println!("deleted: {id}");
    Ok(())
}

async fn experience_list(agent: AgentId, json: bool) -> Result<(), SilynError> {
    let app = App::open(home_dir()).await?;
    let experiences = app.list_experiences(agent).await?;

    if json {
        return print_json(&serde_json::to_value(&experiences)?);
    }

    let rows: Vec<Vec<String>> = experiences
        .iter()
        .map(|record| {
            vec![
                record.id.to_string(),
                record.lesson.clone(),
                evidence_name(record),
                format!("{:.3}", record.confidence),
                record
                    .source_run_ids
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
                record.created_at.to_rfc3339(),
            ]
        })
        .collect();
    print_table(
        &["ID", "LESSON", "EVIDENCE", "CONFIDENCE", "RUNS", "CREATED"],
        &rows,
    );
    Ok(())
}

/// Dispatches `silyn evolution`.
///
/// `promote` returns a rejection decision as exit 5 (a hard gate failure) while
/// `await_approval` is a successful command with a pending outcome.
async fn evolution(command: EvolutionCommand, json: bool) -> Result<ExitCode, SilynError> {
    match command {
        EvolutionCommand::Init { agent } => {
            let app = App::open(home_dir()).await?;
            let record = app.init_policy(agent).await?;
            print_policy_version(&record, json)?;
            Ok(ExitCode::SUCCESS)
        }
        EvolutionCommand::List { agent } => {
            let app = App::open(home_dir()).await?;
            let inventory = app.list_policies(agent).await?;
            let candidates = app.list_candidates(agent).await?;
            print_evolution_list(agent, &inventory, &candidates, json)?;
            Ok(ExitCode::SUCCESS)
        }
        EvolutionCommand::Show { id, agent } => {
            let app = App::open(home_dir()).await?;
            let detail = app.show_candidate(agent, id).await?;
            print_candidate_detail(&detail, json)?;
            Ok(ExitCode::SUCCESS)
        }
        EvolutionCommand::Submit { agent, patch } => {
            let patch = read_patch(&patch)?;
            let app = App::open(home_dir()).await?;
            let record = app.submit_candidate(agent, patch).await?;
            print_candidate_record(&record, json)?;
            Ok(ExitCode::SUCCESS)
        }
        EvolutionCommand::Evaluate {
            id,
            agent,
            report,
            stats,
        } => {
            let app = App::open(home_dir()).await?;
            let attachment = app
                .attach_evaluation(agent, id, &report, stats.as_deref())
                .await?;
            print_evaluation(&attachment, json)?;
            Ok(ExitCode::SUCCESS)
        }
        EvolutionCommand::Approve {
            id,
            agent,
            operator,
        } => {
            let app = App::open(home_dir()).await?;
            let record = app.approve_candidate(agent, id, &operator).await?;
            print_candidate_record(&record, json)?;
            Ok(ExitCode::SUCCESS)
        }
        EvolutionCommand::Promote { id, agent } => {
            let app = App::open(home_dir()).await?;
            let result = app.promote_candidate(agent, id).await?;
            print_promotion(&result, json)?;
            let code = if matches!(result.decision, PromotionDecision::Reject { .. }) {
                ExitCode::from(EXIT_PERMANENT)
            } else {
                ExitCode::SUCCESS
            };
            Ok(code)
        }
        EvolutionCommand::Rollback { id, agent } => {
            let app = App::open(home_dir()).await?;
            let result = app.rollback_promotion(agent, id).await?;
            print_rollback(&result, json)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Reads a `PolicyPatch` from a path, or stdin for `-`.
fn read_patch(source: &str) -> Result<PolicyPatch, SilynError> {
    let text = if source == "-" {
        let mut buffer = String::new();
        std::io::stdin()
            .read_to_string(&mut buffer)
            .map_err(|error| {
                SilynError::Permanent(format!("cannot read patch from stdin: {error}"))
            })?;
        buffer
    } else {
        std::fs::read_to_string(source).map_err(|error| {
            SilynError::Permanent(format!("cannot read patch file {source}: {error}"))
        })?
    };
    serde_json::from_str(&text)
        .map_err(|error| SilynError::Schema(format!("invalid policy patch: {error}")))
}

fn print_policy_version(record: &PolicyVersionRecord, json: bool) -> Result<(), SilynError> {
    if json {
        return print_json(&serde_json::to_value(record)?);
    }
    println!("policy_version: {}", record.id);
    println!("agent: {}", record.agent_id);
    println!("checksum: {}", record.checksum);
    println!("active: true");
    Ok(())
}

fn print_evolution_list(
    agent: AgentId,
    inventory: &PolicyInventory,
    candidates: &[CandidateRecord],
    json: bool,
) -> Result<(), SilynError> {
    if json {
        return print_json(&serde_json::json!({
            "agent_id": agent,
            "active_policy": inventory.active,
            "latest_policy": inventory.latest,
            "candidates": candidates,
        }));
    }

    match &inventory.active {
        Some(active) => println!("active_policy: {} ({})", active.id, active.checksum),
        None => println!("active_policy: -"),
    }
    match &inventory.latest {
        Some(latest) => println!("latest_policy: {}", latest.id),
        None => println!("latest_policy: -"),
    }
    println!("candidates: {}", candidates.len());

    let rows: Vec<Vec<String>> = candidates
        .iter()
        .map(|candidate| {
            vec![
                candidate.id.to_string(),
                candidate_state_name(candidate.state),
                risk_name(candidate.risk_class),
                candidate.created_at.to_rfc3339(),
            ]
        })
        .collect();
    print_table(&["ID", "STATE", "RISK", "CREATED"], &rows);
    Ok(())
}

fn print_candidate_record(record: &CandidateRecord, json: bool) -> Result<(), SilynError> {
    if json {
        return print_json(&serde_json::to_value(record)?);
    }
    println!("candidate: {}", record.id);
    println!("state: {}", candidate_state_name(record.state));
    println!("risk_class: {}", risk_name(record.risk_class));
    println!("base_policy_version: {}", record.base_policy_version);
    Ok(())
}

fn print_candidate_detail(detail: &CandidateDetail, json: bool) -> Result<(), SilynError> {
    if json {
        return print_json(&serde_json::to_value(detail)?);
    }

    print_candidate_record(&detail.candidate, false)?;
    for evaluation in &detail.evaluations {
        println!(
            "evaluation: {} suite={} baseline={} report={}",
            evaluation.id,
            evaluation.suite_version,
            evaluation.baseline_id,
            evaluation.report_path.as_deref().unwrap_or("-")
        );
    }
    for promotion in &detail.promotions {
        println!(
            "promotion: {} approved_by={} from={} to={} rolled_back_at={}",
            promotion.id,
            promotion.approved_by,
            promotion.from_version,
            promotion.to_version,
            timestamp(promotion.rolled_back_at)
        );
    }
    for event in &detail.audit {
        println!(
            "audit: {} {} {} {}",
            event.occurred_at.to_rfc3339(),
            event.action,
            event.actor,
            event.decision.as_deref().unwrap_or("-")
        );
    }
    Ok(())
}

fn print_evaluation(attachment: &EvaluationAttachment, json: bool) -> Result<(), SilynError> {
    if json {
        return print_json(&serde_json::to_value(attachment)?);
    }
    println!("candidate: {}", attachment.candidate_id);
    println!("state: {}", candidate_state_name(attachment.state));
    println!("evaluation: {}", attachment.evaluation_id);
    println!("report_sha256: {}", attachment.report_sha256);
    println!("stats_sha256: {}", attachment.stats_sha256);
    println!("suite_version: {}", attachment.suite_version);
    println!("verdict: {}", attachment.verdict);
    println!("gain_pp: {:.2}", attachment.gain_pp);
    println!("ci_low_pp: {:.2}", attachment.ci_low_pp);
    println!("unauthorized_actions: {}", attachment.unauthorized_actions);
    Ok(())
}

fn print_promotion(result: &PromotionResult, json: bool) -> Result<(), SilynError> {
    if json {
        return print_json(&serde_json::to_value(result)?);
    }
    println!("decision: {}", decision_name(&result.decision));
    println!("state: {}", candidate_state_name(result.state));
    println!(
        "from_version: {}",
        result
            .from_version
            .map_or_else(|| "-".to_owned(), |value| value.to_string())
    );
    println!(
        "to_version: {}",
        result
            .to_version
            .map_or_else(|| "-".to_owned(), |value| value.to_string())
    );
    println!(
        "promotion: {}",
        result
            .promotion
            .as_ref()
            .map_or_else(|| "-".to_owned(), |promotion| promotion.id.to_string())
    );
    if let PromotionDecision::Reject { reason } = &result.decision {
        println!("reason: {reason}");
    }
    Ok(())
}

fn print_rollback(result: &RollbackResult, json: bool) -> Result<(), SilynError> {
    if json {
        return print_json(&serde_json::to_value(result)?);
    }
    println!("promotion: {}", result.promotion_id);
    println!("from_version: {}", result.from_version);
    println!("to_version: {}", result.to_version);
    Ok(())
}

/// Candidate state names match the JSON wire format (snake_case).
fn candidate_state_name(state: CandidateState) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{state:?}"))
}

fn risk_name(risk: silyn_app::RiskClass) -> String {
    match risk {
        silyn_app::RiskClass::Low => "low",
        silyn_app::RiskClass::Medium => "medium",
        silyn_app::RiskClass::High => "high",
    }
    .to_owned()
}

fn decision_name(decision: &PromotionDecision) -> &'static str {
    match decision {
        PromotionDecision::Promote => "promote",
        PromotionDecision::AwaitApproval => "await_approval",
        PromotionDecision::Reject { .. } => "reject",
    }
}

fn print_memory_rows(memories: &[MemoryHitRecord]) {
    let rows: Vec<Vec<String>> = memories
        .iter()
        .map(|memory| {
            vec![
                memory.id.to_string(),
                memory.summary.clone(),
                provenance_name(memory.provenance),
                format!("{:.3}", memory.confidence),
            ]
        })
        .collect();
    print_table(&["ID", "SUMMARY", "PROVENANCE", "CONFIDENCE"], &rows);
}

fn provenance_name(provenance: Provenance) -> String {
    serde_json::to_value(provenance)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

fn evidence_name(record: &ExperienceRecord) -> String {
    serde_json::to_value(record.evidence_status)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

async fn runs_list(json: bool) -> Result<(), SilynError> {
    let app = App::open(home_dir()).await?;
    let runs = app.list_runs().await?;

    if json {
        return print_json(&serde_json::to_value(&runs)?);
    }

    let rows: Vec<Vec<String>> = runs
        .iter()
        .map(|run| {
            vec![
                run.id.to_string(),
                run.task_id.to_string(),
                status_name(run.status),
                run.model_id.clone(),
                run.created_at.to_rfc3339(),
            ]
        })
        .collect();
    print_table(&["ID", "TASK", "STATUS", "MODEL", "CREATED"], &rows);
    Ok(())
}

async fn runs_show(id: RunId, json: bool) -> Result<(), SilynError> {
    let app = App::open(home_dir()).await?;
    let run = app.get_run(id).await?;

    if json {
        return print_json(&serde_json::to_value(&run)?);
    }

    print_run(&run);
    Ok(())
}

async fn runs_events(id: RunId, json: bool) -> Result<(), SilynError> {
    let app = App::open(home_dir()).await?;
    let events = app.list_run_events(id).await?;

    if json {
        return print_json(&serde_json::to_value(&events)?);
    }

    let rows: Vec<Vec<String>> = events
        .iter()
        .map(|envelope| {
            vec![
                envelope.seq.to_string(),
                event_name(&envelope.event),
                envelope.occurred_at.to_rfc3339(),
                envelope.event_id.to_string(),
            ]
        })
        .collect();
    print_tsv(&rows);
    Ok(())
}

async fn runs_tools(id: RunId, json: bool) -> Result<(), SilynError> {
    let app = App::open(home_dir()).await?;
    let calls = app.list_tool_calls(id).await?;

    if json {
        return print_json(&serde_json::to_value(&calls)?);
    }

    let rows: Vec<Vec<String>> = calls.iter().map(tool_row).collect();
    print_tsv(&rows);
    Ok(())
}

async fn runs_cancel(id: RunId, json: bool) -> Result<(), SilynError> {
    let app = App::open(home_dir()).await?;
    let request = app.request_cancel(id).await?;

    if json {
        return print_json(&serde_json::to_value(&request)?);
    }

    println!("path: {}", request.path.display());
    println!("already_requested: {}", request.already_requested);
    Ok(())
}

/// Executes or resumes a run and maps its terminal status to an exit code.
///
/// A failed or cancelled run is a *result*, not a CLI error: the `RunOutcome`
/// is printed on stdout and the process exits 5 or 3 so scripts can branch
/// without parsing stderr.
async fn run(args: RunArgs, json: bool) -> Result<ExitCode, SilynError> {
    // Validation runs before storage is touched, so an invalid request can
    // never create a home directory or a database.
    let request = args.into_request()?;
    let app = App::open(home_dir()).await?;
    let outcome = app.run(request).await?;

    if json {
        print_json(&serde_json::to_value(&outcome)?)?;
    } else {
        print_outcome(&outcome);
    }

    Ok(match outcome.status {
        RunStatus::Completed => ExitCode::SUCCESS,
        RunStatus::Cancelled => ExitCode::from(EXIT_CANCELLED),
        _ => ExitCode::from(EXIT_PERMANENT),
    })
}

impl RunArgs {
    /// Converts parsed flags into a validated service request.
    ///
    /// Budget defaults come from [`BudgetSpec::default`]; `--offline` is carried
    /// separately because it selects the provider, not the run record.
    fn into_request(self) -> Result<RunRequest, SilynError> {
        let defaults = BudgetSpec::default();
        let options = RunOptions {
            model: self
                .model
                .map(|model| model.trim().to_owned())
                .filter(|model| !model.is_empty())
                .unwrap_or_else(default_model),
            capabilities: dedupe(self.capabilities),
            budget: BudgetSpec {
                max_steps: self.max_steps.unwrap_or(defaults.max_steps),
                max_input_tokens: self.max_input_tokens.unwrap_or(defaults.max_input_tokens),
                max_output_tokens: self.max_output_tokens.unwrap_or(defaults.max_output_tokens),
                max_cost_usd: self.max_cost_usd,
                max_wall_clock: Duration::from_secs(
                    self.max_wall_secs
                        .unwrap_or(defaults.max_wall_clock.as_secs()),
                ),
                step_timeout: defaults.step_timeout,
            },
            // The service fills the price from the provider configuration; a
            // cost cap without a configured price is a permanent error there.
            price: None,
            // Filled once the run id exists, so another process can cancel it.
            cancel_file: None,
            memories: Vec::new(),
            agent_id: None,
        };

        let request = RunRequest {
            task_id: self.task,
            session_id: self.session,
            objective: self.objective,
            resume: self.resume,
            options,
            offline: self.offline,
            expected_json: self.expect_json,
        };
        request.validate()?;
        Ok(request)
    }
}

/// Default model id: `SILYN_LLM_MODEL`, else `deepseek-chat`.
fn default_model() -> String {
    env::var("SILYN_LLM_MODEL")
        .ok()
        .filter(|model| !model.trim().is_empty())
        .unwrap_or_else(|| silyn_llm::DEEPSEEK_MODEL.to_owned())
}

/// Drops repeated capability names while keeping the caller's order.
fn dedupe(names: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    names
        .into_iter()
        .filter(|name| seen.insert(name.clone()))
        .collect()
}

/// Human-readable summary of one finished run.
fn print_outcome(outcome: &RunOutcome) {
    println!("run: {}", outcome.run_id);
    println!("status: {}", status_name(outcome.status));
    println!(
        "outcome: {}",
        outcome.outcome.map_or_else(|| "-".to_owned(), outcome_name)
    );
    println!("steps: {}", outcome.steps);
    println!("input_tokens: {}", outcome.input_tokens);
    println!("output_tokens: {}", outcome.output_tokens);
    println!("cost_usd: {:.6}", outcome.cost_usd);
    println!(
        "final_answer: {}",
        outcome.final_answer.as_deref().unwrap_or("-")
    );
    if let Some(reason) = &outcome.reason {
        println!("reason: {reason}");
    }
}

/// Dispatches `silyn report`.
async fn report_command(command: ReportCommand, json: bool) -> Result<(), SilynError> {
    match command {
        ReportCommand::Run(args) => report_run(args, json).await,
        ReportCommand::Experiment(args) => report_experiment(args, json).await,
    }
}

/// Renders one run as a self-contained bundle.
///
/// The run record, the journal and the tool ledger are read through `silyn-app`;
/// the outcome is derived from the last `run_verified` event and the failure
/// reason from the last `run_failed` event, never guessed from the status.
///
/// Memory references are intentionally absent: `RunOutcome::memory_refs` is not
/// persisted, so the app cannot expose them for a finished run. Reporting an
/// empty list would claim that no memory was injected.
async fn report_run(args: ReportRunArgs, json: bool) -> Result<(), SilynError> {
    let app = App::open(home_dir()).await?;
    let run = app.get_run(args.id).await?;
    let events = app.list_run_events(args.id).await?;
    let tools = app.list_tool_calls(args.id).await?;

    let bundle = run_report_bundle(&run, &events, &tools);
    let markdown = render_run_markdown(&bundle);
    emit_payload(&bundle, &markdown, args.format, json, args.out.as_deref())
}

/// Builds the run report document. Markdown is rendered from this value only.
fn run_report_bundle(
    run: &RunRecord,
    events: &[silyn_core::event::EventEnvelope],
    tools: &[ToolCallRecord],
) -> serde_json::Value {
    let mut event_types = Vec::with_capacity(events.len());
    let mut outcome = None;
    let mut failure = None;
    for envelope in events {
        event_types.push(event_name(&envelope.event));
        match &envelope.event {
            RunEvent::RunVerified { outcome: verified } => outcome = Some(*verified),
            RunEvent::RunFailed { reason } => failure = Some(reason.clone()),
            _ => {}
        }
    }

    let mut started = 0_usize;
    let mut succeeded = 0_usize;
    let mut failed = 0_usize;
    let mut indeterminate = 0_usize;
    for call in tools {
        match call.state {
            ToolCallState::Started => started += 1,
            ToolCallState::Succeeded => succeeded += 1,
            ToolCallState::Failed => failed += 1,
            ToolCallState::Indeterminate => indeterminate += 1,
        }
    }

    serde_json::json!({
        "schema_version": 1,
        "kind": "run_report",
        "generated_at": chrono::Utc::now(),
        "run": run,
        "status": status_name(run.status),
        "terminal": run.status.is_terminal(),
        "outcome": outcome.map(outcome_name),
        "failure": failure.map(|reason| serde_json::json!({ "reason": reason })),
        "event_count": events.len(),
        "event_types": event_types,
        "events": events,
        "tool_call_count": tools.len(),
        "tool_summary": {
            "started": started,
            "succeeded": succeeded,
            "failed": failed,
            "indeterminate": indeterminate,
        },
        "tool_calls": tools,
        "usage": {
            "input_tokens": run.input_tokens,
            "output_tokens": run.output_tokens,
            "cost_usd": run.cost_usd,
        },
    })
}

/// Renders the run report as Markdown from the JSON bundle.
///
/// Every number in the output is read back from the bundle, so the two
/// encodings cannot drift apart.
fn render_run_markdown(report: &serde_json::Value) -> String {
    let status = scalar_at(report, "/status");
    let terminal = scalar_at(report, "/terminal");
    let outcome = scalar_at(report, "/outcome");
    let failure = scalar_at(report, "/failure/reason");
    let model = scalar_at(report, "/run/model_id");
    let task = scalar_at(report, "/run/task_id");
    let policy = scalar_at(report, "/run/policy_version");
    let snapshot = scalar_at(report, "/run/memory_snapshot_version");
    let created = scalar_at(report, "/run/created_at");
    let started = scalar_at(report, "/run/started_at");
    let finished = scalar_at(report, "/run/finished_at");

    let mut text = String::new();
    let run_id = scalar_at(report, "/run/id");
    text.push_str(&format!("# Run report: {run_id}\n\n"));
    text.push_str("## Summary\n\n");
    text.push_str(&format!("- status: {status}\n"));
    text.push_str(&format!("- terminal: {terminal}\n"));
    text.push_str(&format!("- outcome: {outcome}\n"));
    text.push_str(&format!("- failure: {failure}\n"));
    text.push_str(&format!("- model: {model}\n"));
    text.push_str(&format!("- task: {task}\n"));
    text.push_str(&format!("- policy_version: {policy}\n"));
    text.push_str(&format!("- memory_snapshot_version: {snapshot}\n"));
    text.push_str(&format!("- created_at: {created}\n"));
    text.push_str(&format!("- started_at: {started}\n"));
    text.push_str(&format!("- finished_at: {finished}\n\n"));

    let input_tokens = scalar_at(report, "/usage/input_tokens");
    let output_tokens = scalar_at(report, "/usage/output_tokens");
    let cost = scalar_at(report, "/usage/cost_usd");
    text.push_str("## Usage\n\n");
    text.push_str("| metric | value |\n|---|---|\n");
    text.push_str(&format!("| input_tokens | {input_tokens} |\n"));
    text.push_str(&format!("| output_tokens | {output_tokens} |\n"));
    text.push_str(&format!("| cost_usd | {cost} |\n\n"));

    let started_calls = scalar_at(report, "/tool_summary/started");
    let succeeded_calls = scalar_at(report, "/tool_summary/succeeded");
    let failed_calls = scalar_at(report, "/tool_summary/failed");
    let indeterminate_calls = scalar_at(report, "/tool_summary/indeterminate");
    text.push_str("## Tool summary\n\n");
    text.push_str("| state | count |\n|---|---|\n");
    text.push_str(&format!("| started | {started_calls} |\n"));
    text.push_str(&format!("| succeeded | {succeeded_calls} |\n"));
    text.push_str(&format!("| failed | {failed_calls} |\n"));
    text.push_str(&format!("| indeterminate | {indeterminate_calls} |\n\n"));

    let events = array_at(report, "/events");
    let event_count = scalar_at(report, "/event_count");
    text.push_str(&format!("## Events ({event_count})\n\n"));
    text.push_str("| seq | type | occurred_at |\n|---|---|---|\n");
    for envelope in &events {
        let seq = scalar_at(envelope, "/seq");
        let kind = scalar_at(envelope, "/event/type");
        let occurred = scalar_at(envelope, "/occurred_at");
        text.push_str(&format!("| {seq} | {kind} | {occurred} |\n"));
    }
    text.push('\n');

    let calls = array_at(report, "/tool_calls");
    let call_count = scalar_at(report, "/tool_call_count");
    text.push_str(&format!("## Tool calls ({call_count})\n\n"));
    text.push_str("| step_id | tool | state | call_id |\n|---|---|---|---|\n");
    for call in &calls {
        let step = scalar_at(call, "/step_id");
        let tool = scalar_at(call, "/tool_name");
        let state = scalar_at(call, "/state");
        let call_id = scalar_at(call, "/tool_call_id");
        text.push_str(&format!("| {step} | {tool} | {state} | {call_id} |\n"));
    }
    text
}

/// Renders an `EvalReport` and its optional statistics companion.
async fn report_experiment(args: ReportExperimentArgs, json: bool) -> Result<(), SilynError> {
    let bundle = experiment_report_bundle(&args.report, args.stats.as_deref())?;
    let markdown = render_experiment_markdown(&bundle);
    emit_payload(&bundle, &markdown, args.format, json, args.out.as_deref())
}

/// Builds the experiment report document.
///
/// `--stats` is authoritative when present; without it the sibling
/// `<report>.stats.json` is used when it exists. A missing companion is not an
/// error: the bundle then carries the report's arms and no comparison, digest or
/// verdict reason, because none of those are in the report schema.
fn experiment_report_bundle(
    report_path: &Path,
    stats_path: Option<&Path>,
) -> Result<serde_json::Value, SilynError> {
    let report = read_json_document(report_path, "evaluation report")?;
    let (stats_path, stats) = match stats_path {
        Some(path) => (
            Some(path.to_path_buf()),
            Some(read_json_document(path, "statistics companion")?),
        ),
        None => {
            let companion = report_path.with_extension("stats.json");
            if companion.is_file() {
                let value = read_json_document(&companion, "statistics companion")?;
                (Some(companion), Some(value))
            } else {
                (None, None)
            }
        }
    };

    validate_report(&report)?;
    let suite_version = require_text(&report, "suite_version", "evaluation report")?;
    let model_id = require_text(&report, "model_id", "evaluation report")?;
    let candidate_id = require_text(&report, "candidate_id", "evaluation report")?;
    let baseline_id = require_text(&report, "baseline_id", "evaluation report")?;
    let verdict = require_text(&report, "verdict", "evaluation report")?;
    if let Some(stats) = stats.as_ref() {
        validate_stats(stats)?;
        let stats_suite = require_text(stats, "suite_version", "statistics companion")?;
        if stats_suite != suite_version {
            return Err(SilynError::Schema(format!(
                "statistics suite_version `{stats_suite}` does not match the report `{suite_version}`"
            )));
        }
    }
    let stats_value = stats.as_ref();

    let arms = experiment_arms(&report, stats_value);
    let arm_count = arms.len();
    let comparisons = stats_value
        .and_then(|stats| stats.get("comparisons"))
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let digests = stats_value
        .and_then(|stats| stats.get("suite_digests"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let (thresholds, thresholds_source) =
        match stats_value.and_then(|stats| stats.get("thresholds")) {
            Some(thresholds) => (thresholds.clone(), "stats"),
            None => (
                report
                    .get("thresholds")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({})),
                "report",
            ),
        };
    let split = stats_value
        .and_then(|stats| stats.get("split"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let sealed = stats_value
        .and_then(|stats| stats.get("sealed"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let seed = stats_value
        .and_then(|stats| stats.get("seed"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let backend = stats_value
        .and_then(|stats| stats.get("backend"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let stats_verdict = stats_value
        .and_then(|stats| stats.get("verdict"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let reason = stats_value
        .and_then(|stats| stats.get("verdict_reason"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let unauthorized_actions = stats_value
        .and_then(|stats| stats.get("unauthorized_actions"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let model_reported = report
        .get("model_reported")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let repeats = report
        .get("repeats")
        .cloned()
        .or_else(|| stats_value.and_then(|stats| stats.get("repeats")).cloned())
        .unwrap_or(serde_json::Value::Null);
    let created_at = report
        .get("created_at")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    Ok(serde_json::json!({
        "schema_version": 1,
        "kind": "experiment_report",
        "generated_at": chrono::Utc::now(),
        "sources": {
            "report": report_path.display().to_string(),
            "stats": stats_path.as_ref().map(|path| path.display().to_string()),
        },
        "report": report,
        "stats": stats,
        "candidate_id": candidate_id,
        "model_id": model_id,
        "model_reported": model_reported,
        "baseline_id": baseline_id,
        "created_at": created_at,
        "suite": {
            "version": suite_version,
            "split": split,
            "sealed": sealed,
            "repeats": repeats,
            "seed": seed,
            "backend": backend,
            "digests": digests,
        },
        "arm_count": arm_count,
        "arms": arms,
        "comparisons": comparisons,
        "thresholds": thresholds,
        "thresholds_source": thresholds_source,
        "verdict": verdict,
        "stats_verdict": stats_verdict,
        "reason": reason,
        "unauthorized_actions": unauthorized_actions,
    }))
}

/// Merges the report arms with the richer statistics arms, keyed by arm id.
///
/// Numbers come from the statistics companion when it carries them, otherwise
/// from the report. `success_rate_pp` is computed from the counts only when
/// neither document states it.
fn experiment_arms(
    report: &serde_json::Value,
    stats: Option<&serde_json::Value>,
) -> Vec<serde_json::Value> {
    let Some(report_arms) = report.get("arms").and_then(serde_json::Value::as_object) else {
        return Vec::new();
    };

    let mut arms = Vec::with_capacity(report_arms.len());
    for (id, report_arm) in report_arms {
        let stats_arm = stats
            .and_then(|stats| stats.get("arms"))
            .and_then(|arms| arms.get(id));
        let successes = merged_value(stats_arm, report_arm, "successes");
        let total = merged_value(stats_arm, report_arm, "total");
        let rate = stats_arm
            .and_then(|arm| arm.get("success_rate_pp"))
            .cloned()
            .or_else(|| success_rate(successes.as_u64(), total.as_u64()))
            .unwrap_or(serde_json::Value::Null);

        arms.push(serde_json::json!({
            "id": id,
            "label": optional_value(stats_arm, "label"),
            "solver": optional_value(stats_arm, "solver"),
            "model_id": optional_value(stats_arm, "model_id"),
            "memory": optional_value(stats_arm, "memory"),
            "evolution": optional_value(stats_arm, "evolution"),
            "successes": successes,
            "total": total,
            "success_rate_pp": rate,
            "cost_usd": merged_value(stats_arm, report_arm, "cost_usd"),
            "latency_p50_ms": merged_value(stats_arm, report_arm, "latency_p50_ms"),
            "latency_p95_ms": merged_value(stats_arm, report_arm, "latency_p95_ms"),
            "tool_errors": merged_value(stats_arm, report_arm, "tool_errors"),
            "unauthorized_actions": merged_value(stats_arm, report_arm, "unauthorized_actions"),
        }));
    }
    arms
}

/// One percentage-point rate rounded to one decimal, or `None` without counts.
fn success_rate(successes: Option<u64>, total: Option<u64>) -> Option<serde_json::Value> {
    match (successes, total) {
        (Some(successes), Some(total)) if total > 0 => {
            let rate = successes as f64 / total as f64 * 100.0;
            Some(serde_json::json!((rate * 10.0).round() / 10.0))
        }
        _ => None,
    }
}

/// Statistics value first, report value second, JSON null last.
fn merged_value(
    stats_arm: Option<&serde_json::Value>,
    report_arm: &serde_json::Value,
    key: &str,
) -> serde_json::Value {
    stats_arm
        .and_then(|arm| arm.get(key))
        .or_else(|| report_arm.get(key))
        .cloned()
        .unwrap_or(serde_json::Value::Null)
}

/// Statistics value or JSON null; used for fields only the companion carries.
fn optional_value(stats_arm: Option<&serde_json::Value>, key: &str) -> serde_json::Value {
    stats_arm
        .and_then(|arm| arm.get(key))
        .cloned()
        .unwrap_or(serde_json::Value::Null)
}

/// Checks the frozen report fields this command renders.
fn validate_report(report: &serde_json::Value) -> Result<(), SilynError> {
    if !report.is_object() {
        return Err(SilynError::Schema(
            "evaluation report must be a JSON object".to_owned(),
        ));
    }
    match report
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
    {
        Some(1) => {}
        Some(other) => {
            return Err(SilynError::Schema(format!(
                "evaluation report schema_version must be 1, got {other}"
            )));
        }
        None => {
            return Err(SilynError::Schema(
                "evaluation report is missing schema_version".to_owned(),
            ));
        }
    }
    for key in [
        "candidate_id",
        "suite_version",
        "model_id",
        "baseline_id",
        "verdict",
    ] {
        require_text(report, key, "evaluation report")?;
    }
    Ok(())
}

/// Checks the companion schema version; every other field is optional.
fn validate_stats(stats: &serde_json::Value) -> Result<(), SilynError> {
    if !stats.is_object() {
        return Err(SilynError::Schema(
            "statistics companion must be a JSON object".to_owned(),
        ));
    }
    match stats
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
    {
        Some(1) => Ok(()),
        Some(other) => Err(SilynError::Schema(format!(
            "statistics schema_version must be 1, got {other}"
        ))),
        None => Err(SilynError::Schema(
            "statistics companion is missing schema_version".to_owned(),
        )),
    }
}

/// Reads a required, non-empty string field.
fn require_text(
    document: &serde_json::Value,
    key: &str,
    label: &str,
) -> Result<String, SilynError> {
    match document.get(key).and_then(serde_json::Value::as_str) {
        Some(value) if !value.trim().is_empty() => Ok(value.to_owned()),
        Some(_) => Err(SilynError::Schema(format!(
            "{label} field `{key}` is empty"
        ))),
        None => Err(SilynError::Schema(format!(
            "{label} is missing field `{key}`"
        ))),
    }
}

/// Reads and parses one JSON document, reporting unreadable and invalid files
/// as permanent errors (exit 5).
fn read_json_document(path: &Path, label: &str) -> Result<serde_json::Value, SilynError> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        SilynError::Storage(format!("cannot read {label} {}: {error}", path.display()))
    })?;
    serde_json::from_str(&text)
        .map_err(|error| SilynError::Schema(format!("invalid {label} {}: {error}", path.display())))
}

/// Renders the experiment report as Markdown from the JSON bundle.
fn render_experiment_markdown(report: &serde_json::Value) -> String {
    let suite = scalar_at(report, "/suite/version");
    let verdict = scalar_at(report, "/verdict");
    let reason = scalar_at(report, "/reason");
    let model = scalar_at(report, "/model_id");
    let model_reported = scalar_at(report, "/model_reported");
    let candidate = scalar_at(report, "/candidate_id");
    let baseline = scalar_at(report, "/baseline_id");
    let split = scalar_at(report, "/suite/split");
    let sealed = scalar_at(report, "/suite/sealed");
    let repeats = scalar_at(report, "/suite/repeats");
    let seed = scalar_at(report, "/suite/seed");
    let backend = scalar_at(report, "/suite/backend");
    let unauthorized = scalar_at(report, "/unauthorized_actions");

    let mut text = String::new();
    text.push_str(&format!("# Experiment report: {suite}\n\n"));
    text.push_str("## Summary\n\n");
    text.push_str(&format!("- verdict: {verdict}\n"));
    text.push_str(&format!("- reason: {reason}\n"));
    text.push_str(&format!("- model: {model}\n"));
    text.push_str(&format!("- model_reported: {model_reported}\n"));
    text.push_str(&format!("- candidate: {candidate}\n"));
    text.push_str(&format!("- baseline: {baseline}\n"));
    text.push_str(&format!("- split: {split}\n"));
    text.push_str(&format!("- sealed: {sealed}\n"));
    text.push_str(&format!("- repeats: {repeats}\n"));
    text.push_str(&format!("- seed: {seed}\n"));
    text.push_str(&format!("- backend: {backend}\n"));
    text.push_str(&format!("- unauthorized_actions: {unauthorized}\n\n"));

    text.push_str("## Suite digests\n\n");
    text.push_str("| split | sha256 |\n|---|---|\n");
    for name in ["train", "dev", "test"] {
        let digest = scalar_at(report, &format!("/suite/digests/{name}"));
        text.push_str(&format!("| {name} | {digest} |\n"));
    }
    text.push('\n');

    let arms = array_at(report, "/arms");
    let arm_count = scalar_at(report, "/arm_count");
    text.push_str(&format!("## Arms ({arm_count})\n\n"));
    text.push_str(
        "| arm | label | successes | total | success_rate_pp | cost_usd | latency_p95_ms |\n\
         |---|---|---|---|---|---|---|\n",
    );
    for arm in &arms {
        let id = scalar_at(arm, "/id");
        let label = scalar_at(arm, "/label");
        let successes = scalar_at(arm, "/successes");
        let total = scalar_at(arm, "/total");
        let rate = scalar_at(arm, "/success_rate_pp");
        let cost = scalar_at(arm, "/cost_usd");
        let p95 = scalar_at(arm, "/latency_p95_ms");
        text.push_str(&format!(
            "| {id} | {label} | {successes} | {total} | {rate} | {cost} | {p95} |\n"
        ));
    }
    text.push('\n');

    text.push_str("## Comparisons\n\n");
    text.push_str(
        "| comparison | point_pp | ci_low_pp | ci_high_pp | paired_tasks |\n\
                   |---|---|---|---|---|\n",
    );
    for comparison in object_at(report, "/comparisons").values() {
        let name = scalar_at(comparison, "/comparison");
        let point = scalar_at(comparison, "/point_pp");
        let low = scalar_at(comparison, "/ci_low_pp");
        let high = scalar_at(comparison, "/ci_high_pp");
        let paired = scalar_at(comparison, "/paired_tasks");
        text.push_str(&format!(
            "| {name} | {point} | {low} | {high} | {paired} |\n"
        ));
    }
    text.push('\n');

    let source = scalar_at(report, "/thresholds_source");
    text.push_str(&format!("## Thresholds ({source})\n\n"));
    text.push_str("| threshold | value |\n|---|---|\n");
    for (name, value) in &object_at(report, "/thresholds") {
        let rendered = match value {
            serde_json::Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        text.push_str(&format!("| {name} | {rendered} |\n"));
    }
    text
}

/// Writes or prints one report payload.
///
/// `--json` forces the JSON encoding. With `--out` and `--json`, stdout carries
/// a small metadata document (`path`, `format`, `bytes`) so the caller still gets
/// exactly one JSON document; the payload itself is in the file.
fn emit_payload(
    payload: &serde_json::Value,
    markdown: &str,
    format: ReportFormat,
    json: bool,
    out: Option<&Path>,
) -> Result<(), SilynError> {
    let format = if json { ReportFormat::Json } else { format };
    let body = match format {
        ReportFormat::Json => {
            let mut text = serde_json::to_string_pretty(payload)?;
            text.push('\n');
            text
        }
        ReportFormat::Markdown => {
            let mut text = markdown.to_owned();
            if !text.ends_with('\n') {
                text.push('\n');
            }
            text
        }
    };

    match out {
        Some(path) => {
            std::fs::write(path, &body).map_err(|error| {
                SilynError::Storage(format!(
                    "cannot write report to {}: {error}",
                    path.display()
                ))
            })?;
            if json {
                return print_json(&serde_json::json!({
                    "path": path.display().to_string(),
                    "format": format.as_str(),
                    "bytes": body.len(),
                }));
            }
            println!("wrote {}", path.display());
            Ok(())
        }
        None => {
            print!("{body}");
            Ok(())
        }
    }
}

/// Renders a JSON scalar as text; missing and null values become `-`.
fn scalar_at(value: &serde_json::Value, pointer: &str) -> String {
    match value.pointer(pointer) {
        None | Some(serde_json::Value::Null) => "-".to_owned(),
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

/// Clones a JSON array; a missing pointer yields an empty vector.
fn array_at(value: &serde_json::Value, pointer: &str) -> Vec<serde_json::Value> {
    value
        .pointer(pointer)
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Clones a JSON object; a missing pointer yields an empty map.
fn object_at(
    value: &serde_json::Value,
    pointer: &str,
) -> serde_json::Map<String, serde_json::Value> {
    value
        .pointer(pointer)
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default()
}

fn print_agent(record: &AgentRecord, json: bool) -> Result<(), SilynError> {
    if json {
        return print_json(&serde_json::to_value(record)?);
    }
    println!("{}", record.id);
    Ok(())
}

fn print_session(record: &SessionRecord, json: bool) -> Result<(), SilynError> {
    if json {
        return print_json(&serde_json::to_value(record)?);
    }
    println!("{}", record.id);
    Ok(())
}

fn print_task(record: &TaskRecord, json: bool) -> Result<(), SilynError> {
    if json {
        return print_json(&serde_json::to_value(record)?);
    }
    println!("{}", record.id);
    Ok(())
}

fn print_run(run: &RunRecord) {
    println!("id: {}", run.id);
    println!("task: {}", run.task_id);
    println!("status: {}", status_name(run.status));
    println!("model: {}", run.model_id);
    println!("policy_version: {}", run.policy_version);
    println!(
        "memory_snapshot_version: {}",
        run.memory_snapshot_version.as_deref().unwrap_or("-")
    );
    println!("input_tokens: {}", run.input_tokens);
    println!("output_tokens: {}", run.output_tokens);
    println!("cost_usd: {:.6}", run.cost_usd);
    println!("started_at: {}", timestamp(run.started_at));
    println!("finished_at: {}", timestamp(run.finished_at));
    println!("created_at: {}", run.created_at.to_rfc3339());
}

fn timestamp(value: Option<chrono::DateTime<chrono::Utc>>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| value.to_rfc3339())
}

/// Status names match the JSON wire format (snake_case).
fn status_name(status: RunStatus) -> String {
    match serde_json::to_value(status) {
        Ok(serde_json::Value::String(name)) => name,
        _ => "unknown".to_owned(),
    }
}

/// Verified-outcome names match the JSON wire format (snake_case).
fn outcome_name(outcome: VerifiedOutcome) -> String {
    match serde_json::to_value(outcome) {
        Ok(serde_json::Value::String(name)) => name,
        _ => "unknown".to_owned(),
    }
}

/// Event names match the JSON wire format (snake_case `type` tag).
fn event_name(event: &RunEvent) -> String {
    match serde_json::to_value(event) {
        Ok(serde_json::Value::Object(fields)) => fields
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_owned(),
        _ => "unknown".to_owned(),
    }
}

/// Ledger state names match the JSON wire format (snake_case).
fn tool_state_name(state: ToolCallState) -> String {
    match serde_json::to_value(state) {
        Ok(serde_json::Value::String(name)) => name,
        _ => "unknown".to_owned(),
    }
}

/// One human-readable ledger row: step, tool, state, call id, result reference.
fn tool_row(call: &ToolCallRecord) -> Vec<String> {
    vec![
        call.step_id.to_string(),
        call.tool_name.clone(),
        tool_state_name(call.state),
        call.tool_call_id.clone(),
        call.result_ref.clone().unwrap_or_else(|| "-".to_owned()),
    ]
}

fn print_json(value: &serde_json::Value) -> Result<(), SilynError> {
    let text = serde_json::to_string_pretty(value)?;
    println!("{text}");
    Ok(())
}

/// Prints tab-separated rows without padding, so the output can be piped
/// through `cut`/`awk`. An empty slice prints nothing.
fn print_tsv(rows: &[Vec<String>]) {
    for row in rows {
        println!("{}", row.join("\t"));
    }
}

fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = headers
        .iter()
        .map(|header| header.chars().count())
        .collect();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            if let Some(width) = widths.get_mut(index) {
                *width = (*width).max(cell.chars().count());
            }
        }
    }

    let header_cells: Vec<String> = headers
        .iter()
        .map(std::string::ToString::to_string)
        .collect();
    print_table_row(&header_cells, &widths);
    for row in rows {
        print_table_row(row, &widths);
    }
}

fn print_table_row(cells: &[String], widths: &[usize]) {
    let line = cells
        .iter()
        .zip(widths)
        .map(|(cell, width)| format!("{cell:<width$}", width = *width))
        .collect::<Vec<_>>()
        .join("  ");
    println!("{}", line.trim_end());
}

fn parse_owner(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("owner must not be empty".to_owned());
    }
    Ok(trimmed.to_owned())
}

fn parse_objective(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("objective must not be empty".to_owned());
    }
    Ok(trimmed.to_owned())
}

fn parse_agent_id(value: &str) -> Result<AgentId, String> {
    value
        .parse()
        .map_err(|error| format!("invalid agent id `{value}`: {error}"))
}

fn parse_session_id(value: &str) -> Result<SessionId, String> {
    value
        .parse()
        .map_err(|error| format!("invalid session id `{value}`: {error}"))
}

fn parse_task_id(value: &str) -> Result<TaskId, String> {
    value
        .parse()
        .map_err(|error| format!("invalid task id `{value}`: {error}"))
}

fn parse_run_id(value: &str) -> Result<RunId, String> {
    value
        .parse()
        .map_err(|error| format!("invalid run id `{value}`: {error}"))
}

fn parse_memory_id(value: &str) -> Result<silyn_core::ids::MemoryId, String> {
    value
        .parse()
        .map_err(|error| format!("invalid memory id `{value}`: {error}"))
}

fn parse_candidate_id(value: &str) -> Result<CandidateId, String> {
    value
        .parse()
        .map_err(|error| format!("invalid candidate id `{value}`: {error}"))
}

fn parse_promotion_id(value: &str) -> Result<PromotionId, String> {
    value
        .parse()
        .map_err(|error| format!("invalid promotion id `{value}`: {error}"))
}

fn parse_operator(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("operator must not be empty".to_owned());
    }
    Ok(trimmed.to_owned())
}

fn parse_memory_text(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("memory text must not be empty".to_owned());
    }
    Ok(trimmed.to_owned())
}

fn parse_expected_json(value: &str) -> Result<serde_json::Value, String> {
    let parsed: serde_json::Value = serde_json::from_str(value)
        .map_err(|error| format!("invalid --expect-json value: {error}"))?;
    if !parsed.is_object() {
        return Err("--expect-json must be a JSON object".to_owned());
    }
    Ok(parsed)
}
