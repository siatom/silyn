# ADR 0004: Evaluation protocol

- Status: Accepted
- Date: 2026-10-20

## Context

The central claim is that Silyn improves on unseen tasks after experience. This
is only credible if the test set is sealed, budgets are equal across arms, and
thresholds are fixed before results are known.

## Decision

Task family for the MVP: synthetic CSV analysis. The generator injects controlled
traps (unit mismatch, missing values, duplicate rows, timezone, thousands
separators, column-name drift, encoding). The oracle is computed from generator
metadata, never from the system output.

Data splits:

- `train` — experience accumulation;
- `dev` — candidate selection;
- `test` — sealed; only the evaluation runner may read it, and its path is never
  given to candidate generation or reflection.

Arms, with equal model, tools, token/time budget and base prompt:

- A: no long-term memory;
- B: memory enabled, evolution disabled;
- C: memory enabled plus the promoted policy.

Protocol:

- 3–5 repeats per task; report paired bootstrap confidence intervals.
- Record model id (requested and reported by the provider), parameters, suite
  version, split hashes, budget and seeds.
- Pre-register thresholds in the suite manifest before the first run:
  `C - B >= +10pp` with CI lower bound > 0; safety/regression degradation
  `<= 5pp`; cost `<= 2x`; zero unauthorized actions.
- Thresholds are never adjusted after seeing results. A run that changes them is
  marked invalid.
- A negative result is a valid outcome and is published in `docs/experiments/`.

## Consequences

- The test set cannot be used for prompt tuning.
- Provider nondeterminism is handled by repetition and intervals, not by claims
  of exact reproducibility.
- The suite hash and thresholds are checked by `verify-eval-integrity`.

## Alternatives considered

- One-shot evaluation without repeats: rejected, model noise dominates.
- Report only point estimates: rejected, the claim is about a reliable direction.
