# ADR 0005: Trust boundaries and capabilities

- Status: Accepted
- Date: 2026-10-20

## Context

Model output, retrieved text, tool output and memory content can all be hostile
or wrong. Authorization must not depend on the model choosing to obey a prompt.

## Decision

Trust levels, from lowest to highest:

1. tool output, retrieved documents, web content;
2. model output and candidate proposals;
3. memory records (ranked by provenance and evidence);
4. user input for the current task;
5. kernel configuration, permissions and evaluation rules.

Lower levels can never cause a higher level to change. Tool and memory text is
wrapped as untrusted data and never parsed as instructions.

Capabilities are explicit strings (`fs.read`, `fs.list`, `data.profile`,
`report.write`). The tool broker checks the run's granted capabilities before
execution. Default is deny. There is no shell tool in the MVP.

Path policy: canonicalize with `dunce`, require `starts_with(run_root)`, reject
`..`, UNC paths, Windows alternate data streams and symlink escapes.

Evaluation isolation: separate process, fresh temporary database and working
directory, scrubbed environment, model endpoint allowlist.

Audit: permission decisions, promotions, rollbacks and deletions are recorded in
an append-only table with actor and evidence reference.

## Consequences

- A prompt injection cannot grant a capability, because the broker never consults
  the prompt for authorization.
- Tool additions require a new capability and an ADR.
- Logs must redact secrets and sensitive prompt text.

## Alternatives considered

- Prompt-only guardrails: rejected, unenforceable.
- Allowing a shell tool early: rejected, it breaks the sandbox assumption before
  the sandbox exists.
