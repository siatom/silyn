# W7-03b: Runtime telemetry and redaction

```yaml
id: W7-03b
wave: 7
depends_on: [W3-01, W4-03, W7-03a]
owns:
  - crates/silyn-runtime/src/telemetry.rs
  - crates/silyn-runtime/src/{lib.rs,run.rs}
  - crates/silyn-runtime/Cargo.toml
  - eval/acceptance/w7-03b.toml
  - docs/gates/w7-03b.md
reads:
  - docs/architecture/observability.md
  - crates/silyn-runtime/src/**
forbids:
  - migrations/**
  - crates/silyn-core/**
  - crates/silyn-storage/**
  - crates/silyn-app/**
  - crates/silyn-cli/**
  - crates/silyn-growth/**
  - crates/silyn-eval/**
  - eval/suites/**
  - Cargo.toml
contracts: [tracing span names v1, redaction rules v1]
acceptance_registry:
  - telemetry.span_names
  - telemetry.no_prompt_fields
  - telemetry.redacts_secret_shapes
  - telemetry.run_id_recorded
commands:
  - cargo test -p silyn-runtime
  - cargo clippy -p silyn-runtime --all-targets -- -D warnings
  - cargo run -p xtask -- verify-acceptance
```

## Goal

Add structured `tracing` spans to the runtime using the frozen contract in
`docs/architecture/observability.md`, with a redaction helper that prevents
prompts, tool/memory content and secrets from entering span fields.

## Deliverables

- Add `tracing` to `crates/silyn-runtime/Cargo.toml`.
- `telemetry.rs`:
  - span names `run`, `step`, `model`, `tool`, `checkpoint`, `verify`;
  - field constants: `run_id`, `step_seq`, `tool`, `model`, `status`,
    `latency_ms`, `input_tokens`, `output_tokens`, `cost_usd`;
  - `redact(value) -> String` removes `sk-...`, `Bearer ...`, `api_key=...`,
    `DEEPSEEK_API_KEY=...` values and truncates to 200 bytes on a UTF-8
    boundary;
  - `span_*` helpers that create `tracing::Span` values with only safe fields.
- Instrument the run loop at boundaries:
  - run start/finish (status);
  - each step (step_seq);
  - model request/response (model, latency, tokens, cost);
  - tool start/finish (tool, status, latency);
  - checkpoint insert (step_seq);
  - verifier (outcome).
  Do not record prompt text, tool arguments, tool output, memory content, memory
  IDs? IDs are safe; record only IDs, not summaries/content.
- No subscriber is installed by the library; the CLI/API may install one later.
- Keep behavior identical when no subscriber is registered.

## Tests

- span-name constants exactly match the observability document;
- a key-shaped string, bearer header and `api_key=` are redacted;
- truncation is UTF-8 safe;
- a run with a MockProvider/temp storage still records the run id field (use a
  `tracing_subscriber` test subscriber if needed; it can be a dev-dependency).
- no span field constant contains `prompt`, `content`, `memory`, `api_key` or
  `secret`.

Add `eval/acceptance/w7-03b.toml` with one gate per test, `required = true`.
Add `docs/gates/w7-03b.md`.

## Commands and commit

```bash
cargo fmt --all
cargo test -p silyn-runtime
cargo clippy -p silyn-runtime --all-targets -- -D warnings
cargo run -p xtask -- verify-acceptance
```

Commit once on `wave7-telemetry` with
`feat(runtime): add redacted tracing spans`.
