# W6-01: Real model solver and A/B/C context arms

```yaml
id: W6-01
wave: 6
depends_on: [W5-01, W5-02]
owns:
  - crates/silyn-eval/src/model_solver.rs
  - crates/silyn-eval/src/context.rs
  - crates/silyn-eval/src/bin/silyn_eval.rs
  - crates/silyn-eval/src/lib.rs
  - crates/silyn-eval/Cargo.toml
  - crates/silyn-eval/tests/model_solver.rs
  - eval/acceptance/w6-01.toml
  - docs/gates/w6-01.md
reads:
  - docs/agents/tasks/W6-formal-experiment.md
  - crates/silyn-eval/src/{evaluator,arm,report,statistics,isolation}.rs
  - crates/silyn-llm/src/{provider,config,transport,mock}.rs
  - crates/silyn-core/src/contracts.rs
forbids:
  - migrations/**
  - crates/silyn-core/**
  - crates/silyn-storage/**
  - crates/silyn-runtime/**
  - crates/silyn-app/**
  - crates/silyn-cli/**
  - crates/silyn-growth/**
  - eval/suites/**
  - Cargo.toml
contracts: [Solver v1, ArmContext v1, experiment context file v1]
acceptance_registry:
  - w6.context_prompts_differ_by_arm
  - w6.solver_parses_submission
  - w6.solver_rejects_non_json
  - w6.solver_records_model_id
  - w6.no_api_key_in_errors
  - w6.equal_budget_preserved
commands:
  - cargo test -p silyn-eval
  - cargo clippy -p silyn-eval --all-targets -- -D warnings
  - cargo run -p xtask -- verify-acceptance
```

## Goal

Add a real OpenAI-compatible model solver and declared A/B/C contexts so W6 can
run the sealed experiment with the same harness. Offline tests use
`MockProvider`; the real run is a separate operator step.

## Context file

`silyn-eval run --solver model --context <file> [...]` reads:

```json
{
  "memory": ["Check units before computing aggregates."],
  "policy": ["Always inspect the header before counting rows."],
  "provenance": {
    "memory_source": "silyn-memory:<agent-id>",
    "policy_version": "<uuid>",
    "notes": "free text, no secrets"
  }
}
```

- A: `memory=[]`, `policy=[]`.
- B: `memory` only.
- C: `memory + policy`.
- The context file's sha256 and provenance are recorded in the statistics
  document; the file must not contain API keys.
- Missing/extra fields are rejected. `memory`/`policy` strings are bounded
  (max 20 entries, 2000 bytes each) and control characters are rejected.
- Context strings are untrusted data in the prompt, never capabilities.

## Solver

`crates/silyn-eval/src/model_solver.rs`:

```rust
pub struct ModelSolver { /* Arc<dyn ModelProvider>, model, context, timeout, max_tokens */ }
impl Solver for ModelSolver {
    fn solve(&self, input: &TaskInput) -> Result<Submission, SilynError>;
    fn cost_usd_per_call(&self) -> f64;
}
```

- Build a system prompt with the CSV task rules and the arm context; include
  the CSV and objective. Require one JSON object with keys `row_count`,
  `missing_total`, `duplicate_rows`, `unit`.
- Use `tokio::runtime::Runtime::new()?.block_on(...)` to call the async
  `ModelProvider`; apply a per-call timeout. Do not use `futures::executor`
  (the provider uses tokio TLS/time).
- Parse `Submission`; a malformed response is a task failure, not a panic.
- Cost: compute from `Price` if provided; otherwise 0.0 and record that the
  provider has no price table in the arm config.
- Never print the API key, prompt, or CSV content in errors. Truncate provider
  errors to a safe length.
- `ModelSolver` is used in-process only. The subprocess worker keeps the
  deterministic baseline; document the isolation caveat.

## CLI / arms

- Add `--solver baseline|model` (default baseline), `--context <file>`,
  `--model <id>`, `--base-url <url>`, `--api-key-env <name>` (default
  `DEEPSEEK_API_KEY`), `--in-process` required for `model`.
- Build `ArmContext` per arm and a `ContextualSolver` wrapper; run A/B/C through
  the existing `Evaluator`/backend with the same budget. If the runner currently
  reuses one solver for all arms, add `run_matrix_with` or an equivalent
  per-arm solver map; keep `run_matrix` for the baseline.
- Record the actual model id, solver id, context sha256 and arm labels in the
  statistics JSON. The report model id must match the provider model.
- `--solver model` with `--split test` still requires `--allow-sealed`; do not
  bypass the sealed-set check.
- Keep baseline behavior byte-compatible for existing tests.

## Tests (offline)

- `MockProvider` returns a valid submission -> success; malformed JSON ->
  task failure; provider error -> failure, no panic.
- The A prompt contains no memory/policy; B contains memory only; C contains
  both; all contain the CSV objective.
- API-key-shaped strings never appear in error messages (test a provider error
  containing `sk-...` and assert redaction/truncation).
- `ArmsConfig` fairness still passes; context changes do not alter budget/model.
- Context file validation (bounds, control chars, extra fields).
- `run_matrix_with` gives each arm its context and keeps paired task order.

Add `eval/acceptance/w6-01.toml` with one gate per test, `required = true`.
Add `docs/gates/w6-01.md`.

## Commands and commit

```bash
cargo fmt --all
cargo test -p silyn-eval
cargo clippy -p silyn-eval --all-targets -- -D warnings
cargo run -p xtask -- verify-acceptance
```

Commit once on `wave6-solver` with
`feat(eval): add model solver and declared abc contexts`.
