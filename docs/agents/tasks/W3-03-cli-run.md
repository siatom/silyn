# W3-03: CLI run/resume integration and end-to-end recovery evidence

```yaml
id: W3-03
wave: 3
depends_on: [W3-01, W3-02]
owns:
  - crates/silyn-app/src/runtime_service.rs
  - crates/silyn-app/src/lib.rs
  - crates/silyn-app/Cargo.toml
  - crates/silyn-cli/src/main.rs
  - crates/silyn-cli/tests/run.rs
  - crates/silyn-cli/Cargo.toml
  - crates/silyn-runtime/src/run.rs
  - crates/silyn-runtime/src/lib.rs
  - eval/acceptance/w3-03.toml
  - docs/gates/w3-03.md
reads:
  - docs/agents/tasks/W3-01-runtime.md
  - docs/agents/tasks/W3-02-observability.md
  - crates/silyn-runtime/src/**
  - crates/silyn-app/src/**
  - crates/silyn-cli/src/main.rs
forbids:
  - migrations/**
  - eval/suites/**
  - crates/silyn-core/**
  - docs/architecture/**
  - Cargo.toml
contracts: [RunOptions v1, RunOutcome v1, CLI run surface v1]
notes:
  - `crates/silyn-runtime/**` is touched only to add `Runtime::create_run` (so the
    app can know the run id before execution and pass the cancel-file path) and
    to derive `Serialize` for `RunOutcome`. Do not change loop/budget/ledger
    semantics.
acceptance_registry:
  - cli.run_completes_offline
  - cli.run_creates_run_and_events
  - cli.run_budget_failure_exit_five
  - cli.resume_unknown_run_exits_five
  - cli.run_cancel_request_path
commands:
  - cargo test -p silyn-cli -p silyn-app -p silyn-runtime
  - cargo clippy -p silyn-cli -p silyn-app -p silyn-runtime --all-targets -- -D warnings
  - cargo run -p xtask -- verify-acceptance
```

## Goal

Wire the W3-01 runtime into `silyn-app` and the CLI, and prove the operator path
end to end: create/execute a run, inspect events, request cancel, resume a
pending run, and map outcomes to stable exit codes.

## Frozen CLI surface

```text
silyn run --task <task-id> [options]
silyn run --session <session-id> --objective <text> [options]
silyn run --resume <run-id> [options]
```

Options:

- `--model <id>` (default from `SILYN_LLM_MODEL`, else `deepseek-chat`)
- `--max-steps <n>` (default 16)
- `--max-wall-secs <n>` (default 600)
- `--max-input-tokens <n>` / `--max-output-tokens <n>`
- `--max-cost-usd <f64>` (requires a configured price; otherwise permanent)
- `--capability <name>` (repeatable). The same name may appear once; unknown
  names are rejected by the broker at execution time, not silently dropped.
- `--offline` uses `MockProvider` with a single final answer
  `{"action":"final","answer":"offline mock run"}`. It must not read an API
  key or touch the network. It is the only mode used by tests.
- `--json` is global and returns a `RunOutcome` object.

Default capabilities: **none**. A run that needs file tools must pass
`--capability fs.list`, `--capability fs.read`, etc. This keeps default-deny
visible at the CLI boundary.

Exit codes for `run` / `run --resume`:

| Outcome | Exit |
|---|---|
| `Completed` | 0 |
| `Cancelled` | 3 |
| `Failed` (including budget, loop, indeterminate, model error) | 5 |
| Invalid CLI usage | 2 |
| Run already terminal / unknown run | 5 |

`runs cancel` remains the W3-02 command. `runs events` and `runs tools` are the
observability surface.

## App service

Create `crates/silyn-app/src/runtime_service.rs`:

```rust
pub struct RunRequest {
    pub task_id: Option<TaskId>,
    pub session_id: Option<SessionId>,
    pub objective: Option<String>,
    pub resume: Option<RunId>,
    pub options: RunOptions,
}

impl App {
    /// Creates a task when `--session/--objective` is used, then runs it.
    pub async fn run(&self, request: RunRequest) -> Result<RunOutcome, SilynError>;

    /// Resumes an existing run; refuses terminal runs.
    pub async fn resume(&self, run_id: RunId, options: RunOptions)
        -> Result<RunOutcome, SilynError>;
}
```

Construction:

- Provider: `--offline` -> `MockProvider`; otherwise
  `OpenAiCompatibleProvider::new(ProviderConfig::from_env()?, ReqwestTransport::new(...))`.
  Reuse the W2 transport configuration; do not duplicate env parsing.
- Tools: `ToolBroker::new(home/workspace, Arc<dyn AuditSink>)`. Create
  `home/workspace` if missing. Use `MemoryAuditSink` in W3 (persistent sink is
  W5); expose the audit count in the JSON outcome only if it is already public,
  otherwise leave it out.
- Verifier: a small W3 `AnswerVerifier` implementing `OutcomeVerifier`:
  non-empty final answer -> `Unknown` (never `Success`), empty -> `Unknown`.
  W4 replaces it. Do not claim success.
- Budget/price: from CLI flags; price from `SILYN_LLM_INPUT_PRICE_PER_M` and
  `SILYN_LLM_OUTPUT_PRICE_PER_M` if both set, via `ProviderConfig`.
- Cancel file: `RunOptions.cancel_file = Some(app.cancel_request_path(run_id))`
  so a `runs cancel` from another terminal is observed by the runtime. For
  `run --task`, the run id is known only after `Runtime::run_task` creates it;
  add a `Runtime::run_task_with` or create the run first via a small
  `Runtime::create_run` helper if needed. Do not duplicate run creation in the
  app.

`RunRequest` validation:

- exactly one of `--task`, `--resume`, or (`--session` + `--objective`);
- `--resume` cannot be combined with task/session/objective;
- empty objective is a permanent error (App already validates this);
- `--offline` with `--model` is allowed (the model name is recorded).

## Tests

`crates/silyn-cli/tests/run.rs` (temp `SILYN_HOME`, no network):

- `run --session ... --objective ... --offline --json` exits 0, prints a
  `RunOutcome` with `status: "completed"`, `outcome: "unknown"`, and a final
  answer; `runs show <id> --json` reports `Completed`; `runs events` contains
  `run_started`, `run_verified`, `run_completed`.
- budget failure: `--max-steps 0` exits 5 and the run is `Failed` with a
  budget reason.
- `run --resume <unknown>` exits 5 and creates nothing.
- `run --task <unknown>` exits 5.
- cancel path: start no long-running process; instead assert the CLI passes the
  expected `cancel_file` path for a created run (unit test on the service), and
  that `runs cancel` on that run creates the file (W3-02).
- `--offline` never prints an API key (stderr scan).

App unit tests may call the service directly for `RunRequest` validation and
the verifier's `Unknown` result.

## Acceptance gates

Add `eval/acceptance/w3-03.toml` with one gate per test, `required = true`.

## Commands and commit

```bash
cargo fmt --all
cargo test -p silyn-cli -p silyn-app
cargo clippy -p silyn-cli -p silyn-app --all-targets -- -D warnings
cargo run -p xtask -- verify-acceptance
```

Commit once on `wave3/cli` with
`feat(cli): run and resume tasks through the runtime`.
Write `docs/gates/w3-03.md` with scope, commands, results and deferred work
(real process-kill harness, live SSE, persistent audit sink, verifier).
