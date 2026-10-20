# Observability: spans, fields and redaction

- Status: **planned — W7-03b**
- Contract: tracing span names and fields v1
- Last reviewed: 2026-10-21

The current build has no tracing subscriber and writes no log file. Durable
observability is the SQLite event journal exposed by `silyn runs events` and the
tool ledger exposed by `silyn runs tools` (see
[operations](../operations.md#logs)). This document freezes the **planned**
structured tracing contract so the runtime and API surfaces can add spans
without inventing names or leaking data. Until W7-03b lands, treat every span
below as a design target, not as an existing feature.

## Status

- Implemented today: journal events, tool ledger states, `silyn doctor` /
  `silyn config` diagnostics.
- Planned for W7-03b: span emission, an optional local exporter, sampling and
  the redaction tests.
- Non-goal for the MVP: hosted telemetry, long-term log retention and remote
  tracing. Silyn is local-first; telemetry stays on the machine unless an
  operator explicitly configures an exporter.

## Span names

Span names are a stable, closed set. Use exactly these strings; do not add a
product prefix, version suffix or dynamic component (dynamic values belong in
fields).

| Span | Parent | Emitted around | One span per |
|---|---|---|---|
| `run` | root | one run, from creation to terminal status | run |
| `step` | `run` | one loop iteration (model turn or tool resolution) | step |
| `model` | `step` | one provider request, including retries as fields | request |
| `tool` | `step` | one broker-authorized tool execution | tool call |
| `checkpoint` | `run` | persisting a checkpoint or recovering from one | checkpoint write/read |
| `verify` | `run` | outcome verification over the run's artifacts | verifier invocation |

Relationships:

```text
run
├── step (1..n)
│   ├── model (0..1 per step)
│   └── tool  (0..1 per step)
├── checkpoint (0..n)
└── verify (0..n)
```

`run` is the correlation root. `checkpoint` and `verify` are direct children of
`run`, not of the step that triggered them, so a failed verification remains
visible after the step span closes.

## Fields

Only the fields listed here may be attached. Field names are snake_case and
stable; adding a field is a contract change and needs a docs update.

| Span | Fields |
|---|---|
| `run` | `run_id`, `task_id`, `agent_id`, `policy_version`, `model_id`, `status`, `step_count`, `input_tokens`, `output_tokens`, `cost_usd`, `duration_ms` |
| `step` | `run_id`, `step_id`, `step_index`, `kind` (`model` or `tool`), `status`, `duration_ms` |
| `model` | `run_id`, `step_id`, `provider`, `model_id`, `requested_model`, `status`, `input_tokens`, `output_tokens`, `cost_usd`, `duration_ms`, `retry_count` |
| `tool` | `run_id`, `step_id`, `tool_name`, `capability`, `call_id`, `state` (`started`, `succeeded`, `failed`, `indeterminate`), `duration_ms`, `result_ref`, `error_class` |
| `checkpoint` | `run_id`, `step_id`, `schema_version`, `checksum_prefix` (first 12 hex chars), `direction` (`write` or `read`), `status` |
| `verify` | `run_id`, `checker`, `outcome` (`success`, `failed`, `unknown`), `evidence_count`, `duration_ms` |

Field rules:

- Identifiers are opaque values (`run_id`, `call_id`, `policy_version`), never
  display names or file paths.
- `error_class` is one of `transient`, `permanent`, `policy_denied`,
  `indeterminate`; the message stays in the journal or stderr and is not copied
  into a span.
- `status` uses the JSON wire names (`created`, `running`, `waiting_tool`,
  `verifying`, `completed`, `failed`, `cancelled`).
- Token counts and cost are numbers, not formatted strings. Missing values are
  omitted; never emit `0` to mean "unknown".
- `result_ref` is the ledger reference (an opaque artifact id). It is not a path
  and not file content.

## Redaction rules

Telemetry is an allowlist, not a denylist. A field is exported only if it appears
in the table above; everything else is dropped at the call site.

Never export:

- API keys, bearer tokens, the value of `SILYN_LLM_API_KEY_ENV`, or any string
  that looks like a key; never `Authorization`, `Cookie` or provider headers.
- Prompts, model messages, retrieved memory text, tool arguments, tool output,
  file contents or report bodies.
- User data from objectives, task text, file names, absolute paths, home
  directories or repository paths.
- Provider response bodies, request ids that embed an account identifier, or URL
  query strings.
- Raw error messages. Use `error_class` plus the existing bounded reason that is
  already persisted in the journal and has passed the error-path tests.

Rules for implementers:

1. Do not use `#[instrument]` with `Debug` on a request, response, argument or
   artifact type. Those types may gain fields later; explicit field lists cannot
   leak by accident.
2. Do not log a model request or response at any level, including `trace`.
3. Redaction is tested with a canary: set a synthetic key-shaped string and a
   synthetic prompt, run a task, and assert neither appears in the exported span
   data. The W7-03b gate will include that test.
4. Local debugging may print to stderr, but the output must still obey the
   secrets rule in [security](../security.md#secrets); never ask an operator to
   paste a full environment dump.
5. Sampling and export are opt-in. The default is no exporter. A local exporter
   writes only on an explicit path or endpoint configured by the operator.

## Export and retention

- The planned default is an in-process subscriber with no exporter; spans are
  discarded unless an operator enables a local sink.
- Sampling is per-run for `run` and per-step for its children, so a sampled run
  never produces an orphaned `step` or `verify` span.
- Spans are not the audit trail. The journal and audit table remain the
  authoritative, append-only record; deleting exported telemetry must never
  remove evidence.
- If an exporter is enabled, its output is subject to the same retention and
  filesystem-protection rules as `$SILYN_HOME` (see
  [operations](../operations.md#retention-and-purge)).
