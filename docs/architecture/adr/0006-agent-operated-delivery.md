# ADR 0006: Agent-operated delivery

- Status: Accepted
- Date: 2026-10-20

## Context

Development is performed by AI agents in parallel git worktrees with no human
reviewer. Parallel agents fail in predictable ways: merge conflicts, duplicated
abstractions, and tests weakened until they pass. The delivery process must make
those failures visible and block them automatically.

## Decision

Roles:

- **architect** writes ADRs, contracts, schemas and task packages; no business code;
- **implementer** owns one worktree and only the files listed in its task package;
- **tester** writes acceptance tests independently, before implementation;
- **reviewer** is read-only and may block a merge;
- **integrator** is the only role that merges to `main` and the only role that
  edits shared files (`Cargo.toml`, `Cargo.lock`, `migrations/`,
  `eval/suites/`, `eval/acceptance/`, `.github/`, `docs/architecture/`).

Parallelism is capped at **3** worktrees. More work means another wave, not more
concurrency.

Contract freeze precedes fan-out: domain types, trait signatures, JSON schemas,
the first two migrations, the CLI surface and the acceptance registry land before
parallel implementation starts.

Gates replace review. Every merge must pass format, clippy with `-D warnings`,
tests, release build, acceptance registry, schema, migration, architecture and
eval-integrity checks on Windows, Linux and macOS.

Anti-gaming rules:

- registered acceptance tests may not be deleted, renamed or `#[ignore]`d without
  a dedicated integrator commit;
- evaluation suites and thresholds are read-only for implementers and are
  hash-checked;
- the reviewer looks for tests derived from the implementation, `unwrap` hiding
  error paths, and code bypassing the tool broker;
- mutation testing runs weekly, not per pull request.

Merge order follows the dependency DAG. The integrator runs the full gate after
each merge, never batches merges.

## Consequences

- File ownership and contract freeze are mandatory, not advisory.
- A red matrix stops the next wave.
- Uncommitted or partially merged work is never treated as a baseline.

## Alternatives considered

- One agent per milestone: simpler, but serializes weeks of work.
- Merge all branches at the end: rejected, conflict cost grows superlinearly.
