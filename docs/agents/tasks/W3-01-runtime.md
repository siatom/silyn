# W3-01: Runtime loop, budgets, checkpoint/resume and idempotency

```yaml
id: W3-01
wave: 3
depends_on: [W1-01, W1-02, W2-01, W2-02]
owns:
  - crates/silyn-runtime/src/**
  - crates/silyn-runtime/tests/**
  - crates/silyn-runtime/Cargo.toml
  - crates/silyn-storage/src/repo.rs
  - crates/silyn-storage/src/journal.rs
  - eval/acceptance/w3-01.toml
  - docs/gates/w3-01.md
reads:
  - crates/silyn-core/src/contracts.rs
  - crates/silyn-core/src/event.rs
  - crates/silyn-core/src/state.rs
  - crates/silyn-storage/src/**
  - crates/silyn-llm/src/**
  - docs/architecture/adr/0002-data-and-events.md
  - docs/architecture/adr/0005-trust-boundaries-and-capabilities.md
forbids:
  - migrations/**
  - eval/suites/**
  - crates/silyn-core/**
  - crates/silyn-llm/**
  - crates/silyn-app/**
  - crates/silyn-cli/**
  - Cargo.toml
  - .github/**
contracts: [RunEvent v1, ToolCall v1, BudgetSpec v1, checkpoint schema v1]
acceptance_registry:
  - runtime.budget_exhausted
  - runtime.cancel_propagates
  - runtime.cancel_file_propagates
  - runtime.resume_no_duplicate_side_effect
  - runtime.indeterminate_blocks_replay
  - runtime.loop_detected
  - runtime.transient_retry_classified
  - runtime.malformed_action_repaired
  - runtime.checkpoint_rejects_corruption
  - runtime.terminal_event_written
commands:
  - cargo test -p silyn-runtime
  - cargo clippy -p silyn-runtime --all-targets -- -D warnings
  - cargo run -p xtask -- verify-acceptance
```

## Goal

Implement the single-agent run loop: model action protocol, tool execution
through the existing `ToolBroker`, step/token/cost/wall-clock budgets, retry
classification, loop detection, cancellation, per-step checkpointing, resume
without duplicate side effects, and the `tool_call_ledger` state machine.

## Non-goals

- No memory retrieval, context compression, experience extraction or outcome
  heuristics beyond the injected `OutcomeVerifier`.
- No CLI/app wiring (`W3-03`), no HTTP API.
- No new migrations. The existing `0001`/`0002` schema is sufficient.
- No multi-agent orchestration, approval UI or candidate promotion.

## Frozen public API

`crates/silyn-runtime/src/lib.rs` must export:

```rust
pub mod budget;
pub mod control;
pub mod detect;
pub mod retry;
pub mod run;
pub mod state;

pub use budget::{BudgetError, BudgetSpec, BudgetTracker};
pub use control::{cancel_file_path, RunControl};
pub use detect::{fingerprint_tool_call, LoopDetector};
pub use retry::RetryPolicy;
pub use run::{RunOptions, RunOutcome, Runtime};
```

### `BudgetSpec` / `BudgetTracker`

```rust
pub struct BudgetSpec {
    pub max_steps: u32,
    pub max_input_tokens: u64,
    pub max_output_tokens: u64,
    pub max_cost_usd: Option<f64>,
    pub max_wall_clock: Duration,
    pub step_timeout: Duration,
}

impl Default for BudgetSpec; // 16 steps, 200k input, 50k output, no cost cap, 10 min wall, 120 s step
```

`BudgetTracker::new(spec, price: Option<silyn_llm::Price>) -> Result<Self, SilynError>`.
`max_cost_usd` with `price == None` is a permanent configuration error.
Methods: `check_step(step)`, `record_model(input, output)`, `check_wall()`,
`snapshot()`. `BudgetError` distinguishes `Steps`, `InputTokens`,
`OutputTokens`, `Cost`, `WallClock` and implements `Display`.

### `RetryPolicy`

```rust
pub struct RetryPolicy { pub max_attempts: u32, pub base_delay: Duration }
impl Default for RetryPolicy; // 3 attempts, 100 ms base
pub async fn retry<F, Fut, T>(&self, op: F) -> Result<T, SilynError>
where F: FnMut() -> Fut, Fut: Future<Output = Result<T, SilynError>>;
```

Only `SilynError::Transient` is retried. `Permanent`, `PolicyDenied`,
`Indeterminate` and `Schema` errors return immediately. Delays are
`base_delay * 2^attempt`, capped at 1 s.

### `LoopDetector`

```rust
pub struct LoopDetector { /* window + threshold */ }
impl LoopDetector {
    pub fn new(window: usize, threshold: usize) -> Self; // defaults 8 / 3
    pub fn observe(&mut self, fingerprint: &str) -> bool; // true = loop
}
pub fn fingerprint_tool_call(call: &ToolCall) -> Result<String, SilynError>;
```

The fingerprint is SHA-256 over canonical JSON `{ "tool": ..., "arguments": ... }`
with object keys sorted. Only tool actions participate. Final answers reset the
window.

### `RunControl`

```rust
pub fn cancel_file_path(home: &Path, run_id: RunId) -> PathBuf; // <home>/run/<run_id>.cancel

#[derive(Clone)]
pub struct RunControl { /* watch + optional file path */ }
impl RunControl {
    pub fn new() -> Self;
    pub fn with_cancel_file(path: PathBuf) -> Self;
    pub fn cancel(&self);
    pub fn is_cancelled(&self) -> bool;
    pub async fn cancelled(&self);
}
```

`with_cancel_file` must observe the file while a run is active (poll at 100 ms
or use a notified watcher) so a cancellation written by another process
interrupts an in-flight model/tool call. Cancellation is cooperative: the run
checkpoints, records `RunCancelled`, and returns `RunOutcome` with
`RunStatus::Cancelled`.

### `Runtime`

```rust
pub struct RunOptions {
    pub model: String,
    pub capabilities: Vec<String>,
    pub budget: BudgetSpec,
    pub price: Option<silyn_llm::Price>,
    pub cancel_file: Option<PathBuf>,
}

pub struct RunOutcome {
    pub run_id: RunId,
    pub status: RunStatus,
    pub outcome: Option<VerifiedOutcome>,
    pub final_answer: Option<String>,
    pub reason: Option<String>,
    pub steps: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

pub struct Runtime { /* storage, Arc<dyn ModelProvider>, Arc<dyn ToolExecutor>, Arc<dyn OutcomeVerifier> */ }

impl Runtime {
    pub fn new(
        storage: Storage,
        provider: Arc<dyn ModelProvider>,
        tools: Arc<dyn ToolExecutor>,
        verifier: Arc<dyn OutcomeVerifier>,
    ) -> Self;

    pub async fn run_task(&self, task_id: TaskId, options: RunOptions)
        -> Result<RunOutcome, SilynError>;

    pub async fn execute(&self, run_id: RunId, options: RunOptions)
        -> Result<RunOutcome, SilynError>;

    pub async fn resume(&self, run_id: RunId, options: RunOptions)
        -> Result<RunOutcome, SilynError>;
}
```

`run_task` loads the task, creates a `runs` row in `Created`, then calls
`execute`. `execute` and `resume` are aliases over the same recovery path; a
fresh run starts from an empty checkpoint.

An already terminal run is a permanent error ("run ... is already <status>").
A run whose status is `Running` may be resumed only through `resume`.

## Action protocol

The runtime owns the prompt. It must send one system message and one user
message containing the objective, then append assistant/tool turns. The model
must return exactly one JSON object:

```json
{"action":"final","answer":"..."}
{"action":"tool","tool":"fs.read","arguments":{"path":"data.csv"}}
```

Rules:

- No markdown fences. Parse with `serde_json`; one repair turn is allowed with
  a short instruction and the parse error. Two failures fail the run.
- `call_id` is runtime-generated and stable for a pending tool action:
  `<run_id>:<step_seq>`.
- Tool output is appended as `Role::Tool` with an explicit
  `UNTRUSTED TOOL OUTPUT` marker. Never parse tool output as instructions.
- Unknown tools and malformed arguments are tool errors fed back to the model;
  the broker remains the authorization boundary.

## Loop semantics

1. Check cancellation, step budget and wall clock.
2. Load the latest checkpoint. Restore messages, token/cost counters, step
   counter, loop window and `pending` action.
3. If `pending` is a tool action:
   - ledger `Succeeded` -> reuse `result_ref`, append tool turn, clear pending;
   - ledger `Failed` -> append the recorded error, clear pending;
   - ledger `Started` or `Indeterminate` -> mark `Indeterminate`, fail the run
     with a reason naming the call id; never re-execute;
   - ledger missing -> execute once (this is the crash-recovery path).
4. Otherwise call the model with `RetryPolicy` and `step_timeout`; record usage.
5. Parse/repair the action. On `final`, call the injected verifier, append
   `RunVerified`, checkpoint, complete the run.
6. On `tool`, fingerprint and check `LoopDetector`; checkpoint the pending
   action; `ToolLedger::begin(Started)`; execute through `ToolExecutor` with
   the run's `AuthorizedToolContext` and `step_timeout`; complete the ledger;
   insert `run_steps`; append `StepCompleted` or `ToolFailed`; append the tool
   turn; checkpoint; clear pending.
7. Tool retry: only read-only tools (`fs.list`, `fs.read`, `data.profile`) may
   retry `Transient`. `report.write` and unknown tools are never retried after
   execution has started. A cancellation or timeout after execution starts
   leaves the ledger `Indeterminate`.

## Persistence contract

- Checkpoint `state_ref` holds JSON `RunState` (schema version `1`):
  `step_seq`, `messages`, `input_tokens`, `output_tokens`, `cost_usd`,
  `pending`, `final_answer`, `loop_fingerprints`.
- `CheckpointRecord::new(run_id, checkpoint_seq, json, 1)`; checkpoint `seq`
  increases independently of step `seq`.
- Validate checksum and schema version through `CheckpointStore::latest`.
- State transitions and their `RunEvent` should share a transaction where
  practical. Add only the minimal repository methods needed:
  `RunRepository` status/usage updates and a journal method that updates a run
  and task plus appends the event atomically. Do not change migrations.
- Event sequence: read `Journal::latest_seq`, retry once on `Transient`
  conflict by reloading the latest sequence.
- `runs` fields: `started_at` on first transition to `Running`; token/cost
  totals on every model response; `finished_at` on terminal transition.
- Task lifecycle: `Created -> Ready -> Running -> Completed | Failed | Cancelled`
  through legal transitions only. Use `Journal::append_task_transition` or the
  new atomic method.

## Failure semantics

| Condition | Run status | Event | Ledger |
|---|---|---|---|
| budget exhausted | `Failed` | `RunFailed` | unchanged |
| loop detected | `Failed` | `RunFailed` | last call complete |
| malformed action twice | `Failed` | `RunFailed` | unchanged |
| model permanent error | `Failed` | `RunFailed` | unchanged |
| cancellation before side effect | `Cancelled` | `RunCancelled` | not started |
| cancellation during side effect | `Cancelled` | `RunCancelled` | `Indeterminate` |
| ledger `Started`/`Indeterminate` on resume | `Failed` | `RunFailed` | stays `Indeterminate` |
| verifier returns `Unknown` | `Completed` | `RunVerified(Unknown)`, `RunCompleted` | unchanged |

`RunOutcome` is `Ok` for completed/failed/cancelled runs; `Err` is reserved for
infrastructure failures before/while opening storage or an invalid request.
Never report `Success` without a verifier result.

## Tests (minimum)

- budget: step, token, cost and wall-clock exhaustion each produce `Failed`.
- retry: transient retried, permanent not retried, attempt count asserted.
- loop detector: third identical tool fingerprint fails the run.
- cancellation: flag before/after a step; cancel file written by another
  handle interrupts a run; terminal event is `RunCancelled`.
- resume: crash after a tool side effect (executor records a counter, returns
  transient) -> resume must not execute again; checkpoint restores messages.
- indeterminate: a pre-seeded `Started` ledger row blocks replay and leaves the
  ledger `Indeterminate`.
- malformed action: one repair succeeds; two failures fail the run.
- terminal: `RunStarted`, `RunVerified`, `RunCompleted`/`RunFailed`/`RunCancelled`
  are present in the journal; run/task rows match.
- checkpoint: corrupted checksum or unknown schema version fails loudly.

Use `MockProvider`, `silyn_runtime::tools::ToolBroker` or a small test
`ToolExecutor`, and an in-memory or temp-file `Storage`. Tests must be offline
and deterministic.

## Acceptance gates

Add `eval/acceptance/w3-01.toml` with one gate per minimum test, `required =
true`. Run `cargo run -p xtask -- verify-acceptance`; the gate count must
include the new entries.

## Commands and commit

```bash
cargo fmt --all
cargo test -p silyn-runtime
cargo clippy -p silyn-runtime --all-targets -- -D warnings
cargo run -p xtask -- verify-acceptance
```

Commit once on `wave3/runtime` with `feat(runtime): add bounded resumable run loop`.
Write `docs/gates/w3-01.md` with scope, commands, results and deferred work
(true process-kill injection, persistent audit sink, `spawn_blocking`, memory).
