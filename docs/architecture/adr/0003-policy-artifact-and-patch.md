# ADR 0003: Policy artifact and patch protocol

- Status: Accepted
- Date: 2026-10-20

## Context

Without a concrete policy format, "candidate", "diff", "promotion" and
"rollback" cannot be implemented or validated. The format also defines what an
agent is forbidden to change.

## Decision

A policy artifact is immutable JSON, versioned by `policy_versions.id` and
identified by the sha256 of its canonical serialization:

```json
{
  "schema_version": 1,
  "context": {
    "memory_budget_ratio": 0.35,
    "retrieval": { "bm25_k": 40, "max_evidence": 8 }
  },
  "planner": { "prompt_fragments": [], "rules": [], "stop_conditions": {} },
  "tools": { "allowlist": ["fs.read", "data.profile", "report.write"] },
  "verification": { "checkers": ["csv-stats-oracle"] }
}
```

Candidates submit a `PolicyPatch` (see `schemas/policy_patch.schema.json`):
metadata plus a restricted RFC 6902 operation list.

Mutable by L2 candidates:

- `/planner/prompt_fragments`
- `/planner/rules`
- `/context/retrieval` (bounded integers)

Never mutable by a candidate:

- `/tools/*`, `/verification/*`
- budgets and cost caps
- evaluation suites, thresholds, oracles
- approval configuration and audit settings
- any executable code or new tool registration

Static checks reject an operation whose path is outside the allowlist, whose
value violates the schema bounds, or whose text attempts to grant capabilities.
Promotion activates a new immutable version and moves the agent's
`active_policy_version` pointer. Rollback moves the pointer back; both actions
are audited.

## Consequences

- Diffs are reviewable and machine-checkable.
- A candidate can never widen its own permissions or edit its grader.
- Prompt-only changes still need an evaluation report and a passing gate.

## Alternatives considered

- Free-form prompt rewriting: rejected, undiffable and trivially escalatory.
- Whole-artifact replacement: rejected, it hides intent and makes regression
  attribution impossible.
