# ADR 0001: Scope, invariants and evolution layers

- Status: Accepted
- Date: 2026-10-20

## Context

Silyn must be able to improve its own behavior without becoming able to widen
its own permissions. The MVP has a single user, a single agent and a local
process. Multi-user, multi-agent and GUI work are deferred.

## Decision

MVP scope:

- local, single-process, single-agent, single-user;
- CLI first; the HTTP API is a thin adapter added after the evolution loop works;
- SQLite for structured state; no external vector database;
- OpenAI-compatible model access only; no fine-tuning.

The **stable kernel** cannot be modified by any agent-generated candidate:

- identity and permission checks;
- outcome verification rules and evaluation harness;
- audit and event persistence protocol;
- promotion rules, thresholds and approver configuration;
- executable code and tool capability grants.

Evolution layers:

| Layer | May change | Promotion |
|---|---|---|
| L1 | memory content (facts, episodes, experiences) | automatic, with provenance |
| L2 | prompt fragments, planning rules, retrieval parameters | evaluation gate; low-risk auto, otherwise operator |
| L3 | tools, workflows, code, plugins | sandbox only; never auto-promoted in the MVP |

## Consequences

- The runtime can rely on a fixed kernel.
- Any change to kernel boundaries requires a new ADR and a schema version bump.
- L3 experiments are out of scope until the L2 loop is proven.

## Alternatives considered

- Let candidates edit any policy field: rejected, it makes self-authorization
  possible and evaluation becomes meaningless.
- Multi-agent orchestration in the MVP: rejected, it adds coordination cost
  before the core hypothesis is validated.
