# Silyn CLI

- Status: W7-02b (reporting, JSON surface and local API serve)
- Date: 2026-10-21
- Contract: CLI surface v1, exit codes v1, JSON envelope v1

The `silyn` binary is a thin frontend over `silyn-app`: it parses flags,
delegates every decision to the service layer and only formats results. It is
built to be scripted, so the `--json` output, the error envelope and the exit
codes are stable contracts.

## Commands

Every command accepts the global `--json` flag, before or after the subcommand.

| Command | Kind | `--json` on stdout | Notes |
|---|---|---|---|
| `silyn init` | write | `{home, database}` | Idempotent; creates the home directory and database |
| `silyn doctor` | read | version, platform, resolved paths, provider fields | Never prints a key |
| `silyn config` | read | non-secret resolved settings | Never prints a key |
| `silyn agent create --owner <label>` | write | `AgentRecord` | Human mode prints the bare id |
| `silyn agent list` | read | `[AgentRecord]` | Newest first |
| `silyn session create --agent <id>` | write | `SessionRecord` | Human mode prints the bare id |
| `silyn task create --session <id> --objective <text>` | write | `TaskRecord` | Human mode prints the bare id |
| `silyn run --task <id>` / `--resume <id>` / `--session <id> --objective <text>` | write | `RunOutcome` | Exit code follows the terminal status |
| `silyn runs list` | read | `[RunRecord]` | Bounded to the 50 newest runs |
| `silyn runs show <run-id>` | read | `RunRecord` | |
| `silyn runs events <run-id>` | read | `[EventEnvelope]` | Sequence order |
| `silyn runs tools <run-id>` | read | `[ToolCallRecord]` | Oldest first; `indeterminate` is reported as such |
| `silyn runs cancel <run-id>` | write | `{run_id, path, already_requested}` | Writes the cancel file; the runtime owns the transition |
| `silyn memory add` | write | `MemoryRecord` | Human mode prints the memory id |
| `silyn memory list --agent <id>` | read | `[MemoryHitRecord]` | Active memories, newest first |
| `silyn memory search --agent <id> <query>` | read | `[MemoryHitRecord]` | Owner and agent scoping is applied by the app layer |
| `silyn memory delete --agent <id> <memory-id>` | write | `{memory_id, deleted}` | Soft delete |
| `silyn experience list --agent <id>` | read | `[ExperienceRecord]` | Active first, then oldest first |
| `silyn evolution init --agent <id>` | write | `PolicyVersionRecord` | Creates and activates the base policy |
| `silyn evolution list --agent <id>` | read | `{agent_id, active_policy, latest_policy, candidates}` | |
| `silyn evolution show <candidate-id> --agent <id>` | read | `CandidateDetail` | Candidate, evaluations, promotions, audit trail |
| `silyn evolution submit --agent <id> --patch <file\|->` | write | `CandidateRecord` | Static checks run before the row is written |
| `silyn evolution evaluate <candidate-id> --agent <id> --report <file> [--stats <file>]` | write | `EvaluationAttachment` | Defaults to `<report>.stats.json` |
| `silyn evolution approve <candidate-id> --agent <id> --as <operator>` | write | `CandidateRecord` | Local operator action, audited |
| `silyn evolution promote <candidate-id> --agent <id>` | write | `PromotionResult` | A rejection exits 5 |
| `silyn evolution rollback <promotion-id> --agent <id>` | write | `RollbackResult` | Restores the replaced version |
| `silyn report run <run-id>` | read | `run_report` bundle | See below |
| `silyn report experiment <report.json>` | read | `experiment_report` bundle | See below |
| `silyn serve [--addr <host:port>] [--token-env <name>]` | serve | `{addr, token_required}` with `--json` | Local HTTP/SSE API; Ctrl-C shuts down |

`--json` is a single document on stdout. Human mode is a table or `key: value`
lines; long values are printed in full rather than truncated, so a script should
prefer `--json`.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success, including a run that completed |
| 2 | Usage error: unknown command, missing flag, malformed id, unknown `--format` |
| 3 | Cancelled run, or a policy denial |
| 4 | Transient failure: safe to retry |
| 5 | Permanent failure, schema failure, or an indeterminate outcome; do not retry blindly |

A failed or cancelled **run** is a result, not a CLI error: `silyn run` prints the
`RunOutcome` on stdout and exits 5 or 3 so scripts can branch without parsing
stderr. Cancellation by another process is therefore exit 3 with the outcome on
stdout. A command that could not produce a result exits non-zero and writes the
error envelope to stderr.

## Error envelope

Every failure in `--json` mode writes exactly one document to stderr:

```json
{"error":{"class":"permanent","message":"permanent failure: run 0192... not found"}}
```

`class` is one of `transient`, `permanent`, `policy_denied`, `indeterminate`.
The mapping to exit codes is stable: `policy_denied` is 3, `transient` is 4,
`permanent` and `indeterminate` are 5. Stdout stays empty on failure.

## Report commands

```text
silyn report run <run-id> [--format json|markdown] [--out <path>] [--json]
silyn report experiment <report.json> [--stats <stats.json>] [--format json|markdown] [--out <path>] [--json]
```

- `--format` selects the payload encoding; the default is `json`.
- `--out` writes the payload to a file instead of stdout and prints a short
  confirmation. With `--out --json`, stdout is the metadata document
  `{"path", "format", "bytes"}` and the payload is only in the file.
- `--json` forces the JSON encoding, so a caller that passes it always gets
  exactly one JSON document on stdout.
- Markdown is rendered from the same JSON bundle the JSON encoding emits: every
  number in the Markdown comes from that bundle, never from a second read of the
  database or the report files.

### `report run`

The bundle reads the run through `silyn-app` and derives the verified outcome
from the last `run_verified` event and the failure reason from the last
`run_failed` event. The run status is never used to guess an outcome.

```json
{
  "schema_version": 1,
  "kind": "run_report",
  "generated_at": "2026-10-21T00:00:00Z",
  "run": {
    "id": "0192f0a0-0000-7000-8000-000000000001",
    "task_id": "0192f0a0-0000-7000-8000-000000000002",
    "policy_version": "0192f0a0-0000-7000-8000-000000000003",
    "memory_snapshot_version": null,
    "model_id": "deepseek-chat",
    "status": "completed",
    "input_tokens": 0,
    "output_tokens": 0,
    "cost_usd": 0.0,
    "started_at": "2026-10-21T00:00:00Z",
    "finished_at": "2026-10-21T00:00:01Z",
    "created_at": "2026-10-21T00:00:00Z"
  },
  "status": "completed",
  "terminal": true,
  "outcome": "unknown",
  "failure": null,
  "event_count": 4,
  "event_types": ["run_created", "run_started", "run_verified", "run_completed"],
  "events": [],
  "tool_call_count": 0,
  "tool_summary": {"started": 0, "succeeded": 0, "failed": 0, "indeterminate": 0},
  "tool_calls": [],
  "usage": {"input_tokens": 0, "output_tokens": 0, "cost_usd": 0.0}
}
```

- `outcome` is `success`, `failed`, `unknown` or `null` when no `run_verified`
  event exists.
- `failure` is `{"reason": "..."}` when a `run_failed` event exists, otherwise
  `null`; a completed run with a failed verification keeps `outcome: "failed"`.
- `events` and `tool_calls` carry the full records (`events` is elided above);
  `event_types`, `tool_summary` and the counts are derived summaries.
- **Memory references are omitted.** `RunOutcome::memory_refs` is not persisted
  and `silyn-app` exposes no per-run memory snapshot, so the report cannot state
  which memories were injected. An empty list would be a false claim.

### `report experiment`

The bundle reads the frozen `EvalReport` and, unless `--stats` says otherwise,
the sibling `<report>.stats.json` when it exists. A missing companion is not an
error; the bundle then carries the report arms and no comparisons, digests or
verdict reason.

```json
{
  "schema_version": 1,
  "kind": "experiment_report",
  "generated_at": "2026-10-21T00:00:00Z",
  "sources": {"report": "eval/reports/w6-dev.json", "stats": "eval/reports/w6-dev.stats.json"},
  "report": {},
  "stats": {},
  "candidate_id": "73b53360-af56-4f75-a416-42c50b55e875",
  "model_id": "deepseek-chat",
  "model_reported": "deepseek-flash",
  "baseline_id": "model.openai-compatible.v1:deepseek-chat",
  "created_at": "2026-10-20T19:43:12.257771500Z",
  "suite": {
    "version": "csv-stats-v1.1",
    "split": "dev",
    "sealed": false,
    "repeats": 1,
    "seed": 6840123409045659686,
    "backend": "in_process",
    "digests": {"train": "...", "dev": "...", "test": "..."}
  },
  "arm_count": 3,
  "arms": [
    {
      "id": "A",
      "label": "no-memory-baseline",
      "solver": "model.openai-compatible.v1",
      "model_id": "deepseek-chat",
      "memory": false,
      "evolution": false,
      "successes": 6,
      "total": 10,
      "success_rate_pp": 60.0,
      "cost_usd": 0.0,
      "latency_p50_ms": 728.333,
      "latency_p95_ms": 919.5297,
      "tool_errors": 0,
      "unauthorized_actions": 0
    }
  ],
  "comparisons": {
    "c_minus_a": {
      "comparison": "C-A",
      "point_pp": 0.0,
      "ci_low_pp": 0.0,
      "ci_high_pp": 0.0,
      "paired_tasks": 10
    }
  },
  "thresholds": {"min_gain_pp": 10.0, "max_cost_ratio": 2.0, "max_regression_pp": 5.0, "max_cost_usd": 1.0},
  "thresholds_source": "stats",
  "verdict": "reject",
  "stats_verdict": "reject",
  "reason": "the policy arm regresses against the memory arm",
  "unauthorized_actions": 0
}
```

- `report` and `stats` embed the source documents so the bundle is
  self-contained (`{}` above only abbreviates them).
- Arm values come from the statistics companion when it carries them and from the
  report otherwise; `success_rate_pp` is computed from `successes / total` only
  when neither document states it.
- `verdict` is the frozen report verdict. `stats_verdict` is the companion's own
  verdict (`inconclusive` maps to a `reject` report and is shown unchanged here).
- `thresholds` prefers the companion because it also carries `max_cost_usd`;
  `thresholds_source` records which document supplied them.
- A companion whose `suite_version` disagrees with the report is a schema error
  (exit 5) rather than a silently mixed bundle.

## Examples

```bash
# Create an identity and a session, then run one offline task.
silyn agent create --owner local-user --json
silyn session create --agent <agent-id> --json
silyn run --session <session-id> --objective "summarize the csv" --offline --json

# Observe and report the run.
silyn runs show <run-id> --json
silyn runs events <run-id> --json
silyn report run <run-id> --json
silyn report run <run-id> --format markdown --out run-report.md

# Report the committed W6 experiment, with its statistics companion.
silyn report experiment eval/reports/w6-dev.json --json
silyn report experiment eval/reports/w6-test.json \
  --stats eval/reports/w6-test.stats.json --format markdown

# Memory and experience.
silyn memory add --agent <agent-id> --summary "deploy window" \
  --content "Deploy window starts Monday." --json
silyn memory search --agent <agent-id> deploy --json
silyn experience list --agent <agent-id> --json

# Evolution: submit, evaluate, approve or promote.
silyn evolution init --agent <agent-id> --json
silyn evolution submit --agent <agent-id> --patch patch.json --json
silyn evolution evaluate <candidate-id> --agent <agent-id> \
  --report eval/reports/w6-dev.json --json
silyn evolution promote <candidate-id> --agent <agent-id> --json
```

## Secrets

The CLI never prints an API key. `doctor` and `config` report only the variable
name and whether a value is present. Report bundles are built from run records,
events, the tool ledger and evaluation documents; none of those carry provider
credentials, and a key-shaped string never reaches stdout or stderr. See
[security](../security.md) for the trust boundaries and
[operations](../operations.md) for recovery procedures.

## Local API

`silyn serve` starts the `silyn-api` router on `SILYN_API_ADDR` (default
`127.0.0.1:7788`) and shuts down gracefully on Ctrl-C. `--json` prints
`{"addr": "...", "token_required": true|false}` before serving. Non-loopback
addresses are refused unless `SILYN_API_ALLOW_NON_LOOPBACK=1`. The optional
bearer token is read from `SILYN_API_TOKEN` or the variable named by
`--token-env`; `/health` stays public. See
[security](../security.md) and the W7-01 gate for the HTTP contract.
