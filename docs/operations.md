# Operations

- Status: W7 operator documentation
- Applies to: the local, single-user MVP
- Last reviewed: 2026-10-21

This is the runbook for a machine that hosts Silyn. It covers the data
directory, health checks, backup and restore, retention, policy rollback, crash
recovery and the evidence trail. Commands that are not implemented in the
current build are marked **planned** with the wave that delivers them; do not
assume a planned command exists.

## Local-only assumptions

- One operator, one local machine, one Silyn process writing a home directory.
- `$SILYN_HOME` (default `.silyn`) is on a **local disk**. SQLite runs in WAL
  mode with foreign keys and a 5 second busy timeout. Network shares, synced
  folders and removable media are unsupported: locking and durability semantics
  differ and can corrupt the database.
- The database is `$SILYN_HOME/silyn.db`, with `silyn.db-wal` and
  `silyn.db-shm` while it is open. Run artifacts and reports live next to it;
  the evaluation worker uses its own temporary directory and database.
- Cross-process control is limited to the cancel file in `$SILYN_HOME/run/`.
- No daemon, no remote API and no multi-tenant isolation exists in this build.
  W7-01 adds a loopback-only HTTP API; it remains single-tenant.
- Backups are the operator's responsibility. Nothing leaves the machine
  automatically.

## Data-directory layout

```text
$SILYN_HOME/
  silyn.db          SQLite database (agents, sessions, tasks, runs, journal,
                    checkpoints, ledger, audit, memory, experiences)
  silyn.db-wal      write-ahead log while a process is connected
  silyn.db-shm      shared-memory index while a process is connected
  run/              cross-process cancel files: <run-id>.cancel
  workspace/        run working directory (planned, W3-03)
  output/           reports written by the `report.write` tool
```

`silyn init` creates the directory and database and applies migrations. Every
command opens the same home; there is no separate "server" to start.

## Health checks

```bash
cargo run --quiet -p silyn-cli -- --json doctor
cargo run --quiet -p silyn-cli -- --json config
cargo run --quiet -p silyn-cli -- runs list
```

`doctor` prints the version, platform, home, database path, configured model and
whether the key variable is present. It never prints the key. A healthy install
reports `api_key_present: true` for real-model work; `false` is fine for
`--offline`.

Repository-level checks, run from a checkout:

```bash
cargo run -p xtask -- verify-migrations
cargo run -p xtask -- verify-schemas
cargo run -p xtask -- verify-arch
cargo run -p xtask -- verify-eval-integrity
cargo run -p xtask -- verify-docs
```

## Backup

Use SQLite's online backup while Silyn is **not** writing; do not copy a live
database file with a generic file copy. The WAL may contain committed
transactions that are not in `silyn.db` yet.

Preferred procedure:

1. Stop every Silyn process (CLI run, API server, evaluation worker).
2. Take a consistent copy. Either:
   - `sqlite3 "$SILYN_HOME/silyn.db" ".backup 'backup/silyn-$(date +%Y%m%d).db'"`,
     or
   - with all processes stopped, copy `silyn.db` **after** the WAL has been
     checkpointed. The simplest safe sequence is to open the database once
     (`silyn doctor` does) and let SQLite close it, then copy `silyn.db` only.
     Do not copy a `-wal` file without its database.
3. Record the Silyn version (`silyn --version`), the commit and the migration
   checksums with the backup.
4. Restore the backup in a scratch home and run `silyn doctor` plus
   `runs list` before relying on it.

A backup is not verified until it has been restored once. Keep the backup
outside `$SILYN_HOME` (and out of git). The shell examples use a POSIX shell; on
PowerShell, replace the `$(date ...)` suffix with a literal timestamp and quote
paths the same way.

## Restore

1. Stop all Silyn processes.
2. Move the current home aside; never delete or overwrite it in place:
   `mv "$SILYN_HOME" "$SILYN_HOME.broken-$(date +%Y%m%d%H%M%S)"`.
3. Recreate `$SILYN_HOME` and copy the backup to `$SILYN_HOME/silyn.db`.
   Do not copy stale `-wal`/`-shm` files from another home.
4. Start Silyn once (`silyn doctor` or `silyn init`) so migrations run, then
   check `silyn runs list` and `silyn runs events <run-id>` for a known run.
5. If the restored database is behind the binary's schema, migrations apply
   forward. There is no downgrade path: to run an older binary, restore a
   matching backup.

If SQLite reports corruption, keep the broken home, restore the newest verified
backup, and record the incident. Do not run `VACUUM` or repair tools on the only
copy.

## Retention and purge

The current build has **no automatic retention** and **no purge command**. The
journal, checkpoints, tool ledger and audit tables are the evidence chain; they
are treated as append-only. Deletion is soft-delete plus an explicit purge job
per [ADR 0002](architecture/adr/0002-data-and-events.md); the purge job is not
exposed yet.

Operational policy for the MVP:

- Keep the database and experiment reports for as long as a published claim
  refers to them. Reports must remain reproducible from the recorded commit,
  suite hashes, seed and budget.
- Run artifacts under `$SILYN_HOME` may be pruned manually when they are no
  longer referenced by an experience, candidate or report. Never prune files a
  run still needs to resume.
- Do not hand-edit rows in `runs`, `tool_call_ledger`, `event_journal`,
  `checkpoints` or `audit_events`. Doing so invalidates recovery and audit and
  is not a supported operation.
- If disk pressure requires reclaiming space, archive the entire home (backup
  above), verify the archive, then start a **new** home. This loses local
  continuity and resets the agent's memory; do it deliberately, not by deleting
  individual files.
- When a purge command lands it will be audited and must run as an explicit
  operator action. Until then, "purge" means "archive the home and start fresh".

## Policy rollback

Promotion is versioned and reversible by design. The policy/candidate tables and
the `silyn evolution` commands are delivered by **W5-02/W5-03**; they are not in
the current build.

Contract (**planned**):

```text
silyn evolution list --agent <agent-id> [--json]
silyn evolution show <candidate-id> --agent <agent-id> [--json]
silyn evolution rollback <promotion-id> --agent <agent-id> [--json]
```

A rollback restores the previous immutable policy version, moves the agent's
active pointer back, stamps the promotion as rolled back, and writes an audit row
with the actor. A second rollback of the same promotion fails permanently. The
agent runtime cannot call the rollback path; it has no capability to write the
promotion or audit tables.

Until W5-03 lands:

- A candidate cannot be promoted in this build, so there is no active policy
  pointer to roll back.
- Do not simulate a rollback with SQL. If you must recover a manually modified
  database, restore the last verified backup from before the change instead.

Rollback drill (required before trusting automatic promotion): copy the active
policy, apply a low-risk candidate in a scratch home, roll it back, and verify
that the pointer, promotion row and audit trail all show both versions. Record
the drill with the experiment report.

## Kill and resume

The run loop checkpoints before it starts a side effect and commits the state
transition with its event in one transaction. Killing the process is safe; it
must not be treated as a successful or failed completion.

1. Kill the process (Ctrl-C or the OS task manager). Do not delete the database
   or the `-wal` file.
2. Inspect the run: `silyn runs show <run-id>` and `silyn runs tools <run-id>`.
   A non-terminal status means it can be resumed.
3. Remove a stale cancel file **only** if the cancellation was requested by
   mistake and the run is still resumable:
   `$SILYN_HOME/run/<run-id>.cancel`. Otherwise the resumed run cancels again
   immediately.
4. Resume with the planned W3-03 surface:
   `silyn run --resume <run-id> [--offline]`.
5. Resume refuses a terminal run. A run that failed on budget or verification
   needs a new run, not a resume.

Deleting the home or the cancel file does not make a lost side effect safe. If
the process died during a write, use the `Indeterminate` procedure below.

## Indeterminate tool calls

A tool call is `Indeterminate` when Silyn cannot prove whether the external side
effect happened: the process died after the call started, or the executor
returned an ambiguous result. The ledger records `indeterminate`, and the
runtime **refuses to replay** it. `Indeterminate` maps to exit code **5**, never
to success and never to a retryable exit code.

Resolution procedure:

1. Run `silyn runs tools <run-id>` and note the `call_id`, tool and step of every
   `indeterminate` row.
2. Inspect the external effect out of band: does the target file exist, is it
   complete, does the report contain the expected bytes? The ledger's
   `result_ref` identifies the intended artifact but is not proof that it was
   written.
3. If the effect is **absent**, the safe action is a new run with a corrected
   plan. Do not edit the ledger to `failed` and resume; the kernel has no
   supported "resolve" transition yet.
4. If the effect is **present and complete**, keep the artifact and treat the
   run as failed for accounting. A future resolution API will record the
   operator, evidence and new state; until then, do not hand-write the ledger.
5. Record the call id and the out-of-band evidence in the incident or experiment
   notes. A cluster of `Indeterminate` calls is a defect, not routine noise.

The invariant is deliberate: an unproven side effect is never repeated, and an
unproven success is never counted.

## Logs

There is no log file in the current build. Diagnostics are:

| Surface | Where | Contents |
|---|---|---|
| CLI errors | stderr | error class and bounded message; `--json` wraps `{"error":{"class","message"}}` |
| Event journal | `silyn runs events <run-id>` | typed business transitions with sequence and timestamp |
| Tool ledger | `silyn runs tools <run-id>` | step, tool, state, call id, result reference |
| Run record | `silyn runs show <run-id>` | status, model, policy version, tokens, cost, timestamps |
| Build/config | `silyn doctor --json`, `silyn config --json` | paths, model id, key **presence** only |

`SILYN_LOG_LEVEL` appears in `.env.example` for the upcoming tracing layer but is
**ignored** by the current build. W7-03b adds structured spans and a configurable
exporter; until then, do not promise log retention or ship stderr captures
without checking them for paths and user data. The planned span names, fields
and redaction rules are in
[observability](architecture/observability.md).

When capturing diagnostics for a bug report, include the version, platform,
command and error class, and redact absolute paths, prompts and any key-shaped
string.

## Experiment operations

- Run development checks on the `dev` split with `SILYN_EVAL_PROVIDER=mock`
  (the default). CI never calls a paid provider.
- A sealed `test` run needs `--allow-sealed`, a sealed manifest and an
  operator; it verifies `suite.lock` before loading tasks.
- Set a hard budget before the first call: steps, tokens, cost and wall clock.
  Stop on a hash mismatch, an endpoint mismatch, an unauthorized action or a
  cost overrun.
- Write reports under `eval/reports/` and record the metadata in
  [`docs/experiments/`](experiments/template.md). Do not edit report numbers;
  regenerate from the recorded seed and hashes.
- The formal experiment is **W6**. No result is claimed before its report is
  published.
