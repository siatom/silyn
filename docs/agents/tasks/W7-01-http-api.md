# W7-01: Local HTTP API and SSE event stream

```yaml
id: W7-01
wave: 7
depends_on: [W3-03, W4-03, W5-03]
owns:
  - crates/silyn-api/**
  - crates/silyn-app/src/api_support.rs
  - crates/silyn-app/src/lib.rs
  - crates/silyn-app/Cargo.toml
  - eval/acceptance/w7-01.toml
  - docs/gates/w7-01.md
reads:
  - crates/silyn-app/src/**
  - crates/silyn-core/src/{error.rs,state.rs,ids.rs}
  - docs.local/Silyn_Self_Evolving_Agent_Technical_Design_v0.1.md (API section)
forbids:
  - migrations/**
  - crates/silyn-core/**
  - crates/silyn-storage/**
  - crates/silyn-runtime/**
  - crates/silyn-growth/**
  - crates/silyn-cli/**
  - eval/suites/**
  - Cargo.toml
contracts: [HTTP API v1, SSE RunEvent v1]
acceptance_registry:
  - api.health
  - api.agent_session_task_round_trip
  - api.run_create_and_get
  - api.events_sse
  - api.cancel
  - api.memory_search
  - api.error_mapping
  - api.local_bind_only
commands:
  - cargo test -p silyn-api
  - cargo clippy -p silyn-api --all-targets -- -D warnings
  - cargo run -p xtask -- verify-acceptance
```

## Goal

Add a thin local HTTP surface over `silyn-app` for the W7 productization gate.
The server binds to `127.0.0.1` by default, is single-tenant local, and shares
all business rules with the CLI.

## Routes

| Method | Path | Behavior |
|---|---|---|
| GET | `/health` | `{status:"ok", version}` |
| POST | `/v1/agents` | create agent `{owner_id}` |
| GET | `/v1/agents` | list agents |
| POST | `/v1/agents/{id}/sessions` | create session |
| POST | `/v1/tasks` | `{session_id, objective, budget?}` |
| POST | `/v1/tasks/{id}/run` | create + execute a run, return `RunOutcome` |
| POST | `/v1/runs/{id}/resume` | resume |
| GET | `/v1/runs/{id}` | run record |
| GET | `/v1/runs/{id}/events` | SSE stream of `EventEnvelope` (replay then live) |
| POST | `/v1/runs/{id}/cancel` | write cancel request |
| GET | `/v1/agents/{id}/memories?q=&limit=` | memory search |
| POST | `/v1/evolution/candidates` | submit patch |
| POST | `/v1/evolution/candidates/{id}/evaluate` | attach report |
| POST | `/v1/evolution/candidates/{id}/promote` | promote |
| POST | `/v1/evolution/promotions/{id}/rollback` | rollback |

- JSON bodies use `silyn-core`/app types; no duplicate DTOs unless necessary.
- Error mapping: `Permanent`/`Schema` -> 400 or 404 (unknown id), `PolicyDenied`
  -> 403, `Transient` -> 503, `Indeterminate` -> 409, storage -> 500. Never
  return a stack trace or secret.
- Optional `SILYN_API_TOKEN`: when set, require `Authorization: Bearer
  <token>` for all `/v1/*` routes; `health` stays public. Constant-time compare.
- Bind address from `SILYN_API_ADDR` (default `127.0.0.1:7788`); reject a
  non-loopback bind unless `SILYN_API_ALLOW_NON_LOOPBACK=1`.
- SSE: `GET /v1/runs/{id}/events` sends existing events first, then polls the
  journal for new events (250 ms) until the run is terminal or the client
  disconnects. `id:` is the event sequence; `event:` is the event type.
- No CORS by default; add only an explicit allowlist for local tools if needed.
- Graceful shutdown on Ctrl-C.

## Tests

Use `tower::ServiceExt::oneshot` with a temp `App` and the offline mock provider;
no network or ports:

- health returns ok/version.
- agent -> session -> task -> run -> get round trip.
- SSE returns the terminal events for an offline run and closes.
- cancel returns the path/already_requested and the runtime observes it in a
  direct service test (or returns 404 for unknown).
- memory search is agent-scoped.
- error mapping for unknown id, policy denied, indeterminate.
- non-loopback bind rejected unless explicitly allowed; token middleware
  rejects missing/wrong tokens.

Add `eval/acceptance/w7-01.toml` with one gate per test, `required = true`.
Add `docs/gates/w7-01.md`.

## Commands and commit

```bash
cargo fmt --all
cargo test -p silyn-api
cargo clippy -p silyn-api --all-targets -- -D warnings
cargo run -p xtask -- verify-acceptance
```

Commit once on `wave7-api` with
`feat(api): add local http and sse surface`.
