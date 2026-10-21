//! Subprocess isolation for candidate code.
//!
//! The CLI never runs a candidate in the harness process. It re-executes its own
//! binary as `silyn-eval worker`, which reads one [`TaskInput`] JSON object from
//! stdin and writes one [`Submission`] JSON object to stdout.
//!
//! Every attempt gets:
//! - a cleared environment plus an allowlist (`PATH`, `SYSTEMROOT`, `TEMP`, `TMP`
//!   on Windows; `PATH`, `HOME` on Unix). macOS CoreFoundation adds
//!   `__CF_USER_TEXT_ENCODING` to every child after `env_clear()`; it carries no
//!   secret and is documented rather than removed;
//! - a fresh temporary working directory, which is also the base for the task
//!   database (`<cwd>/silyn-eval.sqlite`) so no two attempts share state;
//! - a wall-clock timeout, after which the child is killed.
//!
//! A non-zero exit, invalid JSON, a panic or a timeout is a task failure. It is
//! never a harness crash.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use silyn_core::SilynError;

use crate::backend::{RunnerBackend, TaskAttempt, TaskBudget, TaskFailure, TaskFailureKind};
use crate::baseline::BaselineSolver;
use crate::evaluator::Solver;
use crate::task::{Submission, TaskInput};

/// Subcommand name of the worker process.
pub const WORKER_SUBCOMMAND: &str = "worker";

/// Name of the per-task database file inside the fresh working directory. The
/// path is derived from the cwd so it needs no extra environment variable.
pub const TASK_DATABASE_FILE: &str = "silyn-eval.sqlite";

/// Prefix of the diagnostics line the worker writes to stderr.
pub const DIAGNOSTICS_PREFIX: &str = "silyn-eval-diagnostics: ";

/// Environment variables the worker is allowed to inherit.
#[must_use]
pub fn allowed_env_vars() -> &'static [&'static str] {
    if cfg!(windows) {
        &["PATH", "SYSTEMROOT", "TEMP", "TMP"]
    } else {
        &["PATH", "HOME"]
    }
}

/// Injected worker behaviour. Only integration tests set anything other than
/// [`WorkerFault::None`]; the CLI never exposes the flag in its help output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkerFault {
    /// Run the registered solver (the W5 baseline).
    #[default]
    None,
    /// Panic after reading the task.
    Panic,
    /// Exit with status 3 after reading the task.
    NonZero,
    /// Exit zero with stdout that is not a submission.
    Garbage,
    /// Sleep past any sane budget so the harness has to kill the process.
    Sleep,
}

impl WorkerFault {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Panic => "panic",
            Self::NonZero => "nonzero",
            Self::Garbage => "garbage",
            Self::Sleep => "sleep",
        }
    }

    pub fn parse(value: &str) -> Result<Self, SilynError> {
        match value {
            "none" => Ok(Self::None),
            "panic" => Ok(Self::Panic),
            "nonzero" => Ok(Self::NonZero),
            "garbage" => Ok(Self::Garbage),
            "sleep" => Ok(Self::Sleep),
            other => Err(SilynError::Permanent(format!(
                "unknown worker fault `{other}` (expected none, panic, nonzero, garbage or sleep)"
            ))),
        }
    }
}

/// What the worker observed about its own sandbox. Written to stderr only when
/// `--diagnostics` is passed, so the isolation contract is testable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerDiagnostics {
    pub env_keys: Vec<String>,
    pub allowed_env: Vec<String>,
    pub cwd: String,
    pub database_path: String,
}

/// One completed worker invocation.
#[derive(Debug, Clone)]
pub struct WorkerRun {
    pub attempt: TaskAttempt,
    pub stderr: String,
    pub diagnostics: Option<WorkerDiagnostics>,
    pub exit_code: Option<i32>,
}

/// How to launch the worker.
#[derive(Debug, Clone)]
pub struct WorkerSpawn {
    pub executable: PathBuf,
    pub fault: WorkerFault,
    pub diagnostics: bool,
}

impl WorkerSpawn {
    #[must_use]
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            fault: WorkerFault::None,
            diagnostics: false,
        }
    }
}

/// Backend that runs every attempt in a fresh worker process.
#[derive(Debug, Clone)]
pub struct SubprocessBackend {
    spawn: WorkerSpawn,
}

impl SubprocessBackend {
    #[must_use]
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            spawn: WorkerSpawn::new(executable),
        }
    }

    /// Uses the running binary. The CLI is the only place that should do this.
    pub fn current_exe() -> Result<Self, SilynError> {
        let exe = std::env::current_exe().map_err(|error| {
            SilynError::Permanent(format!("resolve the current executable: {error}"))
        })?;
        Ok(Self::new(exe))
    }

    /// Test hook: make the worker misbehave on purpose.
    #[must_use]
    pub fn with_fault(mut self, fault: WorkerFault) -> Self {
        self.spawn.fault = fault;
        self
    }

    /// Test hook: ask the worker to report its sandbox on stderr.
    #[must_use]
    pub fn with_diagnostics(mut self, diagnostics: bool) -> Self {
        self.spawn.diagnostics = diagnostics;
        self
    }

    #[must_use]
    pub fn spawn(&self) -> &WorkerSpawn {
        &self.spawn
    }

    /// Runs one attempt and keeps the stderr/diagnostics for tests.
    #[must_use]
    pub fn run_detailed(&self, input: &TaskInput, budget: &TaskBudget) -> WorkerRun {
        run_worker_process(&self.spawn, input, budget)
    }
}

impl RunnerBackend for SubprocessBackend {
    fn name(&self) -> &'static str {
        "subprocess"
    }

    fn run(&self, input: &TaskInput, budget: &TaskBudget) -> TaskAttempt {
        self.run_detailed(input, budget).attempt
    }
}

/// Spawns one worker, feeds it one task and normalizes every outcome.
#[must_use]
pub fn run_worker_process(
    spawn: &WorkerSpawn,
    input: &TaskInput,
    budget: &TaskBudget,
) -> WorkerRun {
    let started = Instant::now();
    let temp = match tempfile::Builder::new()
        .prefix("silyn-eval-task-")
        .tempdir()
    {
        Ok(temp) => temp,
        Err(error) => {
            return WorkerRun {
                attempt: TaskAttempt::failed(
                    TaskFailure::new(
                        TaskFailureKind::SpawnError,
                        format!("create task directory: {error}"),
                    ),
                    started.elapsed().as_secs_f64() * 1000.0,
                ),
                stderr: String::new(),
                diagnostics: None,
                exit_code: None,
            };
        }
    };

    let mut command = Command::new(&spawn.executable);
    command.arg(WORKER_SUBCOMMAND);
    if spawn.fault != WorkerFault::None {
        command.arg("--fault").arg(spawn.fault.as_str());
    }
    if spawn.diagnostics {
        command.arg("--diagnostics");
    }
    command
        .current_dir(temp.path())
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for key in allowed_env_vars() {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return WorkerRun {
                attempt: TaskAttempt::failed(
                    TaskFailure::new(
                        TaskFailureKind::SpawnError,
                        format!("spawn worker {}: {error}", spawn.executable.display()),
                    ),
                    started.elapsed().as_secs_f64() * 1000.0,
                ),
                stderr: String::new(),
                diagnostics: None,
                exit_code: None,
            };
        }
    };

    if let Ok(payload) = serde_json::to_vec(input) {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(&payload);
            let _ = stdin.flush();
        }
    }
    // Dropping stdin above closes the pipe, so the worker cannot wait forever.

    let deadline = started + Duration::from_millis(budget.timeout_ms.max(1));
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return WorkerRun {
                    attempt: TaskAttempt::failed(
                        TaskFailure::new(
                            TaskFailureKind::SpawnError,
                            format!("poll worker: {error}"),
                        ),
                        started.elapsed().as_secs_f64() * 1000.0,
                    ),
                    stderr: String::new(),
                    diagnostics: None,
                    exit_code: None,
                };
            }
        }
        if Instant::now() >= deadline {
            timed_out = true;
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(5));
    };

    let stdout = read_pipe(&mut child);
    let stderr = read_pipe_stderr(&mut child);
    let duration_ms = started.elapsed().as_secs_f64() * 1000.0;
    let exit_code = status.and_then(|status| status.code());

    if timed_out {
        return WorkerRun {
            attempt: TaskAttempt::failed(
                TaskFailure::new(
                    TaskFailureKind::Timeout,
                    format!("worker exceeded {} ms", budget.timeout_ms),
                ),
                duration_ms,
            ),
            stderr,
            diagnostics: None,
            exit_code,
        };
    }

    let Some(status) = status else {
        return WorkerRun {
            attempt: TaskAttempt::failed(
                TaskFailure::new(TaskFailureKind::SpawnError, "worker produced no status"),
                duration_ms,
            ),
            stderr,
            diagnostics: None,
            exit_code,
        };
    };

    if !status.success() {
        return WorkerRun {
            attempt: TaskAttempt::failed(
                TaskFailure::new(
                    TaskFailureKind::NonZeroExit,
                    format!("worker exited with {status}: {stderr}"),
                ),
                duration_ms,
            ),
            diagnostics: parse_diagnostics(&stderr),
            stderr,
            exit_code,
        };
    }

    match serde_json::from_str::<Submission>(stdout.trim()) {
        Ok(submission) => WorkerRun {
            attempt: TaskAttempt::succeeded(submission, duration_ms),
            diagnostics: parse_diagnostics(&stderr),
            stderr,
            exit_code,
        },
        Err(error) => WorkerRun {
            attempt: TaskAttempt::failed(
                TaskFailure::new(
                    TaskFailureKind::InvalidOutput,
                    format!("stdout is not one submission: {error}"),
                ),
                duration_ms,
            ),
            diagnostics: parse_diagnostics(&stderr),
            stderr,
            exit_code,
        },
    }
}

fn read_pipe(child: &mut Child) -> String {
    let mut buffer = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut buffer);
    }
    buffer
}

fn read_pipe_stderr(child: &mut Child) -> String {
    let mut buffer = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut buffer);
    }
    buffer.trim().to_owned()
}

fn parse_diagnostics(stderr: &str) -> Option<WorkerDiagnostics> {
    stderr
        .lines()
        .find_map(|line| line.strip_prefix(DIAGNOSTICS_PREFIX))
        .and_then(|json| serde_json::from_str(json).ok())
}

/// Entry point for `silyn-eval worker`. Returns the process exit code.
///
/// Reads one task from stdin, writes one submission to stdout. Errors go to
/// stderr with a non-zero exit; nothing is retried inside the worker.
pub fn run_worker(fault: WorkerFault, diagnostics: bool) -> i32 {
    let mut raw = String::new();
    if let Err(error) = std::io::stdin().read_to_string(&mut raw) {
        eprintln!("worker: read stdin: {error}");
        return 2;
    }
    let task: TaskInput = match serde_json::from_str(&raw) {
        Ok(task) => task,
        Err(error) => {
            eprintln!("worker: invalid TaskInput: {error}");
            return 2;
        }
    };

    if diagnostics {
        let cwd = std::env::current_dir()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let mut env_keys: Vec<String> = std::env::vars_os()
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect();
        env_keys.sort();
        let report = WorkerDiagnostics {
            env_keys,
            allowed_env: allowed_env_vars()
                .iter()
                .map(|key| (*key).to_owned())
                .collect(),
            cwd: cwd.clone(),
            database_path: PathBuf::from(&cwd)
                .join(TASK_DATABASE_FILE)
                .display()
                .to_string(),
        };
        match serde_json::to_string(&report) {
            Ok(json) => eprintln!("{DIAGNOSTICS_PREFIX}{json}"),
            Err(error) => eprintln!("worker: encode diagnostics: {error}"),
        }
    }

    match fault {
        WorkerFault::None => match BaselineSolver.solve(&task) {
            Ok(submission) => match serde_json::to_string(&submission) {
                Ok(json) => {
                    println!("{json}");
                    0
                }
                Err(error) => {
                    eprintln!("worker: encode submission: {error}");
                    2
                }
            },
            Err(error) => {
                eprintln!("worker: solver failed: {error}");
                2
            }
        },
        WorkerFault::Panic => panic!("worker fault: deliberate panic"),
        WorkerFault::NonZero => {
            eprintln!("worker fault: deliberate non-zero exit");
            3
        }
        WorkerFault::Garbage => {
            println!("not a submission");
            0
        }
        WorkerFault::Sleep => {
            std::thread::sleep(Duration::from_secs(30));
            0
        }
    }
}
