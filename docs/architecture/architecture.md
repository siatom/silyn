# Silyn architecture

- Status: W0 baseline
- Date: 2026-10-20

## Purpose

Silyn is a self-evolving agent: it accumulates experience, proposes changes to
its own policies, evaluates them against a sealed task distribution, and promotes
only what demonstrably improves outcomes without violating risk constraints.

The MVP proves one claim: under a fixed model and tool set, experience leads to a
reproducible improvement on unseen tasks, with a full evidence trail and a working
rollback.

## Objects

```
User -> Agent -> Session -> Task -> Run -> Step -> Event
                          \-> Memory
                          \-> Experience -> Candidate -> Evaluation -> Promotion
```

- **Agent** has a persistent identity and its own memory space.
- **Session** is a conversation context.
- **Task** is an objective with constraints.
- **Run** is one attempt; retries create new runs.
- **Step** is one model or tool operation.
- **Event** is an append-only business transition.
- **Candidate** is a proposed policy change with evidence, independent of any
  session or task.
- **Promotion** activates an immutable policy version and is auditable.

## Modules

| Module | Crate | Responsibility |
|---|---|---|
| Domain kernel | `silyn-core` | IDs, state machines, errors, events, protocol traits |
| Storage | `silyn-storage` | SQLite, migrations, event journal, checkpoints, outbox |
| Model gateway | `silyn-llm` | OpenAI-compatible adapter, capability probe, cost accounting |
| Runtime | `silyn-runtime` | run loop, budgets, cancellation, tool broker, context builder |
| Growth | `silyn-growth` | memory, experience, reflection, evaluation, promotion |
| App | `silyn-app` | service layer shared by CLI and HTTP |
| CLI | `silyn-cli` | local interaction and scriptable automation |

Allowed dependency direction:

```
core <- storage <- runtime <- app <- cli
core <- llm     <- runtime
core <- storage <- growth  <- app
```

`verify-arch` enforces this.

## Fast loop (per task)

1. Bind agent, session, policy version, memory snapshot, model and tool versions.
2. Build context: policy, retrieved memory with source IDs, tool descriptions.
3. Plan a step; check step, token, cost and wall-clock budgets.
4. Execute through the tool broker with capability and path checks.
5. Emit events and checkpoint; resume never repeats a known side effect.
6. Verify the outcome with independent evidence; `unknown` is not success.
7. Extract experience as a *provisional* memory with provenance.

## Slow loop (across tasks)

1. Aggregate experiences and cluster failures.
2. Reflect on patterns and form a testable hypothesis.
3. Emit a `PolicyPatch` with evidence run IDs.
4. Static-check the patch against the allowlist and schema bounds.
5. Evaluate A/B/C on train/dev, with the sealed test set held back.
6. Apply the promotion gate; low-risk candidates may auto-promote.
7. Monitor and roll back by moving the active policy pointer.

## Boundaries

- Model output may only propose actions; the broker authorizes them.
- Candidates cannot change tools, verification, budgets, thresholds or audit.
- The evaluation runner is a separate process with a temporary database.
- Retrieved and tool text is untrusted data, never instructions.

## Repository layout

See `README.md`. Design decisions are recorded as ADRs in `docs/architecture/adr/`.
Delivery tasks and merge logs live in `docs/agents/`.
