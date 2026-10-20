# W3-02: Run observability and cancellation surface

```yaml
id: W3-02
wave: 3
depends_on: [W1-01, W1-02, W2-03]
owns:
  - crates/silyn-app/src/observability.rs
  - crates/silyn-app/src/lib.rs
  - crates/silyn-app/Cargo.toml
  - crates/silyn-cli/src/main.rs
  - crates/silyn-cli/tests/observe.rs
  - crates/silyn-cli/Cargo.toml
  - crates/silyn-storage/src/ledger.rs
  - eval/acceptance/w3-02.toml
  - docs/gates/w3-02.md
reads:
  - crates/silyn-core/src/event.rs
  - crates/silyn-core/src/state.rs
  - crates/silyn-storage/src/journal.rs
  - crates/silyn-storage/src/ledger.rs
  - crates/silyn-app/src/app.rs
  - crates/silyn-cli/src/main.rs
  - docs/agents/tasks/W3-01-runtime.md
forbids:
  - migrations/**
  - eval/suites/**
  - crates/silyn-core/**
  - crates/silyn-runtime/**
  - docs/architecture/**
  - Cargo.toml
contracts: [RunEvent v1, ToolCall ledger, cancel file v1]
acceptance_registry:
  - observe.run_events_listed
  - observe.unknown_run_events_rejected
  - observe.tool_ledger_listed
  - observe.cancel_file_created
  - observe.cancel_is_idempotent
  - observe.cancel_terminal_run_rejected
commands:
  - cargo test -p silyn-cli -p silyn-app
  - cargo clippy -p silyn-cli -p silyn-app --all-targets -- -D warnings
  - cargo run -p xtask -- verify-acceptance
```

## Goal

Expose the read/control surface an operator needs before the run loop lands:

- list a run's journal events;
- list a run's tool-call ledger and its `Indeterminate` state;
- request cancellation of a run through a cross-process file signal.

This task must not implement the runtime loop or change the `silyn run`
behavior beyond keeping its existing "runtime lands in W3" permanent error.

## Cancel protocol (frozen)

- Path: `<home>/run/<run_id>.cancel`, where `<home>` is the `App` home.
- `runs cancel <id>` validates the run exists and is non-terminal, creates the
  `run/` directory, and writes an empty file. Writing when the file already
  exists is idempotent and reports `already_requested: true`.
- The runtime (W3-01) polls this file and transitions the run to `Cancelled`.
  The CLI must not update run status itself.
- Terminal runs (`Completed`, `Failed`, `Cancelled`) are a permanent error.
- Unknown runs are a permanent error.

## Storage addition

Add to `crates/silyn-storage/src/ledger.rs`:

```rust
impl ToolLedger<'_> {
    pub async fn list_by_run(&self, run_id: RunId) -> Result<Vec<ToolCallRecord>, SilynError>;
}
```

Order by `created_at`, then `step_id`, then `tool_call_id`. Decode through the
same helpers as `get`; no duplicate row-decoding logic.

## App additions

Create `crates/silyn-app/src/observability.rs` and re-export from `lib.rs`:

```rust
pub struct CancelRequest {
    pub run_id: RunId,
    pub path: PathBuf,
    pub already_requested: bool,
}

impl App {
    pub async fn list_run_events(&self, run_id: RunId)
        -> Result<Vec<silyn_core::event::EventEnvelope>, SilynError>;

    pub async fn list_tool_calls(&self, run_id: RunId)
        -> Result<Vec<ToolCallRecord>, SilynError>;

    pub async fn request_cancel(&self, run_id: RunId)
        -> Result<CancelRequest, SilynError>;

    /// `<home>/run/<run_id>.cancel`; does not touch the filesystem.
    pub fn cancel_request_path(&self, run_id: RunId) -> PathBuf;
}
```

`list_run_events` and `list_tool_calls` validate that the run exists first
(`RunRepository::get`), then use `Journal::list_for_aggregate` and
`ToolLedger::list_by_run`. Empty vectors are valid for an existing run.

Keep `silyn-app` free of a dependency on `silyn-runtime`; the path contract is
duplicated as a string and covered by a test.

## CLI additions

Extend the existing `runs` subcommand:

```text
silyn runs events <run-id> [--json]
silyn runs tools  <run-id> [--json]
silyn runs cancel <run-id> [--json]
```

- `events`: JSON array of `EventEnvelope`; human mode prints
  `seq<TAB>type<TAB>occurred_at<TAB>event_id`.
- `tools`: JSON array of ledger records (including `state`); human mode prints
  `step_id<TAB>tool_name<TAB>state<TAB>call_id<TAB>result_ref`.
- `cancel`: JSON `{ "run_id", "path", "already_requested" }`; human mode prints
  the path and whether the request already existed.
- Unknown/terminal runs use the existing permanent-error mapping (exit 5).
- `silyn run` keeps returning exit 5 with
  `runtime loop is not available until W3`; do not create a run.
- All commands respect the global `--json` and never print secrets.

## Tests (minimum)

Use a temp `SILYN_HOME` and `std::process::Command` as in `tests/cli.rs`, or
call `App` directly for the service-level cases:

- append two events to a run, then `runs events --json` returns both in seq
  order; unknown run exits 5.
- seed a `tool_call_ledger` row (`Started` and `Indeterminate`), then
  `runs tools --json` exposes the state; unknown run exits 5.
- `runs cancel --json` creates the exact file path, returns
  `already_requested: false`; a second call returns `true` and keeps one file.
- cancelling a terminal run exits 5; cancelling an unknown run exits 5.
- `silyn run` still exits 5.

## Acceptance gates

Add `eval/acceptance/w3-02.toml` with one gate per test, `required = true`.

## Commands and commit

```bash
cargo fmt --all
cargo test -p silyn-cli -p silyn-app
cargo clippy -p silyn-cli -p silyn-app --all-targets -- -D warnings
cargo run -p xtask -- verify-acceptance
```

Commit once on `wave3/observe` with
`feat(cli): expose run events, tool ledger and cancel requests`.
Write `docs/gates/w3-02.md` with scope, commands, results and deferred work
(`runs resume`, live event streaming, status transitions owned by the runtime).
