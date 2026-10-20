# W7-02b: `silyn serve` integration

```yaml
id: W7-02b
wave: 7
depends_on: [W7-01, W7-02a]
owns:
  - crates/silyn-cli/src/main.rs
  - crates/silyn-cli/Cargo.toml
  - crates/silyn-cli/tests/serve.rs
  - docs/architecture/cli.md
  - eval/acceptance/w7-02b.toml
  - docs/gates/w7-02b.md
reads:
  - crates/silyn-api/src/lib.rs
  - docs/agents/tasks/W7-01-http-api.md
  - docs/agents/tasks/W7-02-cli-complete.md
forbids:
  - migrations/**
  - crates/silyn-core/**
  - crates/silyn-storage/**
  - crates/silyn-runtime/**
  - crates/silyn-growth/**
  - crates/silyn-app/**
  - crates/silyn-api/**
  - eval/suites/**
  - Cargo.toml
contracts: [HTTP API v1, CLI serve v1]
acceptance_registry:
  - cli.serve_health
  - cli.serve_auth_env
  - cli.serve_loopback_default
  - cli.serve_graceful_exit
commands:
  - cargo test -p silyn-cli
  - cargo clippy -p silyn-cli --all-targets -- -D warnings
  - cargo run -p xtask -- verify-acceptance
```

## Goal

Wire the W7-01 server into the CLI:

```text
silyn serve [--addr <host:port>] [--token-env <name>] [--json]
```

- Default addr from `SILYN_API_ADDR` or `127.0.0.1:7788`.
- Default token env from `SILYN_API_TOKEN`; `--token-env` names another variable.
- Non-loopback bind requires `SILYN_API_ALLOW_NON_LOOPBACK=1`; otherwise exit 5.
- `--json` prints one object `{addr, token_required}` after the listener is
  bound, then serves until Ctrl-C; errors go to stderr as the usual envelope.
- Reuse `silyn_api::serve`; do not duplicate routing or auth logic.
- Add `silyn-api` as a CLI dependency. No other dependency changes.

## Tests

- start `serve` on `127.0.0.1:0` (ephemeral) in a child task/process, read the
  bound address from JSON, hit `/health` with a plain TcpStream/HTTP request,
  then shut it down; no fixed port.
- token middleware: with a token env set, `/health` is public and `/v1/agents`
  without bearer is 401/403.
- non-loopback addr without the allow flag exits 5 before binding.
- graceful shutdown on Ctrl-C is documented; a unit test may call the server's
  shutdown future directly instead of sending a real signal.

Add `eval/acceptance/w7-02b.toml` (required=true) and `docs/gates/w7-02b.md`.
Update `docs/architecture/cli.md` to remove the "serve deferred" note.

## Commands and commit

```bash
cargo fmt --all
cargo test -p silyn-cli
cargo clippy -p silyn-cli --all-targets -- -D warnings
cargo run -p xtask -- verify-acceptance
cargo run -p xtask -- verify-docs
```

Commit once on `wave7-serve` with `feat(cli): serve the local api`.
