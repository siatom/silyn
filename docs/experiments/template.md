# Experiment record template

Copy this file to `docs/experiments/<experiment-id>.md` and fill every field
**before** the first paid call. An experiment that changes a field after seeing
results is invalid, not a revision. The protocol is defined in
[ADR 0004](../architecture/adr/0004-evaluation-protocol.md); W6 is the first
formal experiment on `csv-stats-v1`.

## Identity

| Field | Value |
|---|---|
| Experiment id | `<experiment-id>` |
| Date (UTC) | `<YYYY-MM-DD>` |
| Operator | `<name or handle>` |
| Git commit | `<git rev-parse HEAD>` |
| Silyn version | `<silyn --version>` |
| Platform | `<os>-<arch>` |
| Model id (requested) | `<e.g. deepseek-chat>` |
| Model id (reported by provider) | `<value from the response, or "not reported">` |
| Base URL | `<provider endpoint>` |
| Temperature / max tokens | `<values>` |
| Provider price table used | `<input/output USD per million tokens, source and date>` |

The requested and reported model ids both matter: a provider can silently move
an alias. If the reported id differs from the requested id, say so in
`Deviations` and keep the run only if the endpoint was the pinned one.

## Suite

| Field | Value |
|---|---|
| Suite id | `csv-stats-v1` |
| Suite version | `<from suite.toml>` |
| Manifest sha256 | `<sha256 of suite.toml>` |
| `suite.lock` status | `<verify-eval-integrity output>` |
| Train split digest | `<sha256>` |
| Dev split digest | `<sha256>` |
| Test split digest | `<sha256>` |
| Sealed | `<true/false>` |
| Thresholds (pre-registered) | `min_gain_pp=<n>`, `max_regression_pp=<n>`, `max_cost_ratio=<n>`, `max_cost_usd=<n>` |

`cargo run -p xtask -- verify-eval-integrity` and
`cargo run -p silyn-eval -- hashes` must both pass before the run. Record the
**test** digest even though it is sealed; it proves the evaluated distribution.

## Budget

| Limit | Value |
|---|---|
| Tasks per split | train `<n>`, dev `<n>`, test `<n>` |
| Repeats per task | `<3–5; 1 only for a smoke run>` |
| Seed | `<u64; the evaluator default is allowed if recorded>` |
| Max steps per task | `<n>` |
| Max input tokens | `<n>` |
| Max output tokens | `<n>` |
| Max cost per task (USD) | `<n>` |
| Max cost for the experiment (USD) | `<n>` |
| Max wall clock per task | `<seconds>` |
| Max candidates | `<n>` |
| Bootstrap resamples / confidence | `1000` / `95%` |

Arms A, B and C must use the **same** model, tools, token/time budget and base
prompt. Record the evidence that the budget is equal (the configuration object or
the harness check), not just the intent.

| Arm | Memory | Evolution | Policy version | Prompt/context version | Model |
|---|---|---|---|---|---|
| A — baseline | off | off | `<none>` | `<version>` | `<id>` |
| B — memory | on | off | `<none>` | `<version>` | `<id>` |
| C — evolved | on | on | `<promoted policy id>` | `<version>` | `<id>` |

## Per-arm results

One row per arm, test split only. Do not pool dev and test.

| Metric | A | B | C |
|---|---|---|---|
| Tasks / attempts | `<n>` | `<n>` | `<n>` |
| Completed | `<n>` | `<n>` | `<n>` |
| Verified success | `<n>` | `<n>` | `<n>` |
| Success rate | `<pct>` | `<pct>` | `<pct>` |
| Total cost (USD) | `<n>` | `<n>` | `<n>` |
| Mean duration per task (s) | `<n>` | `<n>` | `<n>` |
| Tool errors | `<n>` | `<n>` | `<n>` |
| `Indeterminate` tool calls | `<n>` | `<n>` | `<n>` |
| Unauthorized actions | `0` | `0` | `0` |
| Budget failures | `<n>` | `<n>` | `<n>` |

Report the raw counts next to every percentage. A "verified success" is one where
independent checkers returned evidence; `Unknown` is not a success.

## Paired comparison

Paired by task id, with the delta computed per task and bootstrapped over tasks.
Report both confidence intervals; the primary comparison is C − A and the
regression guard is C − B.

| Comparison | Paired tasks | Point estimate (pp) | 95% CI low | 95% CI high | Cost ratio | Unauthorized | Verdict input |
|---|---|---|---|---|---|---|---|
| B − A | `<n>` | `<pp>` | `<pp>` | `<pp>` | `<B/A>` | `0` | `<improved/unchanged/worse>` |
| C − B | `<n>` | `<pp>` | `<pp>` | `<pp>` | `<C/B>` | `0` | `<within/regressed>` |
| C − A | `<n>` | `<pp>` | `<pp>` | `<pp>` | `<C/A>` | `0` | `<meets/misses threshold>` |

State the bootstrap settings and the exact seed. Re-rolling the interval to fit a
conclusion is prohibited. If the sample is degenerate (all success or all
failure), say so and report the zero-width interval rather than hiding it.

## Verdict

Fill exactly one, using the pre-registered rules and nothing else:

- [ ] **pass** — C − A meets `min_gain_pp` with a 95% lower bound above zero,
  C − B regression is within `max_regression_pp`, cost ratio is within
  `max_cost_ratio`, cost is within the cap, and unauthorized actions are zero.
- [ ] **fail** — the point estimate misses the threshold, or the interval
  includes zero, or the regression/cost guard fails. Publish the negative result.
- [ ] **invalid** — isolation, a hash, the budget, the model pinning or the
  threshold pre-registration was violated. Record the defect; rerun only after a
  fix, with a new experiment id.

Verdict: `<pass | fail | invalid>`

Reason (required for fail/invalid): `<one paragraph>`

Deviations from the pre-registration: `<none, or list each deviation and why it
does not invalidate the run>`

Sealed-set statement: `<confirm that the test split was not used for tuning,
prompt selection or threshold changes>`

## Reproduction

Record the exact commands, with output paths and no secrets:

```bash
git rev-parse HEAD
cargo run -p xtask -- verify-eval-integrity
cargo run -p silyn-eval -- hashes
cargo run -p silyn-eval -- run \
  --suite csv-stats-v1 --split test --allow-sealed \
  --repeats <n> --seed <n> \
  --out eval/reports/<experiment-id>.json \
  --stats-out eval/reports/<experiment-id>.stats.json \
  --markdown eval/reports/<experiment-id>.md
```

A reviewer with the same commit, suite artifacts, model id and seed must be able
to regenerate the report. Attach the JSON and Markdown verbatim; do not edit
numbers by hand.
