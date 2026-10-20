---
id: W1-01
wave: 1
depends_on: []
owns:
  - crates/silyn-storage/src/**
  - crates/silyn-storage/tests/**
reads:
  - crates/silyn-core/src/**
  - migrations/0001_core.sql
  - docs/architecture/adr/0002-data-and-events.md
forbids:
  - migrations/**
  - crates/silyn-core/**
  - Cargo.toml
contracts: [EventEnvelope v1, TaskStatus v1, RunStatus v1]
acceptance_registry:
  - storage.migrations_apply
  - storage.foreign_keys_enforced
  - storage.run_round_trip
commands:
  - cargo test -p silyn-storage
  - cargo clippy -p silyn-storage --all-targets -- -D warnings
---

# W1-01 Storage core

## Goal

Implement the SQLite layer for the `0001_core` schema: connection setup, migration
runner, FTS5 self-check, and repositories for agents, sessions, tasks, runs and
run steps.

## Requirements

- Enable WAL and `PRAGMA foreign_keys = ON` per connection.
- Apply migrations from the workspace `migrations/` directory in filename order.
- Record applied migrations with checksums; refuse to start if a merged migration
  changed.
- Repository methods take explicit IDs; no ambient current-agent state.
- Timestamps are UTC RFC 3339 strings.
- All writes that change status happen inside a transaction.

## Non-goals

- Event journal, checkpoints, outbox and tool ledger (W1-02).
- Query macros with compile-time database access.

## Acceptance

- Fresh database reaches the latest schema version.
- Foreign key violation is rejected, not silently ignored.
- Agent/session/task/run/step round-trip preserves IDs and status.
- A failed transaction leaves no partial rows.
- FTS5 self-check fails fast when unavailable.
