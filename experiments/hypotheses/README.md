# Hypothesis format

A hypothesis is one candidate change to be validated against the current
`dev` baseline over a fixed holdout window (see
`docs/self-improvement-loop.md` for the full pipeline). Each file here is
one JSON hypothesis; the weekly routine feeds each one to its own
`isolation: "worktree"` agent.

## Schema

```json
{
  "name": "kebab-case-unique-id",
  "kind": "parameter" | "algorithm",
  "rationale": "Why this might help, in 1-3 sentences. Cite the specific
                observation from the weekly report that motivated it.",
  "trading_config": { /* full TradingConfig, see experiments/baseline-config.json */ },
  "code_change_summary": "Only for kind=algorithm. Plain-language description
                of the source change the worktree agent should make.",
  "constraints": [ "Only for kind=algorithm. Explicit boundaries — see below." ]
}
```

- `kind: "parameter"` — the worktree agent does not touch source code. It
  only uses the given `trading_config` (a full, `validate()`-passing
  `TradingConfig`) to run `shirube backtest-variant`. No PR is opened for
  a parameter hypothesis directly — see the pipeline doc; a promoted
  parameter hypothesis becomes a PR that updates the config the running
  instance loads (via `/api/config` or the DB-persisted default), not a
  source change.
- `kind: "algorithm"` — the worktree agent makes an actual source change
  (e.g. a new indicator, a new field on `IndicatorPoint`) described by
  `code_change_summary`, then still runs `shirube backtest-variant` with
  `trading_config` to score it. A promoted algorithm hypothesis becomes a
  real PR against `dev` with the code diff.

## `compute_btc_target` may be modified by `kind: "algorithm"` hypotheses

As of 2026-09-08, `TradingEngine::compute_btc_target`
(`src/trading/engine.rs`) is no longer categorically off-limits — a prior
hand-edit (`fbce478`) had replaced its multi-indicator formula with one
where the `sma` term canceled out algebraically, pinning the signal
inside a single allocation zone for an entire 30-day backtest and
producing exactly one trade for the whole window. That regression is why
the constraint below was relaxed: an automated hypothesis is now allowed
to fix or improve this function's body, subject to the same isolation
this pipeline already uses for every other algorithm hypothesis:

- The change is made only on that hypothesis's own dedicated
  `isolation: "worktree"` branch — **never** as a direct commit to
  `dev`/`main`.
- `cargo test` must pass, including the `compute_btc_target_*` unit tests
  in `src/trading/engine.rs` (update their expected values if the new
  formula legitimately changes them — don't weaken or delete a test just
  to make it pass).
- A promoted change still becomes its own PR against `dev` (per the
  pipeline's step 6) and is never merged automatically — a human reviews
  it like any other algorithm hypothesis. **Flag explicitly in the PR
  description whenever a diff touches `compute_btc_target`'s body**, so
  the reviewer gives it the scrutiny an allocation-formula change
  deserves.

Algorithm hypotheses may still prefer additive changes (new indicators,
new `IndicatorPoint` fields, changes to how indicators are
constructed/reset) when that's a sufficient fix — but a hypothesis whose
`rationale` specifically targets `compute_btc_target`'s own sub-signal
mappings or weighting formula (e.g. "the signal is stuck in one zone",
"trade count is too low") is now a valid `kind: "algorithm"` hypothesis
in its own right.

## Example files

- `example-parameter-tuning.json` — widens the Bollinger band width
- `example-algorithm-change.json` — adds a new indicator without touching
  the guarded allocation formula
