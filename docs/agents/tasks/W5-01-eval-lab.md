# W5-01: Evaluation lab runner, sealed suite and statistical report

```yaml
id: W5-01
wave: 5
depends_on: [W2-04, W4-02]
owns:
  - crates/silyn-eval/src/**
  - crates/silyn-eval/tests/**
  - crates/silyn-eval/Cargo.toml
  - eval/suites/csv-stats-v1/**
  - eval/acceptance/w5-01.toml
  - docs/gates/w5-01.md
reads:
  - schemas/eval_report.schema.json
  - schemas/suite_manifest.schema.json
  - docs/architecture/adr/0004-evaluation-protocol.md
  - crates/silyn-eval/src/**
  - eval/suites/csv-stats-v1/**
forbids:
  - migrations/**
  - crates/silyn-core/**
  - crates/silyn-storage/**
  - crates/silyn-runtime/**
  - crates/silyn-app/**
  - crates/silyn-cli/**
  - eval/suites/other-*
  - Cargo.toml
contracts: [EvalReport v1, SuiteManifest v1, solver isolation v1]
acceptance_registry:
  - eval.sealed_split_not_exposed
  - eval.byte_identical_split
  - eval.tampered_artifact_rejected
  - eval.three_arms_same_budget
  - eval.report_schema_complete
  - eval.bootstrap_ci_deterministic
  - eval.worker_isolation
commands:
  - cargo test -p silyn-eval
  - cargo clippy -p silyn-eval --all-targets -- -D warnings
  - cargo run -p xtask -- verify-eval-integrity
  - cargo run -p xtask -- verify-schemas
  - cargo run -p xtask -- verify-acceptance
```

## Goal

Turn the W2 eval skeleton into a reproducible lab: sealed test split, isolated
task workers, A/B/C arms with equal budgets, repeated runs, paired bootstrap
confidence intervals, JSON + Markdown reports that validate against the frozen
schema, and hash-checked suite artifacts.

## Non-goals

- No candidate generation or promotion (W5-02).
- No real provider call; W6 runs the formal experiment.
- No new task families beyond `csv-stats-v1`.
- No changes to `eval/acceptance/registry.toml` or other suites.

## Runner requirements

- `silyn-eval run --suite csv-stats-v1 --split dev|test --arm a|b|c|all --repeats N --out report.json [--markdown report.md]`.
- `test` is only runnable through `SealedTestSet`; a candidate solver never
  receives test tasks or oracles. The CLI must refuse `--split test` for
  candidate code; the lab itself may run it only after the suite is sealed.
- Each task executes in an isolated child process:
  - `silyn-eval worker` reads one `TaskInput` JSON from stdin and writes one
    `Submission` JSON to stdout;
  - `env_clear()` then allow only `PATH`, `SYSTEMROOT`, `TEMP`, `TMP` (Windows)
    and `HOME` (Unix); set a fresh temp cwd and database path;
  - wall-clock timeout per task; kill the child on timeout;
  - non-zero exit, invalid JSON or timeout is a task failure, not a harness
    crash.
- In-process mode is allowed for unit tests behind a `RunnerBackend` trait, but
  the CLI default is subprocess isolation.
- A/B/C arms use the same task order, budgets and model id. The W5 solver is a
  deterministic baseline; each arm records its memory/policy configuration
  explicitly. Empty/different arms are allowed, but the report must label them.
- `--repeats` is bounded (1–5). Each repeat uses the same tasks and a recorded
  seed. No arm gets extra tokens, time or tools.

## Suite sealing

- Set `sealed = true` in `eval/suites/csv-stats-v1/suite.toml` and bump
  `suite_version` to `csv-stats-v1.1` (or add a new suite directory if the
  schema requires a new version).
- `suite.lock` lists sha256 of every committed suite artifact (manifest,
  README, generator/oracle source paths under `eval/suites/...` only). Do not
  list generated fixtures.
- `silyn-eval hashes` must match the train/dev/test digests in the manifest.
- Any changed artifact fails `verify-eval-integrity` until a new suite version
  is created.

## Statistics and report

- Per arm: `successes`, `total`, `success_rate`, `cost_usd`, `duration_ms`,
  `tool_errors`, `unauthorized_actions` (expected 0).
- Paired comparisons: B−A, C−B, C−A; report point estimate and 95% bootstrap
  percentile CI over task-level paired outcomes, deterministic seed, 1000
  resamples.
- Verdict follows pre-registered thresholds:
  - `promote` only if C−A gain >= `min_gain_pp`, lower CI bound > 0, and no
    safety regression; otherwise `reject`; insufficient samples -> `inconclusive`
    if the schema allows it, otherwise `reject` with a reason.
- Report must validate against `schemas/eval_report.schema.json`; add a
  field-by-field validator test (no new schema-validator dependency required).
- Markdown report mirrors the JSON, never introduces numbers not in JSON.

## Tests

- sealed test split has no public task/oracle accessor; `run_arm(Test)` for a
  candidate is `PolicyDenied`.
- same seed -> byte-identical CSV, oracle and split digests.
- tampering with a suite artifact changes its hash and fails integrity.
- three arms get the same task count/budget and the report records all three.
- report has schema fields, arms A/B/C, thresholds and a verdict.
- bootstrap CI is deterministic for a fixed seed and degenerate samples
  (all success / all failure) behave.
- worker isolation: env scrubbing, timeout kill, invalid stdout -> task failure.
- a solver that panics/exits non-zero does not abort the whole run.

Add `eval/acceptance/w5-01.toml` with one gate per test, `required = true`.
Add `docs/gates/w5-01.md`.

## Commands and commit

```bash
cargo fmt --all
cargo test -p silyn-eval
cargo clippy -p silyn-eval --all-targets -- -D warnings
cargo run -p xtask -- verify-eval-integrity
cargo run -p xtask -- verify-schemas
cargo run -p xtask -- verify-acceptance
```

Commit once on `wave5-eval` with
`feat(eval): add sealed isolated runner and paired statistics`.
