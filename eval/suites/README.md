# Evaluation suites

Each suite is a directory:

```
eval/suites/<suite-id>/
  suite.toml        # SuiteManifest (see schemas/suite_manifest.schema.json)
  suite.lock        # sha256 of every sealed artifact, one "<hash>  <path>" per line
  generator/        # task generator and oracle (fixtures are reproducible)
  train/ dev/ test/ # task inputs; test is sealed
```

Rules:

- `test/` is only readable by the evaluation runner. Candidate generation and
  reflection never receive its path.
- `suite.lock` is written once, before the first real evaluation. Any change to a
  sealed artifact fails `cargo xtask verify-eval-integrity`.
- Thresholds are pre-registered in `suite.toml` and cannot be edited after seeing
  results. A changed threshold invalidates the report.
- Oracles are computed from generator metadata, never from the system under test.
