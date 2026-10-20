# Threat model (MVP)

- Status: W0 baseline
- Date: 2026-10-20

## Assets

1. User data and files inside the run workspace.
2. Model credentials and provider budget.
3. The sealed evaluation test set and its oracle.
4. Policy versions, audit log and promotion history.
5. The integrity of the "improvement" claim itself.

## Actors and trust boundaries

| Actor | Trust | Can influence |
|---|---|---|
| User | high (current task only) | objective, constraints, feedback |
| Model provider | untrusted output | plans, tool arguments, candidate text |
| Tool output / retrieved text | untrusted | context content |
| Memory records | medium, provenance-ranked | future plans |
| Candidate author (agent) | untrusted | policy patch within allowlist |
| Evaluation runner | trusted, isolated | metrics and verdict |
| Kernel | trusted, immutable | permissions, thresholds, audit |

## Threats and mitigations

| Threat | Vector | Mitigation | Verification |
|---|---|---|---|
| Prompt injection grants a capability | tool output or memory text | broker checks run capabilities, not prompt text; untrusted content is labeled | security tests in W2 |
| Candidate widens its own permissions | patch to `/tools` or `/verification` | path allowlist + schema bounds + static check | `verify-acceptance`, W5 tests |
| Candidate edits its grader | patch or config change | evaluator and thresholds are read-only and hash-checked | `verify-eval-integrity` |
| Sealed test leakage | candidate process reads test fixtures | runner passes only task inputs; path never exposed; hashes recorded | W5 process-level test |
| Path traversal / symlink escape | tool arguments | canonicalize, root prefix check, reject ADS/UNC | W2 security tests |
| Secret leakage | logs, snapshots, error messages | env-only secrets, redaction, no keys in DB | secret scan + review |
| Duplicate side effects after crash | resume replays a tool call | idempotency ledger, `Indeterminate` state | W3 crash tests |
| Evaluation contamination | tuning on test results | sealed split, pre-registered thresholds, invalid-run marking | experiment report review |
| Model drift | provider updates the model | record requested and reported model id; periodic baseline rerun | W6 report |
| Audit tampering | update/delete rows | append-only audit table; promotions reference evidence hashes | migration + W5 tests |

## Residual risks

- A sufficiently persuasive model may produce a policy that passes the current
  suite but generalizes poorly. Mitigation: sealed test, regression suite, and
  published negative results.
- OS-level sandboxing is not implemented in the MVP; isolation relies on
  capability restrictions and a separate process/temp directory.
- Provider nondeterminism limits exact reproducibility; conclusions use repeated
  runs and confidence intervals.
