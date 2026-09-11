# Capital-size and fee-tier study — 2026-09-11

Ad-hoc study requested by the repo owner, not a scheduled loop run. The
questions: evaluate the strategy at 50,000 / 500,000 / 5,000,000 JPY of
initial capital, quantify how bitFlyer's volume-tiered commission falls
as trade volume rises, and assess whether the trading logic should adapt
to the fee it is actually being charged.

## Method

| | |
|---|---|
| Data | `run.db`, 33,452 real bitFlyer BTC_JPY 1-minute OHLCV bars, 2026-08-09 .. 2026-09-09 |
| Window | 2026-08-10T00:00Z .. 2026-09-09T09:00Z (30.4 days) |
| Config | `experiments/baseline-config.json` at `bf48570` (zone 0.1/0.9) unless stated |
| Resolution | 60s, `--warmup-candles 300`, `--slippage-pct 0.001` |
| Fees | `MockExchangeClient`'s tiered schedule (default) unless `--fee-pct` is given |

Market context: BTC/JPY ran 10,243,186 → 12,253,997 over the window,
**+19.63%**. This is a bull window; read every figure against that, not
against zero.

**Baseline caveat.** The first pass of this study ran at `e45581f` and
produced materially worse numbers (50,000 JPY: -3.34%; 500,000 JPY:
-2.20%). That was not an error in either run: `dev` had meanwhile merged
PR #11 (`wide-zone-band`, zone 0.2/0.8 → 0.1/0.9), which cuts turnover
from ~2727 to ~1635 trades. Re-running the old config on the new binary
reproduced the old figures to the last digit
(-3.33605259720223 / -2.1955476379985224), confirming that the fee-metrics
commit is behavior-neutral and the shift is entirely the promoted zone
change. All numbers below are at `bf48570`.

## Result 1 — capital changes nothing except the fee tier

Default (tiered) fees. `eff fee` and `drag` are now measured directly
rather than inferred — they are new `BacktestReport` fields added for
this study.

| Initial JPY | Return % | Sharpe | Max DD % | Trades | Volume (JPY) | eff fee | final tier | drag % |
|---|---|---|---|---|---|---|---|---|
| 50,000 | +2.35 | 1.94 | 5.74 | 286 | 3.61M | **0.1106%** | 0.10% | 8.00 |
| 500,000 | +0.92 | 0.86 | 5.69 | 1635 | 101.9M | **0.0464%** | 0.02% | 9.47 |
| 1,000,000 | +3.64 | 2.72 | 4.73 | 1635 | 207.0M | **0.0331%** | 0.02% | 6.84 |
| 5,000,000 | +7.00 | 4.94 | 4.01 | 1635 | 1,056.4M | **0.0174%** | 0.01% | 3.68 |

Controls that isolate the mechanism:

| Initial JPY | zero fee | flat 0.15% |
|---|---|---|
| 50,000 | +10.3747% (352 trades) | -2.6675% (340) |
| 500,000 | +10.8732152% (1635) | -17.8177472% (1630) |
| 5,000,000 | +10.8732179% (1635) | -17.8177784% (1630) |

At and above 500,000 JPY the results agree to six decimal places once the
fee rate is held fixed. The allocation logic is percentage-based, so
**capital is not a strategy variable above the lot-size floor — its only
effect is which commission tier the account's turnover reaches.** The
50,000 JPY row is the exception; Result 3 explains why.

**Direct answer to "does the fee fall with trade volume":** yes, and the
spread is wide. Running the identical strategy, a 5,000,000 JPY account
pays an effective **0.0174%** while a 50,000 JPY account pays **0.1106%**
— a factor of **6.4**. The tier reached is a direct function of capital,
because turnover is proportional to it.

## Result 2 — the fee, not the signal, is the binding constraint

Capital fixed at 500,000 JPY, sweeping `--fee-pct`:

| fee | return % | Sharpe | trades |
|---|---|---|---|
| 0.15% | -17.82 | -13.45 | 1630 |
| 0.12% | -12.75 | -9.31 | 1631 |
| 0.10% | -9.18 | -6.53 | 1631 |
| 0.09% | -7.35 | -5.14 | 1631 |
| 0.07% | -3.68 | -2.44 | 1635 |
| 0.06% | -1.72 | -1.03 | 1635 |
| 0.05% | **+0.27** | 0.37 | 1635 |
| 0.04% | +2.31 | 1.77 | 1635 |
| 0.03% | +4.39 | 3.17 | 1635 |
| 0.02% | +6.51 | 4.57 | 1635 |
| 0.01% | +8.67 | 5.97 | 1635 |
| 0% | +10.87 | 7.36 | 1635 |

Linear, which pins the numbers exactly:

- **Turnover is ~216x equity per 30 days** (108.1M JPY of volume on
  500,000 JPY of capital, ~7x/day across 1635 trades).
- **Break-even effective commission is ~0.051%** (interpolating between
  the 0.06% and 0.05% rows).

Comparing break-even against what each account actually pays:

| Initial JPY | turnover (x equity) | break-even fee | fee actually paid | margin |
|---|---|---|---|---|
| 50,000 | 90.9x | ~0.114% | 0.1106% | razor-thin |
| 500,000 | 203.9x | ~0.051% | 0.0464% | thin |
| 1,000,000 | 207.0x | ~0.051% | 0.0331% | comfortable |
| 5,000,000 | 211.3x | ~0.051% | 0.0174% | wide |

Every capital level currently sits on the profitable side of its own
break-even, but 50,000 and 500,000 JPY do so by under 0.005 percentage
points of commission. That is not a margin to rely on: it is smaller than
the difference between two adjacent bitFlyer tiers.

For reference, the same strategy under the pre-PR-#11 zone band had a
break-even of ~0.030% against ~305x turnover. Widening the band roughly
doubled the fee the strategy can survive — the promoted variant was, in
hindsight, a fee-cost fix.

### Caveat: the simulated tier is wrong in both directions

`MockExchangeClient` accumulates volume from zero at the start of each
run and never expires it, while bitFlyer's tier is keyed on a *trailing
30-day* window. So:

- **Early in a run the tier is too expensive.** A live account that has
  been trading for a month already sits in its steady-state tier from the
  first candle; the backtest makes it earn its way down from 0.15%. At
  500,000 JPY the measured effective 0.0464% versus a steady-state 0.02%
  is worth roughly 5 percentage points of return over this window — the
  difference between a rejected and a promoted verdict.
- **Late in a long run the tier is too cheap.** Nothing ever expires, so
  a backtest materially longer than 30 days ratchets into tiers a real
  account could not hold.

Both are the same missing piece: there is no trailing-window model. At the
current 30-day lookback only the first error is active, and it
systematically penalizes small accounts hardest.

## Result 3 — at 50,000 JPY the exchange lot size, not the config, is the gate

Sweeping `allocation_threshold` (tiered fees):

| threshold | 50,000 JPY | 500,000 JPY |
|---|---|---|
| 0.05 | +2.34688% / 286 trades / eff 0.1106% | -2.55% / 4948 / eff 0.0352% |
| 0.10 | +2.34688% / 286 trades / eff 0.1106% | +0.92% / 1635 / eff 0.0464% |
| 0.15 | +2.34688% / 286 trades / eff 0.1106% | +2.60% / 669 / eff 0.0586% |
| 0.25 | +2.19% / 210 / eff 0.1135% | +4.05% / 286 / eff 0.0676% |
| 0.30 | +12.09% / 14 | +12.22% / 14 |

Three thresholds producing byte-identical reports at 50,000 JPY is the
tell. `TradingEngine::allocation_delta_to_order` computes
`size = delta * total_value / price` and returns `None` when
`size < min_order_size` (0.001 BTC). At ~12.2M JPY/BTC that minimum is
~12,224 JPY of notional — **24.4% of a 50,000 JPY portfolio**. Every
configured threshold below ~0.245 is inert on that account: the real gate
is bitFlyer's lot size. The floor is 2.4% at 500,000 JPY and 0.24% at
5,000,000 JPY, which is why the larger accounts are scale-invariant.

Two consequences, neither currently visible in a report:

- The `None` return happens *before* `RiskManager::evaluate`, so dropped
  rebalances are counted in neither `total_trades` nor `orders_rejected`.
- With promotion now judged at 50,000 JPY, **any parameter hypothesis
  tuning `allocation_threshold` below ~0.245 will score identically to
  the baseline** — not because the idea is wrong, but because the lot
  size erases it. This is the same failure mode as the CoinGecko
  constant-volume VWMA recorded in `tried.json`, where a 0.0 delta was
  arithmetic rather than evidence.

The `0.30` row is not a discovery: 14 trades over 30 days is a hold. The
strategy's zero-fee ceiling of +10.87% is about half the market move,
which is what a partial-allocation model should produce; fees then take
34-90% of that half depending on capital. Result 4 puts that against the
proper benchmarks.

### The turnover/tier trade-off is sublinear

Cutting turnover to save fees also pushes the account into a worse tier,
so the saving is less than proportional. At 500,000 JPY, raising the
threshold from 0.05 to 0.25 cuts volume by **4.4x** (176.9M → 39.8M) but
total fees by only **2.3x** (62,277 → 26,885 JPY), because the effective
rate nearly doubles (0.0352% → 0.0676%). Any "trade less to save fees"
hypothesis must be scored on fees paid, not on trade count.

## Result 4 — the edge is real, and the fees eat all of it

Benchmarks computed over the identical window, same slippage (0.1%) and a
single entry commission at the 0.15% entry tier. The static mix is sized
at the strategy's own average BTC exposure (~55%), which is the fair
comparison: measuring a partial-exposure allocation model against 100%
buy-and-hold mostly measures exposure rather than skill.

| | Return | Sharpe | Max DD | Trades |
|---|---|---|---|---|
| 100% buy-and-hold | +19.63% | 6.73 | 7.67% | 1 |
| Static 30% BTC | +5.88% | 6.29 | 2.69% | 1 |
| Static 50% BTC | +9.80% | 6.43 | 4.28% | 1 |
| **Static 55% BTC** | **+10.78%** | **6.46** | **4.65%** | **1** |
| Static 70% BTC | +13.73% | 6.55 | 5.73% | 1 |
| Strategy, zero fee | +10.87% | **7.36** | **3.28%** | 1635 |
| Strategy @5,000,000 | +7.00% | 4.94 | 4.01% | 1635 |
| Strategy @1,000,000 | +3.64% | 2.72 | 4.73% | 1635 |
| Strategy @500,000 | +0.92% | 0.86 | 5.69% | 1635 |
| Strategy @50,000 | +2.35% | 1.94 | 5.74% | 286 |

Two findings, and they point in opposite directions:

- **The timing logic has a genuine edge.** At zero fees the strategy's
  Sharpe of 7.36 beats 100% buy-and-hold (6.73) and every static mix, and
  its 3.28% max drawdown is the lowest figure in the table. This is not a
  strategy that merely tracks its exposure.
- **The fees consume the entire edge.** Once real commissions apply, every
  capital level loses to the static 55% mix on **both** return and Sharpe
  — to a benchmark that requires exactly one trade. The best case,
  5,000,000 JPY, returns +7.00% at Sharpe 4.94 against the benchmark's
  +10.78% at 6.46, while carrying the execution risk of 1635 orders.

So the earlier framing in Result 3 ("no configuration beats holding") was
right but incomplete: the shortfall is not a risk-taking story that Sharpe
would forgive, and it is not a signal-quality problem either. It is
entirely a cost problem. **Cost reduction is worth more than signal work
until this gap closes.**

Caveat: one 30-day bull window. Static long exposure is structurally
strong in a rising market, which is why the loop reports this comparison
and warns on it but does not gate promotion on it — a benchmark gate would
measure the regime rather than the strategy.

## Implications for the loop

1. **Promotion is decided at 50,000 JPY** (the owner's current account
   size), with 500,000 and 5,000,000 JPY reported alongside as robustness
   diagnostics carrying no veto. 500,000 JPY is the stated near-term
   target.
2. The sweep is cheap — three runs of a few seconds each against one DB.
3. Fee-adaptive logic is now a measured direction, not speculation:
   Result 2 shows the commission rate, not signal quality, decides
   whether this strategy makes money.
4. **Every run now reports the buy-and-hold and matched-exposure static
   benchmarks**, and warns when the baseline loses to them (owner's
   decision, 2026-09-11). They are reported and warned on, never a
   promotion gate — see "Benchmarks" in `docs/self-improvement-loop.md`
   for why a benchmark gate would measure the regime rather than the
   strategy. After three consecutive losing runs, at least one generated
   hypothesis must target trading cost.

## Hypotheses generated

- **`fee-tier-aware-allocation-threshold`** (algorithm): make the
  rebalance gate a function of the live commission rate
  (`max(allocation_threshold, fee_threshold_multiplier * commission_rate)`),
  so a small or freshly-started account trades only on large allocation
  moves and tightens automatically as its volume reaches cheaper tiers.
- **`min-notional-aware-rebalance`** (algorithm): count the
  silently-dropped sub-lot rebalances, and round a warranted-but-too-small
  order up to exactly `min_order_size` when the balance affords it.

## Enabling changes from this study

- **Fee metrics on `BacktestReport`** (`bf48570`, merged) —
  `total_fees_jpy`, `traded_volume_jpy`, `effective_fee_pct`,
  `final_fee_tier_pct`, `fee_drag_pct`. The first pass of this study had
  to back-solve every cost figure from paired runs because the report
  carried no cost information at all.
- **Loop data bootstrap** — the cloud routine's container has neither
  `gh` nor `sqlite3`, so `scripts/backtest-data.sh` could not restore or
  publish the accumulated DB there.

## Open follow-ups (not yet implemented)

- **Fee-tier warm start** — an `--initial-volume-jpy` flag seeding the
  simulated trailing-30-day volume, plus expiry of volume older than 30
  days, so a backtest is neither charged the entry tier for turnover a
  live account already has nor credited with volume that would have
  aged out.
- **Live commission rate** — `BitFlyerRestClient::fee_pct()` returns a
  hardcoded 0.0015 with a comment claiming the rate cannot be fetched
  over REST. That comment is stale: `get_trading_commission()`
  (`/v1/me/gettradingcommission`) is already implemented and unit-tested
  in the same file, and nothing calls it. The live bot therefore believes
  it pays 0.15% regardless of what it actually pays, and every
  fee-adaptive hypothesis above is inert in production until this is
  wired up.
