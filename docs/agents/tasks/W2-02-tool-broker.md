---
id: W2-02
wave: 2
depends_on: [W1-01]
owns:
  - crates/silyn-runtime/src/broker/**
  - crates/silyn-runtime/src/tools/**
  - crates/silyn-runtime/tests/tools.rs
reads:
  - crates/silyn-core/src/contracts.rs
  - docs/architecture/adr/0005-trust-boundaries-and-capabilities.md
forbids:
  - crates/silyn-core/**
  - crates/silyn-storage/**
  - Cargo.toml
contracts: [ToolExecutor v1, AuthorizedToolContext v1]
acceptance_registry:
  - tools.capability_denied_by_default
  - tools.path_traversal_rejected
  - tools.audit_written
commands:
  - cargo test -p silyn-runtime
  - cargo clippy -p silyn-runtime --all-targets -- -D warnings
---

# W2-02 Tool broker and read-only tools

## Goal

Implement the capability-gated tool broker and three read-only tools:
`fs.list`, `fs.read`, `data.profile` (plus `report.write` into the run output
directory).

## Requirements

- Deny by default; check capability strings before any filesystem access.
- Canonicalize paths with `dunce`; require `starts_with(run_root)`; reject `..`,
  UNC paths, Windows ADS and symlink escapes.
- Validate arguments against a JSON schema per tool.
- Write an audit record for every allowed and denied call.
- Record idempotency keys in the ledger; repeated calls return the prior result.
- Tool output is data, wrapped as untrusted when injected into context.

## Non-goals

- Shell execution, network tools, MCP.
- Runtime loop (W3).

## Acceptance

- A call without the capability is denied and audited.
- Traversal and symlink escape attempts are rejected on all platforms.
- Repeated calls with the same key do not write twice.
- No code path reads a file outside the run root.
