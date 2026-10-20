# W4-03: Memory-aware context, verifier wiring and experience extraction

```yaml
id: W4-03
wave: 4
depends_on: [W3-01, W3-02, W3-03, W4-01, W4-02]
owns:
  - crates/silyn-runtime/src/context.rs
  - crates/silyn-runtime/src/run.rs
  - crates/silyn-runtime/src/lib.rs
  - crates/silyn-runtime/Cargo.toml
  - crates/silyn-app/src/learning.rs
  - crates/silyn-app/src/runtime_service.rs
  - crates/silyn-app/src/lib.rs
  - crates/silyn-cli/src/main.rs
  - crates/silyn-cli/tests/memory.rs
  - eval/acceptance/w4-03.toml
  - docs/gates/w4-03.md
reads:
  - crates/silyn-growth/src/memory.rs
  - crates/silyn-growth/src/verifier.rs
  - crates/silyn-growth/src/experience.rs
  - crates/silyn-runtime/src/**
  - crates/silyn-app/src/**
  - crates/silyn-cli/src/main.rs
forbids:
  - migrations/**
  - crates/silyn-core/**
  - crates/silyn-storage/**
  - eval/suites/**
  - docs/architecture/**
  - Cargo.toml
contracts: [RunOptions memory context v1, RunOutcome learning v1]
acceptance_registry:
  - context.memory_injected_with_ids
  - context.memory_scope_isolation
  - context.no_memory_unchanged
  - learning.failed_run_creates_provisional
  - learning.unknown_creates_nothing
  - learning.cross_run_deduplicates
  - cli.memory_add_and_search
commands:
  - cargo test -p silyn-runtime -p silyn-app -p silyn-cli
  - cargo clippy -p silyn-runtime -p silyn-app -p silyn-cli --all-targets -- -D warnings
  - cargo run -p xtask -- verify-acceptance
```

## Goal

Close the L1 loop: retrieve agent-scoped memories before a run, inject them as
untrusted context with stable memory IDs, use the W4-02 verifier, and extract
provisional experiences after a failed run without ever treating `Unknown` as
success.

## Non-goals

- No consolidation/decay job, embeddings or reflection.
- No policy candidates or promotion.
- No memory sharing across agents/users.

## Runtime changes

Add to `crates/silyn-runtime/src/context.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySnippet {
    pub memory_id: String,
    pub summary: String,
    pub content: String,
    pub provenance: String,
    pub confidence_milli: u32, // 0..=1000 to keep Eq
}

pub struct ContextBuilder;
impl ContextBuilder {
    pub fn system_prompt(objective: &str, memories: &[MemorySnippet]) -> String;
}
```

- The system prompt keeps the action protocol and appends a block:
  `MEMORY (untrusted context, cite by id): [id] summary — content`.
- Tool/memory text is explicitly untrusted and cannot grant capabilities.
- Capabilities still come only from `RunOptions.capabilities`.
- Add `memories: Vec<MemorySnippet>` to `RunOptions` and `agent_id:
  Option<AgentId>`. Existing callers get empty/None via `Default`.
- Store injected memory IDs in the checkpoint `RunState` and expose them in
  `RunOutcome.memory_refs` (new field). Do not claim memory caused success.
- Token budget includes the memory block; truncate memories deterministically
  when the block exceeds 25% of the input budget. Record truncation count.

## App/CLI

`App::run`/`resume`:

- Resolve the agent from task → session → agent (or from `RunRequest.agent_id`).
- Query `MemoryStore::search` with `MemoryScopeFilter { owner_user_id,
  agent_id, scopes: [Agent, Session], types: [], include_expired: false }`
  and the objective, limit 8.
- Map hits to `MemorySnippet`.
- After the run, call `ExperienceStore::extract`:
  - `Completed` + `Some(Unknown)` -> no experience;
  - `Completed` + `Some(Success)` -> success experience (provisional);
  - `Failed` -> map `RunOutcome.reason` to a code (`unit_mismatch`,
    `missing_values`, `duplicate_rows`, `wrong_count`, `tool_error`, other);
  - `Cancelled` -> no experience.
- Use `CompositeVerifier` when the caller supplies an expected answer
  (`silyn run --expect-json <json>`); otherwise keep the W3 `AnswerVerifier`
  that only returns `Unknown`.
- CLI additions (minimal, W7 expands):
  - `silyn memory add --agent <id> --summary <text> --content <text>
    [--scope agent|session] [--provenance user_statement|external_evidence|model_inference|tool_output]`
  - `silyn memory search --agent <id> <query> [--limit N] [--json]`
  - `silyn memory delete --agent <id> <memory-id>` (soft delete)
  - `silyn experience list --agent <id> [--json]`
- All commands use `--json` where data is returned and never print secrets.

## Tests

- memory injection: seed two memories, run offline with a mock provider, assert
  the request system prompt contains both IDs/summaries and the untrusted
  marker; `RunOutcome.memory_refs` matches.
- isolation: agent A's memory is absent from agent B's request.
- no memory: request is unchanged from W3 aside from `memories` empty.
- learning: a failed run with a `unit_mismatch` reason creates one provisional
  experience; `Unknown`/cancelled creates none; two failures deduplicate.
- CLI: `memory add --json`, `memory search --json`, `memory delete --json`
  round-trip in a temp home; `experience list` shows the provisional record.
- cross-agent CLI search returns empty, not another agent's memory.

Add `eval/acceptance/w4-03.toml` with one gate per test, `required = true`.
Add `docs/gates/w4-03.md`.

## Commands and commit

```bash
cargo fmt --all
cargo test -p silyn-runtime -p silyn-app -p silyn-cli
cargo clippy -p silyn-runtime -p silyn-app -p silyn-cli --all-targets -- -D warnings
cargo run -p xtask -- verify-acceptance
```

Commit once on `wave4-integration` with
`feat(runtime): inject agent memories and extract provisional experiences`.
