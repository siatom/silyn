# W7-02a: CLI reporting, JSON coverage and exit codes

```yaml
id: W7-02a
wave: 7
depends_on: [W3-03, W4-03, W5-03]
owns:
  - crates/silyn-cli/src/main.rs
  - crates/silyn-cli/tests/report.rs
  - crates/silyn-cli/tests/json_surface.rs
  - docs/architecture/cli.md
  - eval/acceptance/w7-02a.toml
  - docs/gates/w7-02a.md
reads:
  - docs/agents/tasks/W7-02-cli-complete.md
  - crates/silyn-app/src/**
  - crates/silyn-cli/src/main.rs
forbids:
  - migrations/**
  - crates/silyn-core/**
  - crates/silyn-storage/**
  - crates/silyn-runtime/**
  - crates/silyn-growth/**
  - crates/silyn-app/**
  - crates/silyn-api/**
  - eval/suites/**
  - Cargo.toml
contracts: [CLI surface v1, exit codes v1, JSON envelope v1]
acceptance_registry:
  - cli.report_run_json
  - cli.report_run_markdown
  - cli.report_experiment_json
  - cli.report_experiment_markdown
  - cli.json_surface_every_read_command
  - cli.report_no_secrets
commands:
  - cargo test -p silyn-cli
  - cargo clippy -p silyn-cli --all-targets -- -D warnings
  - cargo run -p xtask -- verify-acceptance
```

## Goal

Complete the scriptable CLI reporting surface without the HTTP server. `serve`
is W7-02b after W7-01 lands.

## Commands

```text
silyn report run <run-id> [--format json|markdown] [--out <path>] [--json]
silyn report experiment <report.json> [--stats <stats.json>] [--format json|markdown] [--out <path>] [--json]
```

- `report run`: bundle the run record, events, tool ledger, outcome, token/cost
  fields and memory references. Markdown is rendered from the same JSON object,
  never a separate source of numbers. If no `--out`, write to stdout.
- `report experiment`: read an `EvalReport` plus its optional `.stats.json`
  companion, render a self-contained JSON bundle and Markdown with model id,
  suite/digests, arms, comparisons/CI, thresholds, verdict and reason.
- `--json` is one document on stdout; errors on stderr as
  `{"error":{"class","message"}}`. No secret/key-shaped text.
- Exit codes stay 0/2/3/4/5; unknown run/report path exits 5.

## Docs

`docs/architecture/cli.md`: command table, JSON shapes, exit codes, `serve`
deferred to W7-02b, examples for run/report/memory/evolution.

## Tests

- seed an offline run in a temp home; `report run --json` contains run id,
  status, events, tool calls, outcome, memory refs; Markdown contains the same
  numbers.
- `report experiment` with the committed W6 report/stats returns the model,
  suite, arm counts, C-A CI and verdict; Markdown mirrors them.
- every read command supports `--json` and emits valid JSON (runs list/show/
  events/tools, memory list/search, experience list, evolution list/show,
  report run/experiment).
- no output contains the `.env` key or a key-shaped string.
- unknown ids and missing files exit 5.

Add `eval/acceptance/w7-02a.toml` (required=true) and `docs/gates/w7-02a.md`.

## Commands and commit

```bash
cargo fmt --all
cargo test -p silyn-cli
cargo clippy -p silyn-cli --all-targets -- -D warnings
cargo run -p xtask -- verify-acceptance
```

Commit once on `wave7-cli` with
`feat(cli): add run and experiment reports`.
