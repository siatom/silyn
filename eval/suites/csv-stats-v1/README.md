# csv-stats-v1

Synthetic CSV analysis suite for the MVP evaluation protocol
(`docs/architecture/adr/0004-evaluation-protocol.md`). The generator lives in
`crates/silyn-eval/src/generator.rs`; there are no checked-in fixtures because
every task is reproducible from its seed.

Suite version: `csv-stats-v1.1` (sealed). The suite is frozen by `suite.lock`;
changing any sealed artifact requires a new suite version.

## Task

Each task is a small CSV file plus a fixed objective. A solver returns JSON:

```json
{ "row_count": 4, "missing_total": 4, "duplicate_rows": 2, "unit": "kg" }
```

- `row_count` — distinct data rows; exact duplicate copies counted once.
- `missing_total` — empty cells across all data rows as written, duplicate
  copies included.
- `duplicate_rows` — rows that repeat an earlier row.
- `unit` — the actual unit of the mass column. The header is deliberately not
  authoritative: a task may claim `mass_lb` for values that are kilograms.

Scoring is exact: all four fields must match the oracle.

## Controlled traps

Every task carries at least one of each of these, selected by seed:

- unit mismatch between the mass header and the mass values (`unit_mismatch`);
- empty cells in mass, quantity or timestamp (`missing_values`);
- exact duplicate rows appended and shuffled into the file (`duplicate_rows`);
- thousands separators for values >= 1000, rendered as quoted CSV fields
  (`thousands_separators`);
- column-name drift: `mass_kg` / `weight_kg` / `Mass (kg)` / `MASS_KG`, plus
  varying quantity and timestamp names (`column_name_drift`);
- an optional UTF-8 byte order mark (`byte_order_mark`);
- timestamps always carry a timezone offset (`Z`, `+02:00`, `-05:00`, `+05:30`),
  which is noise for the counts.

Mass values are generated in kilograms (40.0–119.9) or pounds
(88.2–264.3, converted from the same range), so the unit can be inferred from
the values even when the header lies.

## Oracle

The oracle is written from the generator's row model *before* the CSV is
rendered. It is never parsed back out of the CSV, and the solver only ever
receives `TaskInput` (objective + CSV). `documented_example()` in the generator
is the hand-checked fixture used by the test suite.

## Splits

| split | tasks | use |
|---|---|---|
| train | 20 | experience accumulation |
| dev | 10 | candidate selection |
| test | 10 | sealed; only `SealedTestSet` |

Seeds are fixed per split and recorded in `suite.toml` as sha256 digests over
task ids, inputs and oracles. `silyn-eval hashes` prints the computed digests and
fails when the manifest is stale.

## Runner

`silyn-eval run` executes one or all arms through an isolated worker:

```bash
silyn-eval run --suite csv-stats-v1 --split dev --arm all --repeats 1 \
  --out report.json --markdown report.md
```

- The default backend re-executes `silyn-eval worker` per task with a cleared
  environment, a fresh temporary working directory and a wall-clock timeout. A
  timeout, non-zero exit, panic or non-JSON stdout is a task failure, never a
  harness crash.
- `--arm a|b|c|all` selects arms; only `all` writes a report.
- `--repeats` stays within 1–5 and the seed is recorded in the statistics
  document.
- `--out` is the schema-valid `EvaluationReport` v1. Seeds, budgets, durations
  and the paired bootstrap intervals that schema v1 has no fields for are written
  to `<out>.stats.json` (override with `--stats-out`).

The test split is refused unless the suite declares `sealed = true` **and** the
operator passes `--allow-sealed`; the runner then verifies `suite.lock` before
loading the sealed tasks.

## Sealing

`suite.lock` lists the sha256 of `suite.toml` and `README.md` only; fixtures are
reproducible from the generator and are not sealed directly. Any change to a
listed file fails `cargo run -p xtask -- verify-eval-integrity` and a sealed run,
until a new suite version is created. Regenerate the digests with
`cargo run -p silyn-eval -- hashes` when preparing a **new** suite version,
never in place.

## Arms

All three arms run the same solver, model, task order, budget and price; only
the recorded capability flags differ (A: none, B: memory, C: memory plus the
promoted policy). In W5 the baseline solver is used for all three, so the honest
verdict is `reject` until W6 supplies real memory and policy arms.

`BaselineSolver` counts rows, duplicates and empty cells mechanically but trusts
the header for the unit. It therefore fails every unit-mismatch task (~half the
split) and never fabricates a success: parse errors are returned as errors.
