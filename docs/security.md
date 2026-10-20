# Security

- Status: W7 operator documentation
- Applies to: the local, single-user MVP
- Last reviewed: 2026-10-21

Silyn executes model-proposed actions through a capability-gated broker, keeps
an append-only event journal, and evaluates policy changes against a sealed task
distribution. This document describes the security properties an operator can
rely on, the ones the MVP deliberately does **not** provide, and how to report a
problem.

The design rationale lives in
[ADR 0001](architecture/adr/0001-scope-and-invariants.md),
[ADR 0005](architecture/adr/0005-trust-boundaries-and-capabilities.md) and the
[threat model](architecture/threat-model.md).

## Trust boundaries

Silyn orders trust explicitly. A lower level can never cause a higher level to
change.

| Level | Source | Trust | May influence |
|---|---|---|---|
| 1 | Tool output, retrieved documents, web content | untrusted data | context content only |
| 2 | Model output, candidate proposals | untrusted output | proposed actions and patch text |
| 3 | Memory records (provenance-ranked) | medium | future context, ranked by evidence |
| 4 | User input for the current task | high for that task | objective and constraints |
| 5 | Kernel configuration, permissions, evaluation rules | trusted, immutable | everything below |

Practical consequences:

- Model output may only **propose** an action. The tool broker authorizes it.
- Text from a tool or a memory record is data. It is never parsed as an
  instruction, and prompt injection cannot widen a capability.
- Candidates cannot change tools, capabilities, verification, budgets,
  thresholds, approval configuration or executable code. Those fields are outside
  the policy allowlist and are statically rejected.

## Capabilities and default deny

A run starts with **no capabilities**. The CLI only grants what is passed
explicitly (`--capability <name>` on the planned `silyn run` surface). The
broker performs a plain string comparison against the run's granted capabilities
before any filesystem access; prompts and tool arguments are never consulted for
authorization.

| Capability | Tool | Reach |
|---|---|---|
| `fs.list` | list a directory | run root only |
| `fs.read` | read a UTF-8 file (<= 1 MiB) | run root only |
| `data.profile` | row/column/missing/duplicate statistics | supplied CSV in the run root |
| `report.write` | write a report | `output/` under the run root |

There is no shell tool, no network tool and no generic HTTP client in the tool
set. A call without its capability fails closed with `PolicyDenied` before any
filesystem access and is recorded in the audit stream.

Path policy: arguments are validated syntactically (relative components only, no
`..`, no `:`, no leading separator), then canonicalized and required to stay
under the run root. Windows alternate data streams, UNC paths, drive prefixes
and symlink escapes are rejected.

## Cancel file

Cancellation is a cooperative file signal, not an authenticated control plane.

- Path: `$SILYN_HOME/run/<run-id>.cancel` (`SILYN_HOME` defaults to `.silyn`).
- `silyn runs cancel <run-id>` validates that the run exists and is
  non-terminal, creates the directory, and writes the file with
  `create_new`. The existence check and the write are one atomic step, so
  repeated or concurrent requests leave exactly one file and report
  `already_requested: true`.
- The runtime polls the file while the run is active and moves the run to
  `Cancelled`; the CLI never changes run status itself.
- A terminal run cannot be cancelled and no file is written.

Security implication: any local process that can write to `$SILYN_HOME/run/` can
stop a run. That is acceptable for a single-user local tool, but it means the
data directory must be protected with filesystem permissions. Deleting a cancel
file does not retroactively undo a cancellation, and the runtime may observe the
file between a delete and a resume.

## Secrets

Silyn reads model credentials **only** from the environment. The key is never
stored in SQLite, a policy artifact, a report or a log.

- The variable name is configured by `SILYN_LLM_API_KEY_ENV` and defaults to
  `DEEPSEEK_API_KEY`.
- `.env` is git-ignored (`.env`, `.env.*`, except `.env.example`). Copy
  `.env.example` to `.env` for local work and leave the example empty.
- `silyn doctor` and `silyn config` report only `api_key_present: true|false`.
  They never print the value.
- Do not pass a key as a command-line argument; it lands in shell history and
  process listings. Export it in the environment or use `.env`.
- Do not enable shell tracing (`set -x`, `bash -x`) around commands that read
  the key.
- Error messages and the event journal contain state transitions, tool names and
  bounded reasons, not request headers, prompts or file content.
- The planned tracing layer (W7-03b) uses an allowlist of span fields and
  redaction rules; `Authorization` headers, request bodies and key-shaped strings
  must never be exported. See
  [observability](architecture/observability.md#redaction-rules).
- If a key is exposed, revoke it at the provider first, then rotate the value in
  your environment. A key that reached a terminal, a log or a screenshot is
  compromised even if the file was deleted afterwards.

## Sealed evaluation suite

The evaluation suite is the integrity boundary for the central claim.

- `eval/suites/csv-stats-v1/suite.toml` declares `sealed = true`; `suite.lock`
  records the sha256 of every sealed artifact.
- `cargo run -p xtask -- verify-eval-integrity` fails if a locked artifact
  changes. The correct response is a **new suite version**, never editing the
  lock or the thresholds in place.
- The test split is reachable only through `SealedTestSet` and
  `silyn-eval run --split test --allow-sealed`. The flag is an operator
  acknowledgement, not an authentication mechanism; the harness verifies
  `suite.lock` before loading any task.
- Candidate generation and reflection never receive test tasks or the oracle.
  A candidate produced by a process that could read the sealed set is not
  eligible for automatic promotion.
- Thresholds are pre-registered in the suite manifest and must not change after
  results are known. A run that changes them is invalid.

**Isolation limit:** the evaluation worker is a separate process with a cleared
environment, a fresh temporary working directory and a timeout. It is **not** an
OS sandbox and can read the filesystem. Do not run an evaluation on a machine
whose other files you would not expose to the solver, and do not describe the
worker as a sandbox in reports.

## Threat model

The assets, actors, threats and residual risks are maintained in
[`docs/architecture/threat-model.md`](architecture/threat-model.md). The
mitigations that exist in code are gated by the acceptance registry and the
`verify-*` commands; the residual risks (model persuasion, no OS sandbox,
provider nondeterminism) are accepted for the MVP and must be repeated in
experiment reports.

## Reporting a vulnerability

Please report security issues **privately**:

1. Use the repository host's private security advisory feature for
   `siatom/silyn` if it is available. If it is not, contact the maintainer
   directly before any public disclosure.
2. Do not open a public issue for an unfixed vulnerability.
3. Include the affected version or commit, the platform, a minimal reproduction
   and the impact. Do not include API keys, user data, sealed test content or
   other people's files; redact them.
4. Allow time for a fix before publishing details. This is a volunteer project
   without a paid response team, so timelines are best effort.

Out of scope for the MVP: OS-level sandbox escapes from a deliberately exposed
worker, denial of service by someone who already has local write access to
`$SILYN_HOME`, and provider-side incidents. In scope: capability bypasses,
path-confinement escapes, secret leakage, journal or audit tampering, sealed-test
exposure and promotion without a valid evaluation.
