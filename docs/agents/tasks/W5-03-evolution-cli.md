# W5-03: Evolution CLI and mock end-to-end promotion path

```yaml
id: W5-03
wave: 5
depends_on: [W5-01, W5-02, W4-03]
owns:
  - crates/silyn-app/src/evolution.rs
  - crates/silyn-app/src/lib.rs
  - crates/silyn-app/Cargo.toml
  - crates/silyn-cli/src/main.rs
  - crates/silyn-cli/tests/evolution.rs
  - eval/acceptance/w5-03.toml
  - docs/gates/w5-03.md
reads:
  - crates/silyn-growth/src/{policy,candidate,promotion}.rs
  - crates/silyn-eval/src/**
  - crates/silyn-app/src/**
  - crates/silyn-cli/src/main.rs
forbids:
  - migrations/**
  - crates/silyn-core/**
  - crates/silyn-storage/**
  - crates/silyn-growth/**
  - eval/suites/**
  - Cargo.toml
contracts: [CLI evolution surface v1]
acceptance_registry:
  - cli.evolution_submit_static_checked
  - cli.evolution_reject_forbidden_patch
  - cli.evolution_attach_report
  - cli.evolution_low_risk_promote
  - cli.evolution_rollback
  - cli.evolution_high_risk_awaits
commands:
  - cargo test -p silyn-cli -p silyn-app
  - cargo clippy -p silyn-cli -p silyn-app --all-targets -- -D warnings
  - cargo run -p xtask -- verify-acceptance
```

## Goal

Expose the evolution pipeline through `silyn-app` and the CLI, and prove the
mock golden path: submit a low-risk patch, attach a pre-registered evaluation
report, auto-promote, then roll back with an audit trail.

## CLI surface

```text
silyn evolution list --agent <id> [--json]
silyn evolution show <candidate-id> --agent <id> [--json]
silyn evolution submit --agent <id> --patch <file|-> [--json]
silyn evolution evaluate <candidate-id> --agent <id> --report <eval-report.json>
silyn evolution promote <candidate-id> --agent <id> [--json]
silyn evolution rollback <promotion-id> --agent <id> [--json]
silyn evolution approve <candidate-id> --agent <id> --as <operator> [--json]
```

- `submit`: read `PolicyPatch` JSON, call `CandidateStore::submit`, print the
  candidate id/state; static failures exit 5 with the reason.
- `evaluate`: read an `EvaluationReport` JSON, validate it against the schema
  fields, derive a `PromotionReport`, store an `evaluation_runs` row, move the
  candidate to `Evaluated`; never invent metrics.
- `promote`: call `PromotionStore::decide`; auto-promote low-risk, otherwise
  `AwaitingApproval`. Print decision and versions.
- `rollback`: restore the previous policy version and print both ids.
- `approve`: local operator path; records the operator name, evidence checksum
  and audit row. It must not be callable from the runtime.
- All commands support `--json`; unknown ids exit 5.

## App service

`crates/silyn-app/src/evolution.rs` wraps growth stores, resolves the agent and
active policy, and exposes the same methods. It must not duplicate SQL.

## Tests

Use temp home plus a synthetic policy/version and a hand-written evaluation
report (no network):

- submit valid low-risk patch -> `static_checked`; forbidden `/tools` patch
  exits 5 and creates no candidate.
- evaluate attaches report, candidate `evaluated`.
- promote low-risk -> `promoted`, agent active pointer changes, audit row
  exists; `evolution show --json` reflects it.
- rollback -> pointer restored, promotion has `rolled_back_at`, candidate
  `rolled_back`; second rollback exits 5.
- medium/high risk -> `awaiting_approval`; `approve` moves to `approved` and
  then promotion is explicit.
- unknown ids exit 5.

Add `eval/acceptance/w5-03.toml` with one gate per test, `required = true`.
Add `docs/gates/w5-03.md`.

## Commands and commit

```bash
cargo fmt --all
cargo test -p silyn-cli -p silyn-app
cargo clippy -p silyn-cli -p silyn-app --all-targets -- -D warnings
cargo run -p xtask -- verify-acceptance
```

Commit once on `wave5-cli` with
`feat(cli): expose audited evolution submission and promotion`.
