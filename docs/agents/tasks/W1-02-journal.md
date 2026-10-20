---
id: W1-02
wave: 1
depends_on: [W1-01]
owns:
  - crates/silyn-storage/src/journal.rs
  - crates/silyn-storage/src/checkpoint.rs
  - crates/silyn-storage/src/outbox.rs
  - crates/silyn-storage/src/ledger.rs
  - crates/silyn-storage/tests/journal.rs
reads:
  - crates/silyn-core/src/event.rs
  - crates/silyn-core/src/state.rs
  - migrations/0002_journal.sql
forbids:
  - migrations/**
  - crates/silyn-core/**
contracts: [EventEnvelope v1]
acceptance_registry:
  - storage.event_seq_conflict
  - storage.state_and_event_same_transaction
  - storage.outbox_idempotent
  - storage.ledger_blocks_duplicate_call
commands:
  - cargo test -p silyn-storage
  - cargo clippy -p silyn-storage --all-targets -- -D warnings
---

# W1-02 Journal, checkpoints, outbox and ledger

## Goal

Implement append-only event storage with optimistic concurrency, run checkpoints,
the outbox and the tool-call idempotency ledger.

## Requirements

- Append `(aggregate_id, seq)`; a duplicate seq returns a retryable conflict and
  never overwrites.
- Status update and event append share one transaction.
- Checkpoint stores state reference, schema version and checksum; mismatch fails.
- Outbox insert is part of the same transaction as the event; consumption is
  idempotent on `(consumer, event_id)`.
- Ledger enforces `(run_id, step_id, tool_call_id)` uniqueness and exposes an
  `Indeterminate` transition for unknown side-effect outcomes.

## Non-goals

- Runtime retry policy (W3).
- Experience extraction consumers (W4).

## Acceptance

- Concurrent append with the same seq: one succeeds, one conflicts.
- Rolled-back status update leaves neither state change nor event.
- Consuming the same outbox row twice has one effect.
- A duplicate ledger insert is rejected.
