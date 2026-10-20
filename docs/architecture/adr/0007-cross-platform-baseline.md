# ADR 0007: Cross-platform baseline

- Status: Accepted
- Date: 2026-10-20

## Context

Silyn targets Windows, macOS and Linux. Paths, process control, filesystem case
sensitivity and line endings differ enough to break a naive implementation.

## Decision

- CI matrix: `windows-latest` today; `ubuntu-latest` and `macos-latest` (arm64)
  are added back before the runtime lands platform-specific process handling.
  The target remains Windows, macOS and Linux.
- All paths use `PathBuf`; no string concatenation. Canonicalization uses `dunce`
  and every filesystem operation is checked against the run root.
- Windows: reject alternate data streams and UNC paths; detect symlink privilege
  before running symlink tests.
- Subprocesses use `tokio::process` with `kill_on_drop`; on Windows a Job Object
  is used so grandchildren are reaped.
- Cancellation uses `tokio::signal::ctrl_c`; no assumption about SIGTERM.
- `.gitattributes` forces LF. Fixtures are generated with LF and tested with CRLF.
- Tests must not depend on case-insensitive filesystems; memory dedupe uses content
  hashes, not filenames.
- SQLite is statically bundled. Startup checks FTS5 availability and fails fast if
  missing. Databases must live on a local disk; network shares are rejected.
- Timestamps are stored as UTC RFC 3339. Number parsing never depends on the
  system locale.
- Secrets come from environment variables. OS keyring is an optional feature and
  is disabled in CI.

## Consequences

- Platform-specific code is isolated and covered by the matrix.
- Any new platform assumption needs a test on all three runners.

## Alternatives considered

- Windows-only first: rejected, it hides portability defects until late.
- Rely on the system SQLite: rejected, FTS5 availability varies by distribution.
