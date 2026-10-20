# Agent-operated delivery

This directory holds the task packages used by implementation agents and the
merge log produced by the integrator.

## Roles

| Role | Agent | May write | May merge |
|---|---|---|---|
| Architect | `team/architect` | ADRs, contracts, schemas, task packages | no |
| Implementer | `team/backend` | files listed in one task package | no |
| Tester | `team/tester` | `tests/`, acceptance cases | no |
| Reviewer | `team/reviewer` | nothing (read-only) | no |
| Integrator | `team/integrator` | shared files, conflict resolution | yes |

The integrator is the only role allowed to merge to `main` and the only role
allowed to edit `Cargo.toml`, `Cargo.lock`, `migrations/`, `eval/suites/`,
`eval/acceptance/`, `.github/` and `docs/architecture/`.

## Worktrees

- Branch and worktree name: `wave<N>/<slug>`.
- At most **3** worktrees at a time.
- One implementer per crate at a time; cross-crate work is split.
- Delete the worktree and branch after merge.

## Task package format

````markdown
---
id: W1-01
wave: 1
depends_on: []
owns:
  - crates/silyn-storage/src/**
reads:
  - crates/silyn-core/src/**
forbids:
  - migrations/**
  - crates/silyn-core/**
contracts: [EventEnvelope v1, RunStatus v1]
acceptance_registry:
  - storage.migrations_apply
  - storage.event_seq_conflict
commands:
  - cargo test -p silyn-storage
  - cargo clippy -p silyn-storage --all-targets -- -D warnings
---
````

The body states goal, non-goals, failure semantics and acceptance. The
implementer prompt points only at this file; requirements are not restated in
chat.

## Gates before merge

```
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace --release
cargo run -p xtask -- verify-acceptance
cargo run -p xtask -- verify-schemas
cargo run -p xtask -- verify-migrations
cargo run -p xtask -- verify-arch
cargo run -p xtask -- verify-eval-integrity
cargo run -p xtask -- verify-docs
```

All three CI platforms must pass. A red matrix stops the next wave.

## Anti-gaming rules

- Registered acceptance tests may not be removed, renamed or `#[ignore]`d.
  Such a change needs a dedicated integrator commit with a reason.
- Evaluation suites, oracles and thresholds are read-only for implementers.
- Do not derive tests from the implementation. The tester writes them from the
  task package first.
- Do not hide error paths with `unwrap` or blanket `expect`.
- Do not bypass the tool broker, capability checks or audit writes.

## Commit and language

- Small commits, one concern each, imperative subject.
- Artifacts in this repository are English: code, comments, docs, commit
  messages, task packages. Conversation with the operator may be Chinese.
- Reference the task id in the commit body.
