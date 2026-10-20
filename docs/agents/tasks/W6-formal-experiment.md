# W6: First formal experiment on `csv-stats-v1`

```yaml
id: W6
wave: 6
depends_on: [W5-01, W5-02]
owns:
  - docs/experiments/csv-stats-v1.md
  - eval/reports/csv-stats-v1-*.json
  - eval/reports/csv-stats-v1-*.md
reads:
  - docs/architecture/adr/0004-evaluation-protocol.md
  - eval/suites/csv-stats-v1/**
  - crates/silyn-eval/src/**
  - .env (never commit)
forbids:
  - eval/suites/** (read-only; no threshold changes)
  - migrations/**
  - crates/silyn-core/**
  - .env in git
contracts: [EvalReport v1]
```

## Goal

Run the pre-registered A/B/C experiment with the official DeepSeek API and
publish a reproducible report. The conclusion is one of: **pass**, **fail with
reason**, or **invalid experiment with defect**. Do not tune thresholds or the
suite after seeing results.

## Preconditions

- W5-01 suite is sealed (`sealed = true`), `suite.lock` hashes match, and
  `verify-eval-integrity` passes.
- W5-02 policy/candidate pipeline is merged.
- `.env` contains `DEEPSEEK_API_KEY`; model id is pinned in the report.
- Record `git rev-parse HEAD`, suite version, split digests, model id, price
  table, repeats, seed, and budget caps before the first call.

## Execution

1. Export the key without printing it. Do not use `set -x`.
2. Run the sealed test split with A/B/C, one repeat for the first run:
   `silyn-eval run --suite csv-stats-v1 --split test --arm all --repeats 1 --out eval/reports/...json --markdown eval/reports/...md`.
3. Enforce a hard budget: max tasks 10, max steps 8, max cost USD 2.00,
   max wall clock 10 minutes per task, no more than 3 candidates, no more than
   5 repeats.
4. Stop immediately on unauthorized actions, a suite hash mismatch, a model
   endpoint mismatch, or cost overrun.
5. Save the JSON and Markdown reports verbatim. Do not edit numbers.

## Report content

`docs/experiments/csv-stats-v1.md`:

- commit, date, model id, base URL, temperature, max tokens;
- suite version and train/dev/test digests;
- A/B/C configurations and equal-budget evidence;
- per-arm success, cost, duration, tool errors, unauthorized actions;
- paired gains with 95% bootstrap CI;
- pre-registered thresholds and verdict;
- failure/invalid-experiment reasons, if any;
- explicit statement that the sealed test set was not used for tuning.

## Verdict rules

- **pass** only if the pre-registered promotion threshold is met with a positive
  lower CI bound and zero unauthorized actions.
- **fail** if the point estimate misses the threshold or CI includes zero.
- **invalid** if the run violated isolation, hashes, budget or model pinning;
  record the defect and rerun only after a fix, with a new report.

## Acceptance

- `verify-eval-integrity` and `verify-schemas` pass after the run.
- The JSON report validates against `schemas/eval_report.schema.json`.
- The Markdown contains every field required above and no secrets.
- `git status` shows only the report files and the experiment document.
- A reviewer can reproduce the report from the recorded seed/hashes/commands.

## Commit

Commit once on `main` (integrator) with
`docs(experiment): record csv-stats-v1 deepseek run`, or on a dedicated
`wave6/experiment` branch if the report is generated there.
