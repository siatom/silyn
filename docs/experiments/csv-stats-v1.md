# Experiment: `csv-stats-v1` on DeepSeek (W6)

- **Status:** fail — no reproducible gain; the candidate was **not** promoted.
- **Date:** 2026-10-20 (UTC)
- **Git commit:** `29f5129`
- **Model requested:** `deepseek-chat`
- **Model reported by provider:** `deepseek-flash`
- **Endpoint:** `https://api.deepseek.com`
- **Temperature:** 0
- **Max output tokens:** 1024
- **Suite:** `csv-stats-v1.1` (`sealed = true`)
- **Repeats:** 1
- **Bootstrap:** 1000 resamples, 95% paired percentile CI, fixed seed
- **Backend:** in-process (`--in-process`); see caveats
- **Unauthorized actions:** 0 in every arm

## Protocol

Three arms differ only in the reference text given to the model. All use the
same model, budget, task order and output schema.

| Arm | Context |
|---|---|
| A | none (baseline) |
| B | three agent memories (units, missing values, duplicate rows) |
| C | B + one proposed planner prompt fragment |

The context file is committed at `eval/contexts/w6-csv-stats.json`; its sha256
and provenance are recorded in each statistics document. The candidate patch is
`eval/contexts/w6-candidate.json`; the candidate id is
`73b53360-af56-4f75-a416-42c50b55e875`.

The candidate was first evaluated on the **dev** split. It failed the
pre-registered gate (C−A = 0 pp, CI `[0, 0]`, verdict `reject`), so it entered
`awaiting_approval` and was **not** promoted. C therefore used the **proposed**
prompt fragment, not a promoted policy. No evolution claim is made.

## Results

### Dev split (selection set)

| Arm | Success | Total | Rate |
|---|---:|---:|---:|
| A | 6 | 10 | 60% |
| B | 7 | 10 | 70% |
| C | 6 | 10 | 60% |

| Comparison | Point | 95% CI |
|---|---:|---|
| B−A | +10 pp | [0, 30] |
| C−B | −10 pp | [−30, 0] |
| C−A | 0 pp | [0, 0] |

Verdict: `reject` — no C−A gain, negative C−B.

### Sealed test split

| Arm | Success | Total | Rate |
|---|---:|---:|---:|
| A | 9 | 10 | 90% |
| B | 8 | 10 | 80% |
| C | 6 | 10 | 60% |

| Comparison | Point | 95% CI |
|---|---:|---|
| B−A | −10 pp | [−40, 30] |
| C−B | −20 pp | [−60, 20] |
| C−A | −30 pp | [−60, 0] |

Verdict: `reject` — the policy arm regresses against the memory arm.

## Pre-registered thresholds

- C−A gain ≥ **+10 pp**
- lower 95% CI bound **> 0**
- cost ratio ≤ 2× the memory arm
- C−B regression ≤ 5 pp

None were met. The thresholds and suite were not changed after seeing the
results.

## Cost

The provider configuration had no price table, so the harness recorded
`cost_usd = 0.0` and `price_configured = false`. Token usage is recorded from the
provider:

- dev: 18,939 input / 840 output tokens across 30 calls
- test: 19,050 input / 840 output tokens across 30 calls

No dollar amount is claimed. The actual provider bill is outside this report.

## Reproduction

```bash
set -a; . ./.env; set +a

# dev (selection)
cargo run -p silyn-eval -- run \
  --split dev --arm all --solver model --in-process \
  --context eval/contexts/w6-csv-stats.json \
  --model deepseek-chat --base-url https://api.deepseek.com \
  --max-tokens 1024 --temperature 0 \
  --candidate-id 73b53360-af56-4f75-a416-42c50b55e875 \
  --out eval/reports/w6-dev.json --markdown eval/reports/w6-dev.md

# sealed test
cargo run -p silyn-eval -- run \
  --split test --allow-sealed --arm all --solver model --in-process \
  --context eval/contexts/w6-csv-stats.json \
  --model deepseek-chat --base-url https://api.deepseek.com \
  --max-tokens 1024 --temperature 0 \
  --candidate-id 73b53360-af56-4f75-a416-42c50b55e875 \
  --out eval/reports/w6-test.json --markdown eval/reports/w6-test.md
```

Suite digests (from `eval/reports/w6-test.stats.json`):

- train `798711e9e2fe9c40aaae3a68b8a7751d5e0e2f7c98daa41e344253013531ead8`
- dev `f4abda23f50c22e75f8577ced0d66bc8364dddfd17f3d15738ef8f4a9f497f13`
- test `960ccb10f1d4acee7cae85548818a0e1ca52fec62d907978736c0c47907eb040`

Report hashes:

- dev report `d4a2a88cae3fb30fe6138649f4307e5267f2ba5f500b33e064f9f46824f20cd9`
- dev stats `8e54c4a2435eaa97189fe25eb661085eae8ccd96edce6fd3fe687b60d8c07de7`
- test report `5aeb703c177c4918698f67dd9f8c4346ee8ee5d70a0c98eb0f6300f5447f4313`
- test stats `84fd087846f1457f8b1b39aecd1357abeeb31b54c23cabd4655d7f2d56ff0711`

## Caveats and invalidity risks

- **No promotion:** the candidate failed the dev gate and stayed
  `awaiting_approval`; C is a proposed policy, not an active one. This experiment
  does not demonstrate evolution.
- **In-process model solver:** the worker isolation contract cannot carry the
  provider key, so the real model ran in-process without an OS sandbox. Results
  are valid for the harness, not for hostile-candidate isolation claims.
- **Single repeat:** one repeat per task; the 10-task test split is underpowered
  for small effects.
- **Memory provenance:** the three memories were recorded through the W4 memory
  store with `external_evidence` provenance; they were not extracted from a
  training run or confirmed by two successful runs.
- **Provider aliasing:** the API reported `deepseek-flash` for the requested
  `deepseek-chat`; reports record both.
- **No cost gate:** without a price table the harness could not enforce the cost
  ratio threshold.

## Conclusion

The experiment is a **negative result**: with this task family, model and
context, neither memory nor the proposed prompt fragment improved sealed-test
accuracy, and the policy arm regressed. The correct action is to keep the base
policy, leave the candidate un-promoted, and design a better reflection/policy
candidate before another pre-registered run. Thresholds and the sealed split
must not be tuned to this outcome.
