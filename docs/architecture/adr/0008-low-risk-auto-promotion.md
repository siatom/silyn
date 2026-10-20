# ADR 0008: Low-risk automatic promotion

- Status: Accepted
- Date: 2026-10-20

## Context

The design requires human approval for L2 changes, but Silyn is operated without
a human in the loop. Refusing every promotion would make the minimal acceptance
story impossible; allowing everything would violate the "no self-authorization"
invariant.

## Decision

A candidate may be promoted automatically only when **all** conditions hold:

- `risk_class = low`;
- every patch operation targets `/planner/prompt_fragments`, `/planner/rules` or
  `/context/retrieval`, within schema bounds;
- no change to tools, capabilities, verification, budgets, thresholds, approval
  configuration or code;
- the evaluation report verdict is `promote` and meets the pre-registered
  thresholds, including zero unauthorized actions;
- every `evidence_run_id` exists, is terminal, and has verified evidence;
- the rollback drill has been executed successfully on a copy of the active
  policy;
- the candidate was not produced by a process that could read the sealed test set.

Otherwise the candidate moves to `AwaitingApproval`. With no operator present it
remains there until an explicit timeout, then becomes `Rejected`. The default is
deny, never "assume approved".

Operator approval is a separate local control path (`silyn evolution approve`)
that records the operator identity and evidence hash in the audit log. The agent
runtime cannot invoke it: it has no shell capability and no write access to the
audit or promotion tables.

## Consequences

- The minimal acceptance story runs without a human.
- A compromised reflection process still cannot grant itself capabilities or
  rewrite its grader.
- Promotion is reversible: rollback restores the previous immutable version and
  is audited.

## Alternatives considered

- All promotions require an operator: rejected for this delivery model; it blocks
  the end-to-end experiment.
- Promote everything with audit only: rejected; it defeats the evaluation gate.
