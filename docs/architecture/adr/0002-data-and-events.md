# ADR 0002: Data model, events and recovery

- Status: Accepted
- Date: 2026-10-20

## Context

Runs must survive process restarts without repeating external side effects, and
the evolution loop needs an auditable trail from evidence to promotion.

## Decision

- SQLite in WAL mode, database on a local disk only.
- Migrations are append-only. Merged migration files are immutable and their
  sha256 is recorded in `migrations/checksums.txt`; `verify-migrations` enforces it.
- Core entities: `agents`, `sessions`, `tasks`, `runs`, `run_steps`, plus the
  journal, checkpoint, outbox, tool ledger and audit tables.
- Events use a versioned envelope:
  `event_id, aggregate_id, seq, type, schema_version, occurred_at,
  correlation_id, payload_ref`.
- `(aggregate_id, seq)` is unique. Appending with a stale `seq` fails and the
  caller re-reads; there is no last-write-wins.
- State updates and their event insert happen in one transaction.
- Checkpoints store `state_ref`, `schema_version` and `checksum`. A checkpoint
  with an unknown schema version fails loudly instead of being guessed.
- Async derivations (experience extraction, indexing, reflection) consume the
  outbox and are idempotent on `(consumer, event_id)`.
- Idempotency key is `run_id + step_id + tool_call_id`. When a non-idempotent
  call has an unknown outcome, the step enters `Indeterminate` and requires
  explicit resolution.

We do **not** fully event-source every object. Events record business
transitions only; current state lives in tables.

## Consequences

- Crash recovery is replay-from-checkpoint plus ledger checks.
- Deletion is soft-delete plus an explicit purge job, so audit history stays intact.
- Any schema change needs a new migration file and a checksum update.

## Alternatives considered

- Full event sourcing: rejected, it adds replay complexity without a proven need.
- Ad-hoc per-module tables: rejected, it breaks cross-module audit and recovery.
