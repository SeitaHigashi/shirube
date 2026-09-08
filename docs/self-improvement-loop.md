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
| Holdout window | last 14 days |
| Total lookback | 30 days (16 days train + 14 days holdout) |
| Promotion rule | Sharpe ratio improves >= 10% relative (or >= 0.1 absolute if baseline Sharpe <= 0) **AND** max drawdown does not worsen **AND** candidate trade count >= 50% of baseline's |
| PR granularity | one PR per promoted variant |
| Backtest resolution | 3600s (1h) candles, adjust via `--resolution-secs` if a finer/coarser view is needed |
| New hypotheses generated per run | at most 3 (see "Hypothesis generation" below) |

The promotion rule is implemented in `backtest::report::compare` (see
`src/backtest/report.rs`) — it is not re-derived by the agent, only
invoked via `shirube compare-backtest`.

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

## Daily procedure

Let `HOLDOUT_START = now - 14d`, `NOW = now` (UTC, ISO 8601).

### 1. Compute the baseline report

```bash
shirube backtest-variant --db shirube.db \
  --config experiments/baseline-config.json \
  --from $HOLDOUT_START --to $NOW \
  > /tmp/baseline.json
```

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

### 5. Aggregate and decide

For each variant report collected in step 4:

```bash
shirube compare-backtest --baseline /tmp/baseline.json --candidate /tmp/variant-<name>.json
```

Collect the `promoted: true` results.

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

## Baseline
<the baseline BacktestReport JSON, plus 1-2 sentences of context: any
notable indicator warmup issue, data gaps, etc.>

## Literature reviewed this run
| Paper | Link | Pros | Cons | Outcome |
|---|---|---|---|---|
| <title, authors, venue/year> | <url> | <1 sentence> | <1 sentence> | adopted as `<hypothesis-name>` / not adopted (why) |

("no papers reviewed this run" if Step 1 of "Hypothesis generation" was
skipped, e.g. because step 3 didn't need new hypotheses.)

## Hypotheses tried today
| Hypothesis | Kind | Verdict | Sharpe Δ | Max DD Δ | Trades (cand/base) |
|---|---|---|---|---|---|
| <name> | parameter/algorithm | promoted/rejected | ... | ... | ... |

(one row per hypothesis tried this run; "no hypotheses tried today —
N new ones generated for tomorrow" if step 3's candidate list was empty)

## PRs opened
- <link or branch name> — <one-line summary>

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
   volatility-sensitive parameters.
2. `experiments/tried.json` to see what's already been tried (never
   propose something whose `content_hash` would collide with an existing
   entry).
3. The current indicator/config code (`src/config.rs`, `src/signal/`) to
   know what fields and indicators actually exist.

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

A cloud routine gets a fresh git checkout with no accumulated
`shirube.db` — there is no running instance's ticker history to read.
Before step 1 of the daily procedure, the routine must build its own
`shirube.db` for this run:

```bash
# 1. Checkout dev — the routine's default checkout is the repo's default
#    branch (main), which does NOT have src/backtest/, src/cli.rs, or
#    experiments/. This bit the manual dry-run validation (2026-09-07)
#    when a worktree agent inherited from main; the same applies here.
git fetch origin && git checkout dev && git pull

# 2. Build once so `shirube` subcommands are available.
cargo build --release

# 3. Initialize a fresh DB's schema (any subcommand that opens the DB
#    works; this one is a fast no-op against an empty range).
rm -f ./run.db
./target/release/shirube backtest-variant --db ./run.db \
  --config experiments/baseline-config.json \
  --from $(date -u -d '1 hour ago' +%Y-%m-%dT%H:%M:%SZ) --to $(date -u +%Y-%m-%dT%H:%M:%SZ) \
  || true   # expected to fail with "no candles found" — that's fine, schema is now created

# 4. Fetch 30 days of real hourly BTC/JPY prices from CoinGecko's public,
#    keyless API and seed them into ./run.db's tickers table.
curl -s "https://api.coingecko.com/api/v3/coins/bitcoin/market_chart?vs_currency=jpy&days=30&interval=hourly" \
  -o /tmp/btc_jpy_30d.json

python3 - <<'PYEOF'
import sqlite3, json, datetime
d = json.load(open('/tmp/btc_jpy_30d.json'))
conn = sqlite3.connect('./run.db')
rows = []
for ts_ms, price in d['prices']:
    ts = datetime.datetime.fromtimestamp(ts_ms/1000, datetime.timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')
    p = str(price)
    rows.append(('BTC_JPY', ts, p, p, '0.1', '0.1', p, p, p, p, '1', '1'))
conn.executemany('''INSERT OR IGNORE INTO tickers
    (product_code, timestamp, best_bid, best_ask, best_bid_size, best_ask_size,
     ltp_open, ltp, ltp_high, ltp_low, volume, volume_by_product)
    VALUES (?,?,?,?,?,?,?,?,?,?,?,?)''', rows)
conn.commit()
print(f"seeded {len(rows)} ticker rows")
PYEOF
```

Use `./run.db` as the `--db` for every `backtest-variant` call this
cycle (baseline and every variant). This is CoinGecko's aggregate market
price, not bitFlyer's own order book — an approximation accepted for
this pipeline's relative (variant-vs-baseline) comparisons, not meant to
match bitFlyer's exact historical prices. If a real `shirube.db` export
from the running instance becomes available later, prefer that instead
(see the note in "CLI building blocks" above about keeping
`experiments/baseline-config.json` in sync with the live DB-persisted
config).
