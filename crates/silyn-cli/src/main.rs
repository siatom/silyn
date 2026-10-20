//! Silyn CLI. Kept scriptable on purpose: stable exit codes and `--json` output.
//!
//! Exit codes (see `docs/gates/w2-03.md`):
//!
//! | code | meaning |
//! |------|---------|
//! | 0 | success |
//! | 2 | usage error (argument parsing) |
//! | 3 | policy denied |
//! | 4 | transient failure, safe to retry |
//! | 5 | permanent failure or indeterminate outcome, do not retry blindly |
//!
//! This binary contains no business logic: every command delegates to
//! `silyn-app` and only formats results.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{CommandFactory, Parser, Subcommand};
use silyn_app::{AgentRecord, App, RunRecord, SessionRecord, TaskRecord};
use silyn_core::SilynError;
use silyn_core::error::ErrorClass;
use silyn_core::ids::{AgentId, RunId, SessionId};
use silyn_core::state::RunStatus;

/// Exit code for a policy denial.
const EXIT_POLICY_DENIED: u8 = 3;
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
    /// Inspect execution runs.
    Runs {
        #[command(subcommand)]
        command: RunsCommand,
    },
    /// Execute a task (requires the runtime loop, which lands in W3).
    Run,
    /// Print build, platform and configuration diagnostics.
    Doctor,
    /// Print resolved non-secret configuration.
    Config,
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
enum RunsCommand {
    /// List recent runs, newest first.
    List,
    /// Show one run by id.
    Show {
        /// Run id (UUID).
        #[arg(value_parser = parse_run_id)]
        id: RunId,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    // Local development only. CI and production read real environment variables.
    dotenvy::dotenv().ok();

    let cli = Cli::parse();
    match cli.command {
        Some(command) => match dispatch(command, cli.json).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => report(&error, cli.json),
        },
        None => print_help(),
    }
}

async fn dispatch(command: Command, json: bool) -> Result<(), SilynError> {
    match command {
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
        Command::Runs { command } => match command {
            RunsCommand::List => runs_list(json).await,
            RunsCommand::Show { id } => runs_show(id, json).await,
        },
        // The runtime loop is W3. Returning a permanent error keeps `run`
        // scriptable without inventing a run record that never executed.
        Command::Run => Err(SilynError::Permanent(
            "runtime loop is not available until W3".to_owned(),
        )),
        Command::Doctor => doctor(json),
        Command::Config => config(json),
    }
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
        ErrorClass::PolicyDenied => ExitCode::from(EXIT_POLICY_DENIED),
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

fn print_json(value: &serde_json::Value) -> Result<(), SilynError> {
    let text = serde_json::to_string_pretty(value)?;
    println!("{text}");
    Ok(())
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

fn parse_run_id(value: &str) -> Result<RunId, String> {
    value
        .parse()
        .map_err(|error| format!("invalid run id `{value}`: {error}"))
}
