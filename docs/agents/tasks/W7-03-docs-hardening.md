# W7-03: Observability, hardening, CI matrix and operator docs

```yaml
id: W7-03
wave: 7
depends_on: [W3-03, W5-03]
owns:
  - README.md
  - docs/operations.md
  - docs/security.md
  - docs/experiments/template.md
  - docs/architecture/observability.md
  - .github/workflows/ci.yml
  - xtask/src/main.rs
  - crates/silyn-runtime/src/telemetry.rs
  - crates/silyn-runtime/src/lib.rs
  - crates/silyn-runtime/Cargo.toml
  - eval/acceptance/w7-03.toml
  - docs/gates/w7-03.md
reads:
  - docs/architecture/**
  - crates/silyn-runtime/src/**
  - crates/silyn-app/src/**
  - crates/silyn-cli/src/main.rs
forbids:
  - migrations/**
  - crates/silyn-core/**
  - crates/silyn-storage/**
  - crates/silyn-growth/**
  - crates/silyn-app/**
  - crates/silyn-cli/**
  - crates/silyn-api/**
  - eval/suites/**
  - docs.local/**
contracts: [tracing span names v1]
acceptance_registry:
  - docs.readme_quickstart
  - docs.security_boundaries
  - docs.operations_rollback
  - ci.three_platforms
  - xtask.docs_gate
  - telemetry.no_secret_fields
commands:
  - cargo test -p silyn-runtime -p xtask
  - cargo clippy -p silyn-runtime -p xtask --all-targets -- -D warnings
  - cargo run -p xtask -- verify-docs
  - cargo run -p xtask -- verify-acceptance
```

## Goal

Make the project operable by someone other than its author: English README and
quickstart, security/operations runbooks, a three-platform CI matrix, a docs
gate, and a tracing layer whose fields cannot leak secrets.

## Deliverables

### Telemetry

- Add `tracing` to `silyn-runtime` and a `telemetry.rs` module with span
  helpers named `run`, `step`, `model`, `tool`, `checkpoint`, `verify`.
- Fields: `run_id`, `step_seq`, `tool`, `model`, `status`, `latency_ms`,
  `input_tokens`, `output_tokens`, `cost_usd`. Never include prompts, API keys,
  tool content, memory content or user text.
- A `redact` helper and a unit test that a key-shaped string is rejected from
  span fields.
- Optional OTel export is documented but not required; no new heavy dependency.

### Docs

- `README.md` (English): what Silyn is, invariants, quickstart in <= 10 minutes
  on a clean machine, CLI examples, experiment command, safety note, license.
- `docs/security.md`: trust boundaries, capabilities, default deny, cancel file,
  secret handling, sealed suite, threat model link, reporting a vulnerability.
- `docs/operations.md`: backup/restore, retention/purge, rollback, kill/resume,
  `Indeterminate` resolution, log locations, local-only assumptions.
- `docs/experiments/template.md`: model id, hashes, budget, repeats, CI, verdict.
- `docs/architecture/observability.md`: span names, fields, where logs go.
- Mermaid architecture diagram in README (no binary assets).

### CI

- `.github/workflows/ci.yml` matrix: `windows-latest`, `ubuntu-latest`,
  `macos-latest`; stable Rust 1.99; `cargo fmt --check`, clippy `-D warnings`,
  tests, release build, `xtask verify-*`.
- Cache cargo registry/target; `cargo test --workspace` must be offline-safe.
- No API keys in CI; W6 runs locally with a secret, not in CI.

### xtask

- `verify-docs`: README exists and contains Quickstart, Security, License;
  required docs exist; no `docs.local` links; no unresolved `TODO` in shipped
  docs; all relative links resolve.
- Keep existing gates working.

## Tests

- xtask unit tests for the docs gate (positive and missing-section negative).
- telemetry redaction test; span field names are compile-time constants.
- CI YAML contains all three runners and the required commands (string check in
  xtask test, no YAML parser dependency required).

Add `eval/acceptance/w7-03.toml` with one gate per test, `required = true`.
Add `docs/gates/w7-03.md`.

## Commands and commit

```bash
cargo fmt --all
cargo test -p silyn-runtime -p xtask
cargo clippy -p silyn-runtime -p xtask --all-targets -- -D warnings
cargo run -p xtask -- verify-docs
cargo run -p xtask -- verify-acceptance
```

Commit once on `wave7-docs` with
`docs: add operator guide, telemetry and three-platform ci`.
