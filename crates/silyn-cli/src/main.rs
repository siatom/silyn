//! Silyn CLI. Kept scriptable on purpose: stable exit codes and `--json` output.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{CommandFactory, Parser, Subcommand};

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
    /// Print build, platform and configuration diagnostics.
    Doctor,
    /// Print resolved non-secret configuration.
    Config,
}

fn main() -> ExitCode {
    // Local development only. CI and production read real environment variables.
    dotenvy::dotenv().ok();

    let cli = Cli::parse();
    match cli.command {
        Some(Command::Doctor) => run_doctor(cli.json),
        Some(Command::Config) => run_config(cli.json),
        None => {
            let mut command = Cli::command();
            if let Err(error) = command.print_help() {
                eprintln!("failed to print help: {error}");
                return ExitCode::FAILURE;
            }
            println!();
            ExitCode::SUCCESS
        }
    }
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
        let home = env::var("SILYN_HOME").map_or_else(|_| PathBuf::from(".silyn"), PathBuf::from);

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
            home,
        }
    }

    fn database(&self) -> PathBuf {
        self.home.join("silyn.db")
    }
}

fn run_doctor(json: bool) -> ExitCode {
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
        match serde_json::to_string_pretty(&payload) {
            Ok(text) => println!("{text}"),
            Err(error) => {
                eprintln!("failed to encode diagnostics: {error}");
                return ExitCode::FAILURE;
            }
        }
    } else {
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
    }

    ExitCode::SUCCESS
}

fn run_config(json: bool) -> ExitCode {
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
        match serde_json::to_string_pretty(&payload) {
            Ok(text) => println!("{text}"),
            Err(error) => {
                eprintln!("failed to encode config: {error}");
                return ExitCode::FAILURE;
            }
        }
    } else {
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
    }

    ExitCode::SUCCESS
}
