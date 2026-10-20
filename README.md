# Silyn

Silyn is a self-evolving AI agent. This repository contains the engineering
implementation of the design described in `docs/architecture/`.

**Status: W0 architecture baseline.** The workspace, protocol contracts, event
model, database schema and automated gates exist. The runtime, memory, learning
and evolution loops land in W1–W6.

## Quick start

```bash
cargo test --workspace
cargo run -p silyn-cli -- doctor
cargo run -p silyn-cli -- --json config
```

Local configuration lives in `.env` (ignored). Copy `.env.example` and set
`DEEPSEEK_API_KEY` only when running a real-model experiment. Pull-request CI
never calls a model; it uses offline fixtures.

## Layout

```
crates/silyn-core      domain types, state machines, events, protocol traits
crates/silyn-storage   SQLite, migrations, repositories (W1)
crates/silyn-llm       OpenAI-compatible gateway (W2)
crates/silyn-runtime   run loop, budgets, checkpoints, tool broker (W3)
crates/silyn-growth    memory, experience, reflection, evolution (W4-W5)
crates/silyn-app       shared service layer for CLI and HTTP
crates/silyn-cli       command line interface
xtask                  repository gates
migrations/            append-only SQLite migrations
schemas/               versioned JSON Schema contracts
eval/                  suites, acceptance registry, reports
docs/architecture/     architecture and ADRs
docs/agents/           task packages and merge log for agent-operated delivery
```

## Gates

`cargo run -p xtask -- <command>` implements the automated gates:

- `verify-acceptance` — registered acceptance tests exist and are executable
- `verify-migrations` — merged migrations are immutable
- `verify-schemas` — schema documents are well formed
- `verify-arch` — crate dependencies follow the allowed directions
- `verify-eval-integrity` — sealed evaluation suites are unchanged

## Design boundaries

The stable kernel — identity and permissions, verification, audit, persistence
protocols and promotion rules — cannot be modified by the agent at runtime.
Candidates may only change policy fields listed in ADR 0003. High-risk
promotions require an operator and are denied by default.

## License

MIT. See `LICENSE`.
