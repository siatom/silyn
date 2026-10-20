---
id: W2-04
wave: 2
depends_on: [W1-01]
owns:
  - eval/suites/csv-stats-v1/generator/**
  - eval/suites/csv-stats-v1/oracle/**
  - eval/runners/**
  - eval/acceptance/registry.toml
reads:
  - schemas/suite_manifest.schema.json
  - schemas/eval_report.schema.json
  - docs/architecture/adr/0004-evaluation-protocol.md
forbids:
  - crates/silyn-core/**
  - crates/silyn-runtime/**
  - migrations/**
contracts: [SuiteManifest v1, EvaluationReport v1]
acceptance_registry:
  - eval.oracle_matches_generator_metadata
  - eval.split_hashes_are_stable
  - eval.test_split_not_exposed
commands:
  - cargo test -p xtask
  - cargo run -p xtask -- verify-eval-integrity
---

# W2-04 Evaluation harness skeleton (arm A)

## Goal

Build the synthetic CSV task family and a single-arm evaluation runner. Arm A
(no long-term memory) must run end to end with the mock provider.

## Requirements

- Generator is deterministic given a seed; it records metadata for every task.
- Traps: unit mismatch, missing values, duplicate rows, timezone, thousands
  separators, column-name drift, encoding.
- Oracle is computed from generator metadata only.
- Splits: train/dev/test with recorded counts and sha256; test is sealed.
- Runner uses a temporary database and working directory; it never touches the
  development database.
- Emit a report matching `schemas/eval_report.schema.json` (arm A only for now).
- Register the acceptance tests above with `required = true`.

## Non-goals

- Arms B/C, candidate evaluation, promotion (W5).

## Acceptance

- Same seed produces identical fixtures and oracle values.
- Corrupting a fixture makes the hash check fail.
- Report validates against the schema.
- Running twice leaves no state behind.
