---
id: W2-03
wave: 2
depends_on: [W1-01, W1-02]
owns:
  - crates/silyn-app/src/**
  - crates/silyn-cli/src/**
  - crates/silyn-cli/tests/**
reads:
  - crates/silyn-core/src/**
  - docs/architecture/adr/0001-scope-and-invariants.md
forbids:
  - crates/silyn-core/**
  - migrations/**
  - eval/suites/**
contracts: [CLI surface v1]
acceptance_registry:
  - cli.json_output_is_stable
  - cli.exit_codes_are_stable
commands:
  - cargo test -p silyn-cli
  - cargo clippy -p silyn-cli --all-targets -- -D warnings
---

# W2-03 CLI and service layer

## Goal

Put a thin, scriptable CLI over `silyn-app`: `init`, `agent create`, `run`,
`runs show|events|cancel|resume`, `config`, `doctor`.

## Requirements

- Every command supports `--json` with a stable, documented shape.
- Exit codes: 0 success, 2 usage error, 3 policy denied, 4 transient failure,
  5 permanent failure. Document them in `docs/architecture/cli.md`.
- `run` accepts a task file and a budget; `--resume` continues the last run.
- `doctor` never prints secret values, only presence.
- No business logic in the CLI; it delegates to `silyn-app`.

## Non-goals

- HTTP API, interactive chat UI, TUI.

## Acceptance

- `silyn --json doctor` parses as JSON and contains no key material.
- Usage errors exit 2; policy denial exits 3.
- `runs resume` on a clean database fails with a clear permanent error.
- Commands work on Windows, Linux and macOS.
