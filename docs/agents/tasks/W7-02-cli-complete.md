# W7-02: Complete CLI surface, reporting and JSON contract

```yaml
id: W7-02
wave: 7
depends_on: [W3-03, W4-03, W5-03, W7-01]
owns:
  - crates/silyn-cli/src/main.rs
  - crates/silyn-cli/tests/**
  - crates/silyn-cli/Cargo.toml
  - docs/architecture/cli.md
  - eval/acceptance/w7-02.toml
  - docs/gates/w7-02.md
reads:
  - crates/silyn-app/src/**
  - crates/silyn-api/src/**
  - docs/agents/tasks/W7-01-http-api.md
forbids:
  - migrations/**
  - crates/silyn-core/**
  - crates/silyn-storage/**
  - crates/silyn-runtime/**
  - crates/silyn-growth/**
  - crates/silyn-app/**
  - eval/suites/**
  - Cargo.toml
contracts: [CLI surface v1, exit codes v1, JSON envelope v1]
acceptance_registry:
  - cli.all_commands_have_json
  - cli.exit_codes_stable
  - cli.report_export
  - cli.serve_starts_api
  - cli.memory_and_experience
  - cli.evolution_round_trip
commands:
  - cargo test -p silyn-cli
  - cargo clippy -p silyn-cli --all-targets -- -D warnings
  - cargo run -p xtask -- verify-acceptance
```

## Goal

Complete the scriptable CLI: every read/write command supports `--json`, exit
codes are stable, runs can be reported, and `silyn serve` starts the local HTTP
API.

## Commands

```text
silyn init
silyn doctor
silyn config
silyn agent create|list
silyn session create
silyn task create
silyn run --task|--session+--objective|--resume
silyn runs list|show|events|tools|cancel
silyn memory add|search|list|delete
silyn experience list
silyn evolution list|show|submit|evaluate|approve|promote|rollback
silyn report run <run-id> [--format json|markdown] [--out <path>]
silyn report experiment <report.json> [--format markdown] [--out <path>]
silyn serve [--addr <host:port>] [--token-env <name>]
```

- `report run`: bundle run record, events, tool ledger, outcome, token/cost,
  and zero-unauthorized-actions claim only if the audit proves it. Markdown is
  a rendering of the same JSON, never a separate source of numbers.
- `report experiment`: render an `EvalReport` with hashes, arms, CI, verdict.
- `serve`: call `silyn-api`'s server; do not reimplement routing. Print the
  bound address unless `--json`; handle Ctrl-C gracefully.
- `--json` output is always one JSON document on stdout, errors on stderr as
  `{"error":{"class","message"}}`. Human mode tables are stable and truncate
  long fields with an ellipsis.
- Exit codes: 0 success, 2 usage, 3 policy/cancelled, 4 transient, 5 permanent.
  Document the cancellation case explicitly.

## Tests

- every top-level command with `--json` emits valid JSON (use `--help`/empty
  home where appropriate).
- exit codes for usage, unknown id, policy denied, cancelled run and transient
  simulated error.
- report run exports JSON/Markdown containing run id, events, tool states and
  costs; no secrets.
- `serve` binds an ephemeral loopback port in a test and `/health` responds,
  then shuts down.
- memory/experience/evolution JSON round trips.
- logs never contain `DEEPSEEK_API_KEY` or a key-shaped string.

Add `eval/acceptance/w7-02.toml` with one gate per test, `required = true`.
Add `docs/architecture/cli.md` and `docs/gates/w7-02.md`.

## Commands and commit

```bash
cargo fmt --all
cargo test -p silyn-cli
cargo clippy -p silyn-cli --all-targets -- -D warnings
cargo run -p xtask -- verify-acceptance
```

Commit once on `wave7-cli` with
`feat(cli): complete scriptable commands and reporting`.
