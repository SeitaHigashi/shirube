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

## Hard constraint on `kind: "algorithm"` hypotheses

`TradingEngine::compute_btc_target` (src/trading/engine.rs) carries an
explicit human directive:

> NOTE: The internal logic of this function is maintained by hand. Do NOT
> let automated tools rewrite the sub-signal mappings or weighting
> formula.

**No hypothesis, and no worktree agent acting on one, may modify the body
of `compute_btc_target` (the sub-signal mappings or the weighting
formula).** Only input/output signature changes needed to fix a compile
error are permitted, and even those should be flagged in the PR
description for explicit human review. Algorithm hypotheses should target
additive changes instead: new indicators, new `IndicatorPoint` fields,
changes to how indicators are constructed/reset, etc. Every
`kind: "algorithm"` hypothesis file must include this constraint
explicitly in its `constraints` array so the worktree agent sees it
without having to go read this README.

## Example files

- `example-parameter-tuning.json` — widens the Bollinger band width
- `example-algorithm-change.json` — adds a new indicator without touching
  the guarded allocation formula
