# Self-improvement loop: worktree-parallel variant validation

This describes the automated pipeline that periodically proposes, tests,
and PRs improvements to the trading logic while `shirube` runs
continuously in paper-trading mode. It is meant to be followed by a
scheduled Claude Code agent (see "Scheduling" below) — every step here is
either a shell command or an `Agent` tool call with concrete inputs, not
a vague instruction.

## Why this design

- **Backtest, not live paper trading, for comparison.** Live paper
  trading can't be parallelized fairly (variants would see different
  market conditions) and takes days to produce a signal. A backtest over
  a fixed historical window is deterministic and takes seconds, so many
  variants can be evaluated per cycle. Live paper trading is reserved for
  final validation of an already-promoted change.
- **Same code path for backtest and live.** `Simulator::run` calls
  `TradingEngine::compute_btc_target` and `allocation_delta_to_order`
  directly (both `pub(crate)`) instead of a re-implementation, so a
  backtest result cannot silently drift from what live trading would
  have done for the same config.
- **Holdout split to avoid overfitting.** A variant may only be
  *authored* by looking at data older than the holdout window. Promotion
  decisions are made exclusively on the holdout window, which both the
  baseline and every candidate share.

## Fixed parameters (see prior discussion; change here if revisited)

| Parameter | Value |
|---|---|
| Holdout window | last 14 days |
| Total lookback | 30 days (16 days train + 14 days holdout) |
| Promotion rule | Sharpe ratio improves >= 10% relative (or >= 0.1 absolute if baseline Sharpe <= 0) **AND** max drawdown does not worsen **AND** candidate trade count >= 50% of baseline's |
| PR granularity | one PR per promoted variant |
| Backtest resolution | 3600s (1h) candles, adjust via `--resolution-secs` if a finer/coarser view is needed |

The promotion rule is implemented in `backtest::report::compare` (see
`src/backtest/report.rs`) — it is not re-derived by the agent, only
invoked via `shirube compare-backtest`.

## Hard constraint: `compute_btc_target` is off-limits

`src/trading/engine.rs`'s `TradingEngine::compute_btc_target` carries an
explicit human directive not to let automated tools rewrite its
sub-signal mappings or weighting formula. **No step in this pipeline may
edit the body of that function.** See
`experiments/hypotheses/README.md` for how this constrains
`kind: "algorithm"` hypotheses. A human wanting to revise the allocation
formula itself does so by hand, outside this pipeline.

## CLI building blocks

Two `shirube` subcommands exist specifically for this pipeline (see
`src/cli.rs`):

```bash
# Run a backtest for one TradingConfig over one time range, print a
# BacktestReport as JSON to stdout.
shirube backtest-variant \
  --db shirube.db \
  --config path/to/trading_config.json \
  --from 2026-08-24T00:00:00Z --to 2026-09-07T00:00:00Z \
  [--product BTC_JPY] [--resolution-secs 3600] \
  [--initial-jpy 1000000] [--slippage-pct 0.001] [--fee-pct 0.0015]

# Compare two BacktestReport JSON files, print a Pros/Cons verdict to
# stderr and a BacktestComparison JSON (with `promoted: bool`) to stdout.
shirube compare-backtest --baseline baseline.json --candidate candidate.json

# Print TradingConfig::default() as JSON (used to seed
# experiments/baseline-config.json).
shirube print-default-config
```

`experiments/baseline-config.json` holds the config the currently-running
instance is assumed to use. If the live DB-persisted config
(`ConfigRepository`) has drifted from this file, refresh it before a
cycle: query `GET /api/config` on the running instance and overwrite
`experiments/baseline-config.json` with the result.

## Weekly procedure

Let `HOLDOUT_START = now - 14d`, `NOW = now` (UTC, ISO 8601).

### 1. Compute the baseline report

```bash
shirube backtest-variant --db shirube.db \
  --config experiments/baseline-config.json \
  --from $HOLDOUT_START --to $NOW \
  > /tmp/baseline.json
```

### 2. Run each hypothesis in its own worktree, in parallel

**Important — verified 2026-09-07:** `isolation: "worktree"` branches
from the repository's default branch (`main` here), *not* from whatever
branch the coordinator session currently has checked out. Since
`src/backtest/`, `src/cli.rs`, and `experiments/` only exist on `dev`,
every worktree agent's prompt **must** explicitly tell it to rebase onto
`dev` before doing anything else, or `shirube backtest-variant` won't
exist yet and the agent will silently fall through into normal server
startup (which opens a real WebSocket connection and starts placing mock
orders — confirmed by a dry run; the agent has to be killed if this
happens, since `backtest-variant`'s argument parsing is skipped
entirely on `main`). Always include step 0 below in the prompt.

For every `experiments/hypotheses/*.json` file (excluding `README.md`),
launch one `Agent` call with `isolation: "worktree"`, all in a single
message so they run in parallel. Each agent's prompt should say, in
substance:

> 0. `git fetch origin && git rebase origin/dev`. Resolve any conflicts,
> preferring `dev`'s current structure over anything you might otherwise
> assume from `main` (`dev` may have refactored the signal/trading
> architecture since this doc was written — adapt to whatever
> `IndicatorPoint`/`compute_indicators()`/`SignalEngine` actually look
> like on `dev`, not what's described here). Run `cargo build` to confirm
> the rebase compiles before continuing.
> 1. Read `experiments/hypotheses/<name>.json`. If `kind` is `"algorithm"`,
> implement `code_change_summary` exactly, respecting every item in
> `constraints` (in particular: never edit the body of
> `TradingEngine::compute_btc_target`). Run `cargo test` and fix any
> failure caused by your change before continuing — do not proceed on a
> red test suite.
> 2. Write the hypothesis's `trading_config` field to a temp JSON file.
> Run `cargo build` then
> `shirube backtest-variant --db <path-to-a-copy-or-the-shared-read-only-db> --config <temp-config.json> --from <HOLDOUT_START> --to <NOW>`
> using the exact same `HOLDOUT_START`/`NOW` as the baseline run.
> 3. Report back: the hypothesis name, the full stdout JSON report,
> whether `cargo test` passed, and (for algorithm hypotheses) the
> worktree path and branch name.

Because backtests only read historical tickers (never place real
orders), every worktree agent can safely point at the same
`shirube.db` file read-only, or a copy — either works since
`Simulator` only ever talks to a `MockExchangeClient`.

### 3. Aggregate and decide

For each variant report collected in step 2:

```bash
shirube compare-backtest --baseline /tmp/baseline.json --candidate /tmp/variant-<name>.json
```

Collect the `promoted: true` results.

### 4. Open PRs for promoted variants, clean up the rest

For each **promoted** variant:
- `kind: "parameter"` — open a PR against `dev` that updates
  `experiments/baseline-config.json` to the new values (and, in the PR
  description, note that an operator must also apply the same values via
  `PUT /api/config` on the running instance — there is no auto-apply
  endpoint). Include the `compare-backtest` Pros/Cons text and the full
  comparison JSON in the PR description.
- `kind: "algorithm"` — push the worktree's branch and open a PR against
  `dev` with the code diff. Include the same Pros/Cons text and JSON.
  Flag explicitly in the PR description if the diff touched
  `compute_btc_target`'s signature at all (it must not touch its body).

For each **rejected** variant, delete its worktree and branch — nothing
to PR:

```bash
git worktree remove <worktree-path> --force
git branch -D <worktree-branch>
```

One PR per promoted variant (never bundle multiple variants into one
PR), so a bad promotion can be reverted independently.

### 5. Human review

All PRs land on `dev` per the branch strategy in `CLAUDE.md` — nothing
in this pipeline merges automatically. A human reviews and merges (or
requests changes / closes) each PR normally.

## Scheduling

Register this procedure as a weekly cron routine with the `/schedule`
skill, pointing its prompt at this document (`docs/self-improvement-loop.md`)
so the scheduled agent re-reads the current version each run rather than
having the steps baked into the routine's own prompt text.
