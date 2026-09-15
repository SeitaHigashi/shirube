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
| Cadence | daily, 04:00 JST (19:00 UTC previous day) |
| Price data | bitFlyer public execution history via `shirube backfill-executions`, accumulated across runs in the `backtest-data` release asset (`shirube backtest-data pull\|push`) |
| Holdout window | last 14 days |
| Total lookback | **30 days** (16 days train + 14 days holdout) — a floor, not a ceiling: the carried-over DB grows past bitFlyer's 31-day retention by ~1 day per run, see "Why the DB is carried over" below |
| Promotion rule | Sharpe ratio improves >= 10% relative (or >= 0.1 absolute if baseline Sharpe <= 0) **AND** max drawdown does not worsen **AND** candidate trade count >= 50% of baseline's |
| PR granularity | one PR per promoted variant |
| Backtest resolution | **60s (1m) candles** — matches live trading; see "Resolution must match live" below |
| Evaluation capital | every backtest is run at **50,000 / 500,000 / 5,000,000 JPY**; promotion is decided at **50,000 JPY** alone, the other two are reported diagnostics with no veto — see "Evaluation capital" below |
| Benchmark | every run reports 100% buy-and-hold and a matched-exposure static mix; **reported and warned on, never a promotion gate** — see "Benchmarks" below |
| New hypotheses generated per run | at most 3 (see "Hypothesis generation" below) |
| Trade frequency | **non-binding guideline**: roughly 4-10 trades/day (see below) |

The promotion rule is implemented in `backtest::report::compare` (see
`src/backtest/report.rs`) — it is not re-derived by the agent, only
invoked via `shirube compare-backtest`.

### Data source: bitFlyer executions, not CoinGecko

Until 2026-09-09 this pipeline seeded its backtest DB from CoinGecko's
hourly `market_chart` endpoint. That was replaced because the series was
structurally unable to test most of what the loop proposes:

- Every row had `open == high == low == close` (2161/2161 rows measured),
  so bars carried **no intra-bar range at all**.
- `volume` was the constant `1` for every row, and `best_bid == best_ask`,
  so there was **no turnover and no spread**.
- The price was a cross-exchange volume-weighted **aggregate** (and
  `vs_currency=jpy` is a USD price converted by an FX rate), not
  bitFlyer's own market. Cross-exchange averaging smooths away
  exchange-specific noise, understating realized volatility and thereby
  inflating Sharpe.

The concrete damage: `add-volume-weighted-ma-indicator` was recorded in
`experiments/tried.json` as rejected with `sharpe_improvement_pct: 0.0`.
That is not a verdict on the idea — on constant volume a VWMA is
*identically* an SMA, so 0.0 was arithmetic, not evidence. Any hypothesis
touching range, volume, or spread was in the same position.

The replacement is `shirube backfill-executions`, which pages bitFlyer's
public execution tape (`GET /v1/getexecutions`) and buckets the prints
into real OHLCV bars for the exact market the bot trades. See
`src/backtest/backfill.rs`.

**The 31-day retention wall.** bitFlyer serves only the most recent 31
days of public executions; anything older returns HTTP 400 with status
-156. This is what caps the lookback at 30 days (31 pulled, one day of
slack ahead of `--from` for indicator warmup) — the window is *rolling*,
so history not captured today is gone tomorrow. If a real `shirube.db`
export from the running instance ever becomes available, prefer it: the
live DB accumulates past the 31-day wall and is the only path to a
longer lookback.

Backfilling 31 days takes roughly 10-12 minutes: ~900 requests paced at
~80 req/min. The pacing is deliberate — `RateLimiter::new(200)` starts
with a full bucket and will burst 200 requests, which trips bitFlyer's
public IP limit (~500 requests / 5 minutes) and returns status -1.
`backtest::backfill` paces itself and retries -1 under exponential
backoff.

### Why the DB is carried over

bitFlyer's 31-day retention wall caps what a *single* backfill can reach,
but not what the pipeline can accumulate. Bars are written
`INSERT OR IGNORE` on `UNIQUE(product_code, timestamp)`, so re-running
`backfill-executions` against an existing DB extends it forward and never
rewrites stored history. Verified 2026-09-09: re-running `--days 1`
against a 33,184-bar DB left the oldest bar untouched, advanced the
newest, added 179 rows, and produced zero duplicate timestamps.

So a DB that survives between runs grows past the wall: 31 days at
bootstrap, then roughly one more day per daily run. The lookback in
"Fixed parameters" is therefore a **floor that rises over time**, not a
permanent ceiling — after a few months the holdout can be widened, which
is the single most valuable thing that can happen to this pipeline's
statistical power. Revisit the 30-day figure once the stored DB comfortably
exceeds it.

Carrying it over also makes the daily run cheap: `--days 2` takes about 40
seconds instead of the 10-12 minutes a full 31-day pull needs.

**Why a release asset.** The DB is generated data that grows, so it does
not belong in git history; Actions cache evicts after 7 days; and object
storage would need credentials the routine does not have. A release asset
is outside git history, is served by GitHub, needs nothing beyond the
routine's existing token, and allows 2GB per file (currently 1.3MB gzipped
from a 7MB DB, growing roughly 300KB per month).

**Why the tag is `backtest-data` and must stay non-semver.** `self_update`'s
`update()` calls `get_latest_releases()`, which lists *every* release —
including prereleases — and filters with
`bump_is_greater(current, &r.version).unwrap_or(false)`. A non-semver tag
fails that parse and is dropped, so the auto-updater on the live trading
instance cannot see it. Renaming this tag to anything semver-shaped would
offer a running instance a bogus "release" to install. The release is also
marked prerelease so it never appears as "Latest"; verified 2026-09-09 that
`/releases/latest` still resolves to `v0.1.109`.

**The asset is public**, because the repo is. That is acceptable here: the
contents are bitFlyer's public trade prints, which are already public
information. Do not extend this mechanism to carry anything account-derived
(order history, balances, or a `shirube.db` exported from the live
instance) — that would publish private trading activity.

### Resolution must match live

`TradingConfig`'s indicator periods are counts of **1-minute bars**
(`BASE_RESOLUTION = 60` in `src/api/routes/indicators.rs`). Running the
backtest at 3600s, as this pipeline did until 2026-09-09, silently
reinterpreted every period as hours: `sma_period: 200` meant a 3.3-hour
SMA live but an **8.3-day** SMA in the backtest. The loop was therefore
optimizing a materially different strategy from the one it shipped, and
no amount of holdout discipline could have caught that — both sides of
every comparison shared the same distortion.

`--resolution-secs` now defaults to 60 in `shirube backtest-variant`.
Do not raise it for the daily cycle. An ad-hoc coarser run is fine for
eyeballing a long-horizon effect, but a promotion decision made at any
resolution other than 60s does not transfer to live trading.

### Benchmarks: is the bot worth running at all?

The promotion rule compares a candidate against the current baseline. That
is the right gradient for making the loop converge, but it says nothing
about whether the whole strategy beats doing nothing — a loop optimizing
variant against variant can climb a hill that is below sea level. Every
backtest therefore also computes two benchmarks over the identical
evaluated window, at the same capital, paying the same slippage and a
single entry commission:

- **100% buy-and-hold** — one buy at the first evaluated candle, then hold.
- **Static mix** — the same, but sized at the run's own
  `avg_btc_exposure`, holding the rest as JPY.

**The static mix is the fair comparison.** This strategy is an allocation
model that holds ~49.8% BTC on average, so measuring it against 100%
buy-and-hold compares two different risk levels and mostly measures
exposure, not skill. Matching the exposure isolates whether the *timing*
earned anything. `excess_return_vs_static_mix_pct` and
`sharpe_minus_static_mix` are the headline numbers: positive means the
trading paid for itself.

What this measures on merged `dev` (2026-08-10 .. 2026-09-09, real
1-minute bars, benchmarks net of the same slippage and a single entry
commission), re-measured 2026-09-12 after PR #14 (`97b0e0c`) made
slippage an actual round-trip cost:

| | Return | Sharpe | Max DD | Trades |
|---|---|---|---|---|
| 100% buy-and-hold | +17.23% | 6.14 | 7.67% | 1 |
| Static mix @ exposure 0.499 | +8.59% | 5.84 | 4.27% | 1 |
| Strategy, zero fee | **-9.63%** | **-7.19** | 12.42% | 1607 |
| Strategy @5,000,000 JPY | -12.77% | -9.68 | 13.82% | 1605 |
| Strategy @1,000,000 JPY | -15.51% | -11.98 | 15.67% | 1606 |
| Strategy @500,000 JPY | -17.76% | -13.92 | 17.90% | 1606 |
| Strategy @50,000 JPY | -5.00% | -3.43 | 7.61% | 293 |

Read that carefully, because it is the single most important fact this
pipeline has established, and it is the opposite of what this section
claimed until 2026-09-12. **The timing logic has no measured edge — it
destroys value even before fees.** Against its own exposure-matched
benchmark the *zero-fee* strategy loses **18.30pp of return and 13.03 of
Sharpe**, at nearly three times the drawdown (12.42% vs 4.27%). There is
no gross alpha here to protect.

**Why the earlier figure was wrong, and by how much.** This section
previously reported the zero-fee strategy at +10.87% / Sharpe 7.36 and
concluded it earned "+1.16pp of return and +0.94 of Sharpe" against the
static mix. That was measured while `Simulator::run` set bid, ask and the
mark-to-market price all to `close * (1 + slippage_pct)`, so a round trip
at an unchanged price cost exactly nothing — the strategy paid no
slippage on any of its ~1,600 trades while the benchmark paid it on its
single entry. The comparison was therefore structurally rigged in the
strategy's favour. The correction is ~19.5pp of return, which is what the
arithmetic predicts independently: turnover of roughly 205x equity over
the window at 0.1% one-way is ~20.5pp of cost the old model never
charged.

**Fees are no longer the headline; turnover itself is.** Going from zero
fee to the 500,000 JPY tier costs a further 8.13pp of return and 6.73 of
Sharpe, which is real and worth reducing — but it is now the *second*
problem. The first is that trading at all, at this turnover, loses to
holding the same average exposure. At every capital level the strategy
loses to a static mix that requires exactly one trade, on return *and* on
Sharpe, and it does so by a margin far larger than any fee tier explains.

The implication for hypothesis generation has changed accordingly, and
this matters more than any individual variant result: **there is no
measured gross alpha to protect, so cost reduction alone cannot make this
strategy profitable.** A hypothesis that only trims turnover moves the
result toward the static mix at best. Until some variant demonstrates
positive `excess_return_vs_static_mix_pct` at zero fee, the honest
description of this system is an allocation model that has not yet been
shown to beat its own exposure held statically.

One caveat on the levels, not on the conclusion: these figures come from
the `backtest-data` release snapshot (33,452 bars, ending
2026-09-09T09:11Z), which is a slightly sparser series than the local DB
the 2026-09-11 study used — hence buy-and-hold reading +17.23% here
versus +19.33% there. Every row above moves together with that, so the
strategy-versus-benchmark *gaps*, which are what the conclusion rests on,
are unaffected.

**Why this is not a promotion gate.** The measurement above covers one
30-day bull window. Static long exposure is structurally strong in a
rising market, so "must beat the benchmark" would promote nothing in a
bull window and almost anything in a bear one — the gate would measure the
regime rather than the strategy. Promotion therefore stays exactly as
defined in "Fixed parameters": candidate versus baseline, on Sharpe,
drawdown and trade count.

**The warning rule.** When the *baseline's* `sharpe_minus_static_mix` is
negative, the day's report must open with a `> **WARNING**` block stating
the deficit, before the baseline section. If it is negative for **three
consecutive runs**, the report must additionally state that the running
instance is currently worse than a one-trade static allocation, and at
least one of that run's generated hypotheses must target trading cost
(turnover, fee tier, or the minimum-notional path) rather than signal
quality. This is the one place where a reported diagnostic is allowed to
constrain what gets proposed — it constrains *hypothesis generation*,
never the promotion verdict.

### Evaluation capital: three levels, one decides

Every backtest this pipeline runs — the baseline and every variant — is
run three times, at `--initial-jpy` **50,000**, **500,000** and
**5,000,000**. The runs share one DB and take seconds each, so the sweep
is nearly free.

**Promotion is decided at 50,000 JPY only.** That is the operator's
current account size. The other two levels are recorded in the report as
robustness diagnostics and have **no veto power whatsoever** — a variant
that clears the promotion rule at 50,000 JPY is promoted even if it looks
worse at 5,000,000 JPY, exactly as with the trade-frequency guideline
below. 500,000 JPY is the stated near-term target size and is the most
informative of the two diagnostics.

Why three levels rather than one, and why the results are not
interchangeable (all figures measured 2026-09-11 over
2026-08-10 .. 2026-09-09; see
`experiments/reports/2026-09-11-capital-and-fee-study.md`):

- **Above the lot-size floor, capital only changes the fee tier.** The
  allocation logic is percentage-based, so with the fee rate pinned, runs
  at 500,000 and 5,000,000 JPY agree to six decimal places. What differs
  in a normal (tiered-fee) run is solely the bitFlyer commission tier the
  account's turnover reaches: 0.0174% effective at 5,000,000 JPY versus
  0.1106% at 50,000 JPY, a factor of 6.4 for the identical strategy.
- **At 50,000 JPY the exchange lot size, not the config, is the gate.**
  `allocation_delta_to_order` returns `None` when the computed size falls
  under `min_order_size` (0.001 BTC). At ~12.2M JPY/BTC that is ~12,224
  JPY of notional — 24.4% of a 50,000 JPY portfolio. So on the primary
  capital, **every `allocation_threshold` below ~0.245 is inert**:
  thresholds 0.05, 0.10 and 0.15 produced byte-identical reports. The
  same floor is 2.4% at 500,000 JPY and 0.24% at 5,000,000 JPY.

That second point is a trap for hypothesis generation, and it has the
same shape as the CoinGecko constant-volume VWMA recorded in
`tried.json`, where a 0.0 Sharpe delta was arithmetic rather than
evidence. **A parameter hypothesis that only moves `allocation_threshold`
below ~0.245 cannot be tested at 50,000 JPY** — it will score identically
to the baseline no matter how good the idea is. When a candidate's
primary-capital report is identical to the baseline's, check this before
recording it as rejected, and say so in the report rather than filing a
verdict the data cannot support.

Because the lot-size floor also throttles turnover, the three levels do
not even share a break-even fee: 50,000 JPY turns over ~91x equity per 30
days against a ~0.114% break-even, while 500,000 JPY and above turn over
~205-216x against ~0.051%. Quote the capital alongside any turnover or
break-even figure.

### Trade frequency is a guideline, not a criterion

A rough target of **4-10 trades per day** is a useful sense of the
activity level this strategy is meant to operate at — enough turnover to
react to intraday moves, not so much that fees and slippage dominate.
Over a 30-day backtest that corresponds to about 120-300 trades. The
baseline measured on real bitFlyer 1-minute bars (2026-09-09) sits at
**2692 trades over 30 days (~90/day)** — an order of magnitude *above*
the range, and the opposite of the problem the old CoinGecko/1h baseline
appeared to have (173 trades over 90 days, ~1.9/day). That earlier figure
is not comparable and should not be cited: at 1h resolution the strategy
could only act 24 times a day, so its low turnover was an artifact of the
data, not of the allocation logic.

Turnover this high is the most obvious hypothesis source available right
now — at ~90 trades/day, fees and slippage plausibly account for the
baseline's negative return (`total_return_pct: -0.25`, `sharpe_ratio:
0.015`, `win_rate: 0.74` — many small wins outweighed by fewer large
losses, which is what a cost-dominated strategy looks like). A
`allocation_threshold` or rebalance-spacing hypothesis is the natural
first thing to test. As always the backtest decides, not the guideline.

This number carries **no force whatsoever**:

- It is **not** part of the promotion rule and must never be added to
  `backtest::report::compare`. Promotion is decided solely by the
  Sharpe / drawdown / trade-count-ratio criteria in the table above.
- A variant landing outside 4-10 trades/day is **not** disqualified. If
  it clears the promotion rule, it gets promoted — results are what
  count, unconditionally.
- A variant landing inside the range earns **no** credit for that alone.
  Hitting the target while failing the promotion rule is still a
  rejection.

Its only role is as an **idea source during hypothesis generation**: when
the current trade count sits well outside the range, that is a hint worth
turning into a testable hypothesis (e.g. a distance-to-zone-boundary or
signal-threshold parameter that would plausibly raise or lower turnover).
The backtest then decides whether that idea was any good. Record the
observed trades/day in each run's report so the trend stays visible, but
never let it override a backtest result in either direction.

## `compute_btc_target` may be modified via algorithm hypotheses

`src/trading/engine.rs`'s `TradingEngine::compute_btc_target` used to be
categorically off-limits to this pipeline. That was lifted on 2026-09-08
after a hand-edit (`fbce478`) had silently degraded it into a formula
where the `sma` term canceled out algebraically, pinning the signal in
one allocation zone for 30 days straight and producing exactly one
trade — a regression only backtesting caught. See
`experiments/hypotheses/README.md`'s "`compute_btc_target` may be
modified" section for the exact rules: the change must happen only on
that hypothesis's own worktree branch, `cargo test` must pass, and a
promoted change still lands as its own PR against `dev` for human
review, with the PR description explicitly flagging that
`compute_btc_target`'s body was touched. A human is of course still free
to revise the formula by hand outside this pipeline at any time.

## Enabling changes: the loop may fix and extend the program itself

Everything above describes changes that *are* the experiment — a variant
whose value is decided by the backtest. But improving the strategy
sometimes requires changing the program first: a capability that does not
exist yet, or a defect that makes the measurements themselves wrong.
Those changes cannot be scored by the promotion rule (a bug fix has no
Sharpe delta of its own, and a missing capability cannot be backtested
until it exists), so they need their own route.

**Standing authorization:** when a needed feature is missing or a defect
is blocking or distorting the work, the agent may implement it on its own
judgement and open a PR, without a hypothesis file and without waiting to
be asked. This is expected behavior, not an exception to apologize for.

Scope is the whole repository, not just the signal/allocation path.
Legitimate targets include `src/risk/`, `src/storage/`, `src/backtest/`
(the harness itself), `src/config.rs` (including adding new
`TradingConfig` fields, with `validate()` and the DB round-trip updated
to match), the `shirube` subcommands in `src/cli.rs`, and this document.

### Rules

- **Own branch, own PR, against `dev`.** Never a direct commit to `dev`
  or `main`, and never bundled into a hypothesis's variant PR — an
  enabling change must be reviewable and revertible on its own. If a
  hypothesis needs the change to run at all, land it as a separate PR and
  say in the hypothesis PR that it depends on it.
- **`cargo test` must pass**, and the change carries a regression test
  whenever the behavior is testable. Never weaken or delete an existing
  test to make a change pass.
- **Not gated on the promotion rule.** These PRs are judged on
  correctness by a human reviewer, not on Sharpe/drawdown, so they are
  opened whether or not any variant was promoted that run.
- **Quantify the effect on past results when a fix invalidates them.** A
  defect in the measurement path means earlier reports were wrong; state
  in the PR description what the numbers were and what they become, so
  the historical reports under `experiments/reports/` can be read with
  that correction in mind.
- **Stay proportionate.** Fix what blocks or distorts the current work
  and things found directly adjacent to it. This authorization is not a
  mandate for unrelated refactors, dependency upgrades, or style passes.
- **Anything touching live trading behavior gets flagged explicitly** in
  the PR description — order placement, risk gates, and the auto-updater
  affect real money on the running instance and deserve the same scrutiny
  as a `compute_btc_target` change.
- **Record it in the run's report** under "Enabling changes" (see the
  report template in step 8), so the trail stays visible even when no
  hypothesis was promoted.

Two real examples, both found by a human on 2026-09-09 and exactly the
kind of thing this section exists to let the loop catch itself:
`TickerRepository::get_aggregated` silently capped `limit: None` at 1000
rows, so every backtest longer than ~41.7 days at 1h resolution
discarded its newest candles and a 60-day and 90-day run returned
byte-identical reports (`a1fdc36`); and `calculate_sharpe` hardcoded a
60-second annualization factor, inflating every 1h backtest's Sharpe by
sqrt(60) ≈ 7.75x (`1d016b4`). Neither could have become a PR under the
promoted-variants-only rule in step 6.

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
  [--product BTC_JPY] [--resolution-secs 60] \
  [--initial-jpy 1000000] [--slippage-pct 0.001] [--fee-pct 0.0015]

# Compare two BacktestReport JSON files, print a Pros/Cons verdict to
# stderr and a BacktestComparison JSON (with `promoted: bool`) to stdout.
shirube compare-backtest --baseline baseline.json --candidate candidate.json

# Print TradingConfig::default() as JSON (used to seed
# experiments/baseline-config.json).
shirube print-default-config

# Print the canonical content_hash of one or more hypothesis files, in
# "<sha256-hex>  <path>" form — the dedup key for experiments/tried.json
# (see "Tried-hypothesis registry" below).
shirube hypothesis-hash experiments/hypotheses/*.json
```

`experiments/baseline-config.json` holds the config the currently-running
instance is assumed to use. If the live DB-persisted config
(`ConfigRepository`) has drifted from this file, refresh it before a
cycle: query `GET /api/config` on the running instance and overwrite
`experiments/baseline-config.json` with the result.

## Tried-hypothesis registry

`experiments/tried.json` is a JSON array tracking every hypothesis this
pipeline has ever run, so the same idea is never backtested twice:

```json
[
  {
    "name": "wider-bollinger-band-2.5std",
    "content_hash": "sha256 hex of the hypothesis file's `trading_config` + `kind` + `code_change_summary`",
    "tried_at": "2026-09-08T19:00:00Z",
    "verdict": "rejected",
    "sharpe_improvement_pct": 0.0,
    "report_path": "experiments/reports/2026-09-08.md"
  }
]
```

`content_hash` (not just `name`) is the dedup key — hash the hypothesis
file's `trading_config` object, `kind`, and `code_change_summary` (if
present) with sha256. This means renaming a hypothesis file doesn't let
it bypass the registry, but genuinely changing its `trading_config`
values (a real new variant) does get a fresh hash and is eligible again.

**Do not compute this by hand — use the CLI:**

```bash
shirube hypothesis-hash experiments/hypotheses/*.json   # prints "<hash>  <path>" per file
```

The exact serialization is pinned in `cli::hypothesis_content_hash`
(`src/cli.rs`) with unit tests: sha256 of the compact JSON encoding of
`{"code_change_summary": …, "kind": …, "trading_config": …}` with every
object key sorted lexicographically at every depth and no whitespace —
i.e. Python's `json.dumps(obj, sort_keys=True, separators=(',', ':'))`.
A field absent from the file is hashed as JSON `null`, so a `kind:
"parameter"` hypothesis hashes the same whether `code_change_summary` is
omitted or written as `null`.

This was pinned on 2026-09-08 after four consecutive runs re-derived the
serialization by hand and got hashes that didn't reproduce on the next
run, forcing the registry to be matched by hypothesis *name* — exactly
the bypass `content_hash` exists to prevent.

## Daily procedure

Let `HOLDOUT_START = now - 14d`, `NOW = now` (UTC, ISO 8601).

### 1. Compute the baseline report

Run the baseline once per evaluation capital (see "Evaluation capital"
above). `/tmp/baseline.json` — the 50,000 JPY run — is the only one the
promotion decision reads; the other two are reported, not compared.

```bash
for JPY in 50000 500000 5000000; do
  shirube backtest-variant --db ./run.db \
    --config experiments/baseline-config.json \
    --from $HOLDOUT_START --to $NOW \
    --resolution-secs 60 --warmup-candles 300 \
    --initial-jpy $JPY \
    > /tmp/baseline-$JPY.json
done
cp /tmp/baseline-50000.json /tmp/baseline.json   # primary: decides promotion
```

Sanity-check the primary run before continuing: if its report is
byte-identical to a previous day's despite a changed config, re-read the
lot-size warning in "Evaluation capital" — at 50,000 JPY an
`allocation_threshold` under ~0.245 cannot move anything.

### 2. Filter out already-tried hypotheses

Read `experiments/tried.json` (treat a missing file as `[]`). For every
`experiments/hypotheses/*.json` file (excluding `README.md`), compute
its `content_hash` the same way and skip it if that hash already appears
in the registry. What remains is today's `CANDIDATES` list.

### 3. Generate new hypotheses when candidates run low

If `CANDIDATES` has fewer than 2 entries, generate up to
`3 - len(CANDIDATES)` new ones (see "Hypothesis generation" below)
before continuing, and add them to `CANDIDATES`. If even after
generation there is nothing to test, skip straight to step 8 with an
empty result set — still write a report noting that.

### 4. Run each hypothesis in its own worktree, in parallel

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
> `constraints`. If the change touches `TradingEngine::compute_btc_target`'s
> body, that is allowed on this dedicated worktree branch (see
> `experiments/hypotheses/README.md`), but say so explicitly in your
> report in step 3 below. Run `cargo test` and fix any failure caused by
> your change before continuing — do not proceed on a red test suite.
> 2. Write the hypothesis's `trading_config` field to a temp JSON file.
> Run `cargo build`, then run the backtest once per evaluation capital —
> `for JPY in 50000 500000 5000000; do shirube backtest-variant --db <path-to-a-copy-or-the-shared-read-only-db> --config <temp-config.json> --from <HOLDOUT_START> --to <NOW> --resolution-secs 60 --warmup-candles 300 --initial-jpy $JPY; done`
> — using the exact same `HOLDOUT_START`/`NOW` as the baseline run. The
> 50,000 JPY run is the one promotion is decided on; the other two are
> reported as diagnostics.
> 3. Report back: the hypothesis name, all three stdout JSON reports
> labelled by capital, whether `cargo test` passed, and (for algorithm
> hypotheses) the worktree path and branch name. If the 50,000 JPY report
> is identical to the baseline's, say so explicitly and check the
> lot-size floor described under "Evaluation capital" before concluding
> the hypothesis had no effect.
> 4. If you hit a missing capability or a defect that blocks or distorts
> this hypothesis — especially anything that makes the backtest numbers
> themselves untrustworthy — you are authorized to fix it (see "Enabling
> changes" in `docs/self-improvement-loop.md`). Keep it on a separate
> branch from the hypothesis diff, add a regression test, and report it
> separately in step 3 so the coordinator can open its own PR. Report a
> suspected defect you chose not to fix as well, rather than silently
> working around it.

Because backtests only read historical tickers (never place real
orders), every worktree agent can safely point at the same
`shirube.db` file read-only, or a copy — either works since
`Simulator` only ever talks to a `MockExchangeClient`.

### 5. Aggregate and decide

For each variant report collected in step 4, compare **the 50,000 JPY
run only** — that is the promotion decision:

```bash
shirube compare-backtest --baseline /tmp/baseline.json --candidate /tmp/variant-<name>-50000.json
```

Collect the `promoted: true` results.

Do **not** run `compare-backtest` on the 500,000 / 5,000,000 JPY reports
to gate anything. Those two go into the report's capital table as
diagnostics only. A promoted variant that looks worse at a larger capital
is still promoted — note the divergence in the report so a human can see
it, and if it is stark it is a good source for tomorrow's hypotheses, but
it never overrides the primary-capital verdict.

### 6. Open PRs for promoted variants, clean up the rest

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
  `compute_btc_target` at all (signature or body) so the reviewer gives
  it the scrutiny an allocation-formula change deserves.

For each **rejected** variant, delete its worktree and branch — nothing
to PR:

```bash
git worktree remove <worktree-path> --force
git branch -D <worktree-branch>
```

**Enabling changes are separate from this.** Any fix or capability
reported under step 4's item 4 gets its own PR against `dev` regardless
of whether the hypothesis that surfaced it was promoted or rejected — a
rejected variant can still have uncovered a real bug, and deleting its
worktree must not discard the fix. Cherry-pick or re-apply the change
onto its own branch off `dev` before removing the worktree. These PRs are
judged on correctness by the reviewer, not on the promotion rule.

One PR per promoted variant (never bundle multiple variants into one
PR), so a bad promotion can be reverted independently.

### 7. Update the tried-hypothesis registry

For every hypothesis run today (whether promoted or rejected), append a
record to `experiments/tried.json` per the schema above. Do this
regardless of outcome — a rejected hypothesis must never be re-tested
just because the registry entry was skipped.

### 8. Write today's report

Write `experiments/reports/<YYYY-MM-DD>.md` (UTC date) with:

```markdown
# Self-improvement report — <date>

## Benchmark
<Lead with this section. If the baseline's `sharpe_minus_static_mix` is
negative, open the whole report with a `> **WARNING**` block per the
warning rule in "Benchmarks" above, before this table.>

| | Return % | Sharpe | Max DD % | Trades |
|---|---|---|---|---|
| Strategy (50,000 JPY, primary) | ... | ... | ... | ... |
| Static mix @ avg exposure <x.xx> | ... | ... | ... | 1 |
| 100% buy-and-hold | ... | ... | ... | 1 |

Excess vs static mix: <excess_return_vs_static_mix_pct> pp return,
<sharpe_minus_static_mix> Sharpe. <One sentence: did the trading earn its
costs this run, and is this the Nth consecutive run in which it did not?>

## Baseline
<the 50,000 JPY BacktestReport JSON in full — this is the primary,
promotion-deciding run — plus 1-2 sentences of context: any notable
indicator warmup issue, data gaps, etc. Also note the observed trades/day
(total_trades / window length in days) against the 4-10 guideline —
recorded for trend visibility only, never as a pass/fail.>

| Capital | Return % | Sharpe | Max DD % | Trades | Volume JPY | eff fee | fee drag % |
|---|---|---|---|---|---|---|---|
| 50,000 (primary) | ... | ... | ... | ... | ... | ... | ... |
| 500,000 | ... | ... | ... | ... | ... | ... | ... |
| 5,000,000 | ... | ... | ... | ... | ... | ... | ... |

(`traded_volume_jpy`, `effective_fee_pct` and `fee_drag_pct` come
straight off `BacktestReport`. `fee_drag_pct` is fees as a percentage of
initial capital, so it is directly comparable with `total_return_pct` —
when it is the larger of the two, trading costs, not signal quality, are
what the run is measuring.)

## Literature reviewed this run
| Paper | Link | Pros | Cons | Outcome |
|---|---|---|---|---|
| <title, authors, venue/year> | <url> | <1 sentence> | <1 sentence> | adopted as `<hypothesis-name>` / not adopted (why) |

("no papers reviewed this run" if Step 1 of "Hypothesis generation" was
skipped, e.g. because step 3 didn't need new hypotheses.)

## Hypotheses tried today
| Hypothesis | Kind | Verdict | Sharpe Δ | Max DD Δ | Trades (cand/base) | Return @500k | Return @5M |
|---|---|---|---|---|---|---|---|
| <name> | parameter/algorithm | promoted/rejected | ... | ... | ... | ... | ... |

(Verdict, Sharpe Δ, Max DD Δ and the trade counts are all from the
50,000 JPY run — the promotion decision. The last two columns are
diagnostics only. Flag in prose any variant whose sign flips between
capitals.)

(one row per hypothesis tried this run; "no hypotheses tried today —
N new ones generated for tomorrow" if step 3's candidate list was empty)

## PRs opened
- <link or branch name> — <one-line summary>

## Enabling changes
- <link or branch name> — <what was missing or broken, and what it
  affected. For a defect in the measurement path, state what past
  reports said and what the corrected numbers are.>

("none this run" if no capability was added and no defect was fixed.
Also list here any suspected defect that was found but deliberately not
fixed, with the reason.)

## New hypotheses generated this run
- **<name>** (<kind>): <rationale, citing the specific observation from
  this or a recent report that motivated it — or, if literature-motivated,
  the paper's title/URL and key claim, plus a one-line summary of the
  Pros/Cons that justified turning it into a hypothesis>

## Running totals
Total tried: <count from experiments/tried.json>. Total promoted: <count>.
```

Commit `experiments/tried.json`, the new report file, and any newly
generated hypothesis files together in one commit directly to `dev`
(this is data/docs, not a source change subject to PR review — unlike
promoted algorithm/parameter hypotheses in step 6, which always go
through a PR). Push it.

### 9. Human review

All PRs land on `dev` per the branch strategy in `CLAUDE.md` — nothing
in this pipeline merges automatically. A human reviews and merges (or
requests changes / closes) each PR normally.

## Hypothesis generation

When step 3 needs new hypotheses, do the following, in order.

### Step 1: literature survey

Before drafting anything, spend this step looking for published research
that bears on what recent reports have struggled with. This is a source
of *ideas*, not of trusted conclusions — a paper's reported Sharpe ratio
was almost always measured on a different market, timeframe, fee/
slippage model, and sample period than shirube's BTC/JPY setup, so it
never substitutes for shirube's own backtest gate (the Sharpe/drawdown/
trade-count promotion rule in the parameters table still decides
everything, unconditionally).

1. Pick 1-3 search queries derived from what the last 5-10 reports under
   `experiments/reports/` actually struggled with (e.g. "bitcoin
   technical indicator ensemble trading strategy", "circuit breaker
   drawdown control crypto trading", "Bollinger Band RSI combined signal
   cryptocurrency backtest"). Use `WebSearch` against arXiv, SSRN, and
   similar sources; prefer papers with an accessible abstract (arXiv
   preprints are usually easiest to pull in full).
2. For each paper worth reading past the abstract, use `WebFetch` to pull
   it and write a short **Pros/Cons** comparison against shirube's
   current logic (composite TA signal in `src/signal/engine.rs`,
   allocation formula in `TradingEngine::compute_btc_target`, risk gates
   in `src/risk/manager.rs`):
   - **Pros**: the specific claim, and why it's plausibly transferable to
     shirube's setup (same asset class, similar timeframe, addresses a
     weakness a recent report actually observed, etc.).
   - **Cons**: reasons to discount the claim here — different instrument/
     market microstructure, no modeled fees/slippage, a pre-2020 sample
     that predates BTC's current liquidity/derivatives regime, small N or
     no out-of-sample test, parameter values tuned on the same data
     they're evaluated on, etc. Every paper gets at least one Con — the
     point of this step is explicitly *not* to accept the paper at face
     value.
   - Decide: does this paper motivate a concrete, testable hypothesis?
     If yes, carry the Pros/Cons and full citation into that hypothesis's
     `paper_reference` field (see `experiments/hypotheses/README.md`'s
     schema) when it's drafted in Step 3 below. If no (Cons dominate, or
     it doesn't map to anything shirube can express as a
     `trading_config`/code change), record it in today's report's
     "Literature reviewed this run" section as *not adopted*, with why.
3. Keep this bounded: review at most 3-5 papers per run, and let it
   consume at most one of the up-to-3 new hypotheses generated per run
   (see the parameters table) — the other slots stay available for ideas
   drawn from internal report/config patterns (Step 2 below), so this
   phase augments rather than replaces the existing generation source.

### Step 2: internal-pattern sources

Used for any generation slots not already spent on a literature-motivated
hypothesis from Step 1. Read (in order of priority):
1. The last 5-10 files under `experiments/reports/` (most recent first)
   for patterns — e.g. a hypothesis that consistently misses the Sharpe
   threshold by a small margin might suggest a nearby parameter value is
   worth trying; a report noting choppy/low-trend periods might suggest
   volatility-sensitive parameters. Also check the recorded trades/day
   against the 4-10 guideline above: a baseline sitting well outside it
   is a hint that a turnover-affecting parameter is worth testing. Treat
   it strictly as a source of candidate ideas — the backtest still
   decides, and a hypothesis is never proposed *because* it would move
   the trade count toward the range.
2. `experiments/tried.json` to see what's already been tried (never
   propose something whose `content_hash` would collide with an existing
   entry).
3. The current indicator/config code (`src/config.rs`, `src/signal/`) to
   know what fields and indicators actually exist.
4. The baseline's **benchmark deficit** — `sharpe_minus_static_mix` and
   `excess_return_vs_static_mix_pct`. A negative value means the strategy
   is currently worth less than a one-trade static allocation, which makes
   it the highest-priority lead available, ahead of any signal idea. After
   three consecutive negative runs at least one new hypothesis must target
   trading cost; see the warning rule under "Benchmarks".
5. The baseline's **cost metrics** — `fee_drag_pct` against
   `total_return_pct`, and `effective_fee_pct` against the break-even
   commission recorded in the most recent capital study. This strategy
   turns over roughly 200x its equity per 30 days, so its P&L is close to
   a linear function of the commission rate: as measured 2026-09-11, a
   0.01 percentage-point change in the effective fee moves the 30-day
   return by about 2 points, and break-even sits near 0.051% at
   500,000 JPY (and ~0.114% at the 50,000 JPY primary, where the lot-size
   floor throttles turnover). When `fee_drag_pct` exceeds
   `total_return_pct`, the run is measuring trading costs more than
   signal quality, and a cost-side hypothesis is usually the higher-value
   thing to test.

   Two caveats keep this honest. First, cutting turnover to save fees
   also pushes the account into a worse tier, so the saving is sublinear
   — at 500,000 JPY, cutting volume 4.4x cut total fees only 2.3x. Score
   such a hypothesis on `total_fees_jpy`, never on trade count. Second,
   `MockExchangeClient` accumulates volume from zero each run and never
   expires it, so the simulated tier is too expensive early in a window
   and too cheap in any run much longer than 30 days; a hypothesis whose
   whole effect is a tier change is standing on that approximation and
   should say so.

### Step 3: draft the hypothesis

For each new hypothesis (whether literature-motivated per Step 1, or
drawn from internal patterns per Step 2), follow
`experiments/hypotheses/README.md`'s schema exactly: pick a unique,
descriptive kebab-case `name`, write a `rationale` that cites the
specific observation motivating it (not a generic guess — for a
literature-motivated hypothesis, cite the paper's key claim here too),
attach `paper_reference` when applicable, and for `kind: "algorithm"`,
include the full `constraints` array from the README verbatim (in
particular the worktree-branch-only and PR-review requirements for any
change touching `compute_btc_target` — see the section above). Write the
new hypothesis as a file in `experiments/hypotheses/<name>.json`. Cap
generation at 3 new hypotheses per run (see the parameters table) to
keep daily runs bounded in cost and review burden.

## Scheduling

Register this procedure as a daily cron routine with the `/schedule`
skill, pointing its prompt at this document (`docs/self-improvement-loop.md`)
so the scheduled agent re-reads the current version each run rather than
having the steps baked into the routine's own prompt text.

## Running as a cloud routine: data bootstrap

A cloud routine gets a fresh git checkout, so nothing on local disk
survives between runs. The DB is instead carried across runs as a
**GitHub Release asset** on the dedicated `backtest-data` tag, restored
and published with the `shirube backtest-data` subcommand — see "Why the
DB is carried over" below for why this matters and why a release asset
specifically.

**The routine calls the `shirube` binary directly and never a shell
script.** This is deliberate. The bootstrap used to go through
`scripts/backtest-data.sh`, which shelled out to the `gh` CLI (for the
release download/upload) and the `sqlite3` CLI (for the recommended
backfill gap and the pre-upload `VACUUM`). Neither is installed in the
cloud routine's container, so `pull` hard-exited on "gh CLI not found on
PATH" before a single backtest could run — the data bootstrap silently
could not run there at all, and the stored DB sat unchanged from
2026-09-09 while the loop kept running against stale history.

The scripts under `scripts/` are local developer conveniences and are not
on this pipeline's path. Do not reintroduce one here, and do not assume
any CLI beyond `git`, `cargo` and the built `shirube` binary exists in the
routine's container. Everything the bootstrap needs is a subcommand:
`shirube db-stats` (row count / time range / recommended backfill days,
via `rusqlite`) and `shirube backtest-data pull|push` (the GitHub release
download/upload, via `reqwest` against the REST API directly).

**Token fallback.** `shirube backtest-data pull` reads `GITHUB_TOKEN`,
falling back to `GH_TOKEN`. If neither is set — or no `backtest-data`
release exists yet — it does not fail: it prints the reason to stderr and
still emits `BACKFILL_DAYS=31` on stdout, so that run falls back to a full
31-day backfill and the loop proceeds rather than dying. `push` has no
such fallback: it fails hard without a token, since silently skipping a
push would lose accumulated history instead of merely deferring a
backfill.

**`push` cannot work from the cloud routine, and no token grant will fix
it.** Since 2026-09-11 every run has failed at
`DELETE /repos/.../releases/assets/<id>` with `403 Forbidden`. The
2026-09-11 report diagnosed this as a credential *scope* limitation and
made "grant the routine's token release-asset write access" its lead #2;
the 2026-09-12 run read the response body and found that diagnosis is
wrong. The token is not under-scoped — `GET /repos/SeitaHigashi/shirube`
returns `"permissions": {"admin": true, "maintain": true, "push": true}`.
The 403 body is:

```json
{"message":"Creating, editing, or deleting releases is not permitted for this session type.",
 "documentation_url":"https://docs.anthropic.com/en/docs/claude-code/github-actions"}
```

That is a **policy of the Claude Code session type**, applied above the
token: releases may not be created, edited, or deleted from this kind of
session at all. It is not repository configuration, so it cannot be
granted by a repo admin, and since it blocks *create* as well as *delete*,
neither uploading under a fresh asset name nor recreating the release is a
way around it.

**Consequence: the release asset cannot accumulate.** `pull` still works
(the download path is an ordinary read), so the stored snapshot is usable,
but it is frozen at whatever a human last uploaded — in practice the
2026-09-09 bootstrap, 33,452 bars. Every routine run therefore re-backfills
the gap from bitFlyer and discards that work when the container is
reclaimed, which caps the lookback at bitFlyer's 31-day retention wall
permanently. The "Why the DB is carried over" section above describes a
mechanism that, as built, works only when a human runs it.

**Two ways out, both needing a human decision:**

1. **A human runs `shirube backtest-data push` periodically** from a normal
   local session, which is not subject to the session-type policy. Cheapest
   fix, no code change; the cost is that accumulation happens only as often
   as someone remembers.
2. **Move the store off releases and onto a git branch** — e.g. a dedicated
   orphan branch holding one gzipped DB blob, force-pushed as a single
   commit each run so history does not accumulate. `git push` is *not*
   blocked for this session type (the routine already pushes `dev` and its
   PR branches every run), and `git` is explicitly on the routine's allowed
   tool list above, so this would let the DB accumulate again without any
   new credential. It needs a `backtest-data` backend change and a human's
   agreement that a ~1.5MB blob force-pushed daily to its own branch is an
   acceptable use of the repository.

Until one of those happens, treat the stored snapshot as read-only and
expect each run's lookback to be bounded by the 31-day wall. Do not record
"grant the token release write access" as a lead again — it has been
tested and it is not the blocker.

Before step 1 of the daily procedure, the routine restores that DB and
tops it up:

```bash
# 1. Checkout dev — the routine's default checkout is the repo's default
#    branch (main), which does NOT have src/backtest/, src/cli.rs, or
#    experiments/. This bit the manual dry-run validation (2026-09-07)
#    when a worktree agent inherited from main; the same applies here.
git fetch origin && git checkout dev && git pull

# 2. Build once so `shirube` subcommands are available.
cargo build --release

# 3. Restore the accumulated DB from the backtest-data release, and ask it
#    how much history is missing. BACKFILL_DAYS is 31 on the very first run
#    (no stored DB yet), 2 on a normal daily run, and larger if the routine
#    has not run for a while — so a skipped day never leaves a hole in the
#    middle of the window.
eval "$(./target/release/shirube backtest-data pull --db ./run.db \
        | grep '^BACKFILL_DAYS=')"

# 4. Top up with fresh bitFlyer 1-minute OHLCV bars from the public
#    execution tape. Bars are written INSERT OR IGNORE on
#    UNIQUE(product_code, timestamp), so this extends the DB forward and
#    never rewrites stored history. A daily --days 2 run takes ~40s; the
#    first-run --days 31 takes 10-12 minutes (~900 requests at ~80 req/min).
./target/release/shirube backfill-executions \
  --db ./run.db --product BTC_JPY --days "$BACKFILL_DAYS" --resolution-secs 60

# 5. Publish the topped-up DB back so tomorrow's run starts from it. Do this
#    BEFORE the backtests, not after: the data is worth keeping even if the
#    rest of the cycle fails.
./target/release/shirube backtest-data push --db ./run.db
```

`backfill-executions` prints a `BackfillStats` JSON, and `pull` reports
the restored row count and range. Sanity-check both before running any
backtest this cycle:

- The DB's total row count should be **at least** what the previous run
  stored, and should grow by roughly 1,000-1,100 bars per elapsed day. A
  count that went *down* means the wrong DB was restored — stop and
  investigate rather than pushing over the stored copy.
- `oldest_bar` in `BackfillStats` reflects only what this run fetched, not
  the DB's full range; on a daily `--days 2` run it will be two days ago,
  which is correct. Check the DB itself for total coverage.
- `hit_history_limit: true` is expected on a first-run `--days 31` (the
  walk reached bitFlyer's retention wall) and unexpected on `--days 2`.
- On the first run, `bars_written` should be on the order of 30k-35k. The
  2026-09-09 bootstrap wrote 33,184 bars from 394,535 executions in 790
  requests — 74% of the 44,640 minutes in 31 days, because a 1-minute bar
  exists only for minutes that actually traded.
- If `executions_fetched` is small relative to the days requested,
  something throttled the run; re-run rather than backtesting on a
  truncated window.

Use `./run.db` as the `--db` for every `backtest-variant` call this
cycle (baseline and every variant), and pass `--resolution-secs 60`
and `--warmup-candles 300` throughout — see "Resolution must match live"
above. The warmup covers the binding indicator period in
`experiments/baseline-config.json` (`sma_period: 200` 1-minute bars) with
margin; those candles are fetched before `--from` and excluded from the
report, which is what the extra backfilled day is for.

These are bitFlyer's own trade prints for BTC_JPY, so the bars are the
real market the bot trades rather than an approximation, and the 31-day
retention wall is worked around by accumulation rather than by finding a
different source (see "Why the DB is carried over" above).

A `shirube.db` exported from the running instance would also have history
past the wall, but do **not** route it through the `backtest-data` release:
that asset is public, and a live DB carries order and balance history. If
that export is ever wanted, take only its `tickers` rows and merge them
into the accumulated DB locally.
