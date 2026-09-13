pub mod engine;
pub mod indicators;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::config::TradingConfig;
use crate::types::market::Candle;

// ---- Signal — TradingEngine が取引所へ送る発注指示専用 ----

/// TradingEngine が取引所に送付する発注指示。
/// インジケータの売買判断には使用しない。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Signal {
    Buy,
    Sell,
    Hold,
}

// ---- IndicatorSignal — 各インジケータの個別シグナル ----

/// 各インジケータの名前と計算値のペア。
/// 売買方向の判断は含まない（TradingEngine の責務）。
#[derive(Debug, Clone, Serialize)]
pub struct IndicatorSignal {
    pub name: String,
    /// 現在の計算値（SMA値、RSI値、MACDヒストグラム等）。ウォームアップ中は None。
    pub value: Option<f64>,
}

// ---- AllocationSignal — 連続配分シグナル ----

/// compute_btc_target の計算結果を表す型。
/// normalized はゾーン変換前の raw_signal 値 [0.0, 1.0]。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AllocationSignal {
    /// 正規化済みシグナル値 [0.0, 1.0]。
    /// 強気 → 1.0、弱気 → 0.0、中立 → 0.5。
    pub normalized: f64,
}

impl AllocationSignal {
    /// 中立シグナル: normalized = 0.5
    pub fn neutral() -> Self {
        Self { normalized: 0.5 }
    }
}

/// raw_signal を ZoneConfig に従って実効BTC配分率 [0.0, 1.0] に変換する。
///
/// Zone A: raw < hold_jpy_below  → 0.0（全JPY）
/// Zone B: hold_jpy_below ≤ raw ≤ hold_btc_above → 線形補間
/// Zone C: raw > hold_btc_above  → 1.0（全BTC）
pub fn apply_zone(raw: f64, zone: &crate::config::ZoneConfig) -> f64 {
    if raw < zone.hold_jpy_below {
        return 0.0;
    }
    if raw > zone.hold_btc_above {
        return 1.0;
    }
    let span = zone.hold_btc_above - zone.hold_jpy_below;
    if span <= 0.0 {
        return if raw >= zone.hold_btc_above { 1.0 } else { 0.0 };
    }
    ((raw - zone.hold_jpy_below) / span).clamp(0.0, 1.0)
}

// ---- SmoothedSignal — composite シグナルの EWMA 平滑化 ----

/// Half-life of the composite-signal EWMA, expressed in **bars**.
///
/// 30 bars of 60s candles = 30 minutes of market time. This is an a priori
/// round figure: it was chosen because it is the obvious "half an hour" value
/// for a 1-minute bar series, and it was **NOT fitted on the holdout window,
/// the training window, or any other window**. If it turns out to be the wrong
/// scale that is a result to report, not a knob to retune mid-run.
pub(crate) const EWMA_HALF_LIFE_BARS: f64 = 30.0;

/// Smoothing factor derived from [`EWMA_HALF_LIFE_BARS`].
///
/// Formula (standard half-life → alpha conversion):
///
/// ```text
///   alpha = 1 - 0.5^(1 / half_life)
/// ```
///
/// With `half_life = 30` this is `1 - 0.5^(1/30) ≈ 0.022840`. After exactly
/// `half_life` updates the filter has closed half the distance to a step
/// input, because `(1 - alpha)^half_life = 0.5` by construction.
///
/// NOTE: not a `const fn` candidate — `f64::powf` is not const — so this is a
/// `LazyLock`-free plain function evaluated at each construction instead.
#[inline]
pub(crate) fn ewma_alpha() -> f64 {
    1.0 - 0.5_f64.powf(1.0 / EWMA_HALF_LIFE_BARS)
}

/// Exponentially weighted moving average over the composite normalized signal.
///
/// This is applied to `TradingEngine::compute_btc_target`'s **return value**,
/// before it is scaled by `zone.range_max` and passed to [`apply_zone`].
/// `compute_btc_target` itself stays a pure function of a single
/// `IndicatorPoint`; all temporal memory lives here, in the caller.
///
/// Update rule, for each candle where `compute_btc_target` returns `Some`:
///
/// ```text
///   s_0 = x_0                              (seed with the first value seen)
///   s_t = alpha * x_t + (1 - alpha) * s_{t-1}
/// ```
///
/// Seeding with the first observed value (rather than 0.5, or 0.0) means the
/// very first evaluated candle is passed through unsmoothed, so the filter
/// introduces no startup bias toward the neutral allocation.
///
/// On a warmup candle — where `compute_btc_target` returns `None` — the state
/// is left **untouched**: it is neither updated nor decayed toward anything,
/// matching the existing sticky-target semantics where a `None` preserves the
/// last directional decision rather than erasing it.
///
/// # Cadence caveat
///
/// NOTE: the two call sites see different cadences. The backtest
/// (`backtest::simulator`) performs exactly one update per 60s bar, while the
/// live `SignalEngine` re-evaluates roughly every 250ms (~4 Hz), so live can
/// feed this filter many updates within a single bar. The half-life is
/// therefore expressed in **bars** and is only equivalent live if the live
/// evaluation cadence is one update per bar. That ~4 Hz live/backtest
/// divergence is a pre-existing, separately tracked open question (flagged in
/// the 2026-09-12 report's "Suspected defects"); it is deliberately NOT
/// addressed here.
#[derive(Debug, Clone, Default)]
pub(crate) struct SmoothedSignal {
    /// Current filter state. `None` until the first value is seen.
    value: Option<f64>,
}

impl SmoothedSignal {
    /// A fresh filter with no state — the next value it sees seeds it.
    pub(crate) fn new() -> Self {
        Self { value: None }
    }

    /// Feed one composite normalized value and return the smoothed value to act on.
    ///
    /// Seeds on the first call (returns `normalized` unchanged); afterwards
    /// applies `s_t = alpha * x_t + (1 - alpha) * s_{t-1}`. Because the input
    /// is a normalized allocation signal in `[0.0, 1.0]` and the update is a
    /// convex combination, the output stays inside that same range.
    pub(crate) fn update(&mut self, normalized: f64) -> f64 {
        let next = match self.value {
            None => normalized,
            Some(prev) => {
                let alpha = ewma_alpha();
                alpha * normalized + (1.0 - alpha) * prev
            }
        };
        self.value = Some(next);
        next
    }

    /// Current filter state without updating it. `None` before the first update.
    #[cfg(test)]
    pub(crate) fn value(&self) -> Option<f64> {
        self.value
    }
}

// ---- IndicatorOutput — SignalEngine が出力する純粋な計算結果 ----

/// SignalEngine がブロードキャストする計算結果。
/// インジケータの計算値のみを含み、「何%BTCを持つか」という判断は含まない。
/// TradingEngine はこれを受け取り compute_btc_target() で配分目標を計算する。
#[derive(Debug, Clone)]
pub struct IndicatorOutput {
    pub indicators: Vec<IndicatorSignal>,
    pub raw: IndicatorPoint,
    pub calculated_at: DateTime<Utc>,
}

// ---- SignalDetail — API レスポンス用（集計 + 個別） ----

#[derive(Debug, Clone, Serialize)]
pub struct SignalDetail {
    pub aggregate: AllocationSignal,
    /// TradingEngine が compute_btc_target で算出した目標 BTC 配分率 [0.0, 1.0]。
    /// sticky_target を反映するため、中立シグナル時も最後の有効値を保持する。
    pub target_pct: f64,
    pub indicators: Vec<IndicatorSignal>,
    /// 生インジケータ値（ウォームアップ中は None）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_indicators: Option<IndicatorPoint>,
    /// シグナルが計算された日時（ミリ秒精度）
    pub calculated_at: DateTime<Utc>,
    /// 計算エンジンの状態: "active" | "waiting_for_data"
    pub calculation_state: String,
}

// ---- IndicatorPoint — 1本のキャンドルに対する全インジケーター値 ----

/// 1本のキャンドル時点における全インジケーターの計算値。
/// ウォームアップ中のフィールドは None。
/// close は TradingEngine が MA系指標との乖離を計算するために含む。
#[derive(Debug, Clone, Serialize)]
pub struct IndicatorPoint {
    pub time: DateTime<Utc>,
    /// キャンドルの終値。TradingEngine が SMA/EMA との乖離計算に使用する。
    pub close: Option<f64>,
    pub sma: Option<f64>,
    pub ema: Option<f64>,
    pub rsi: Option<f64>,
    pub macd_line: Option<f64>,
    pub signal_line: Option<f64>,
    pub histogram: Option<f64>,
    pub bb_upper: Option<f64>,
    pub bb_middle: Option<f64>,
    pub bb_lower: Option<f64>,
}

/// キャンドル列にインジケーターを順次適用し、各時点の値を返す。
/// シグナルエンジンと同一ロジックを使用する純粋関数。
pub fn compute_indicators(candles: &[Candle], cfg: &TradingConfig) -> Vec<IndicatorPoint> {
    use indicators::{bollinger::Bollinger, ema::Ema, macd::Macd, rsi::Rsi, sma::Sma};
    use rust_decimal::prelude::ToPrimitive;

    let mut sma = Sma::new(cfg.sma_period);
    let mut ema = Ema::new(cfg.ema_period);
    let mut rsi = Rsi::new(cfg.rsi_period);
    let mut macd = Macd::new(cfg.macd_fast, cfg.macd_slow, cfg.macd_signal);
    let mut bb = Bollinger::new(cfg.bollinger_period, cfg.bollinger_std);

    candles.iter().map(|c| {
        sma.update(c);
        ema.update(c);
        rsi.update(c);
        macd.update(c);
        bb.update(c);

        let (macd_line, signal_line, histogram) = macd.macd_components()
            .map(|(ml, sl, h)| (Some(ml), Some(sl), Some(h)))
            .unwrap_or((None, None, None));
        let (bb_upper, bb_middle, bb_lower) = bb.band_values()
            .map(|(u, m, l)| (Some(u), Some(m), Some(l)))
            .unwrap_or((None, None, None));

        IndicatorPoint {
            time: c.open_time,
            close: c.close.to_f64(),
            sma: sma.value(),
            ema: ema.value(),
            rsi: rsi.value(),
            macd_line,
            signal_line,
            histogram,
            bb_upper,
            bb_middle,
            bb_lower,
        }
    }).collect()
}

// ---- Indicator trait ----

/// インジケータが返す生値のバリアント。
/// スカラー（SMA/EMA/RSI）と多値（MACD/Bollinger）を型安全に区別する。
#[derive(Debug, Clone)]
pub enum IndicatorRawValues {
    /// スカラー値（SMA, EMA, RSI 等）。ウォームアップ中は None。
    Scalar(Option<f64>),
    /// MACD の 3 値セット。ウォームアップ中は全て None。
    Macd {
        macd_line: Option<f64>,
        signal_line: Option<f64>,
        histogram: Option<f64>,
    },
    /// ボリンジャーバンドの 3 値セット。ウォームアップ中は全て None。
    BollingerBands {
        upper: Option<f64>,
        middle: Option<f64>,
        lower: Option<f64>,
    },
}

pub trait Indicator: Send + Sync {
    fn name(&self) -> &str;
    /// Candle を受け取り内部状態を更新する。売買判断は行わない。
    /// 計算値は value() / snapshot() で取得する。
    fn update(&mut self, candle: &Candle);
    /// 現在の計算値を返す（SMA値、RSI値等）。ウォームアップ中は None。
    fn value(&self) -> Option<f64>;
    /// バックテスト再利用のためのリセット
    fn reset(&mut self);
    /// シグナルを出すのに必要な最低 Candle 数
    fn min_periods(&self) -> usize;
    /// 現在の生計算値を返す。多値インジケータ（MACD/Bollinger）はそれぞれのバリアントを返す。
    fn snapshot(&self) -> IndicatorRawValues;
}

// ---- MockIndicator (テスト用) ----

#[cfg(any(test, feature = "mock"))]
pub mod mock {
    use super::*;

    /// テスト用インジケータ。コンストラクト時に渡した value 列を順番に返す。
    pub struct MockIndicator {
        name: String,
        values: std::collections::VecDeque<Option<f64>>,
        current: Option<f64>,
    }

    impl MockIndicator {
        pub fn new(name: impl Into<String>, values: Vec<Option<f64>>) -> Self {
            Self {
                name: name.into(),
                values: values.into(),
                current: None,
            }
        }
    }

    impl Indicator for MockIndicator {
        fn name(&self) -> &str {
            &self.name
        }

        fn update(&mut self, _candle: &Candle) {
            self.current = self.values.pop_front().flatten();
        }

        fn value(&self) -> Option<f64> {
            self.current
        }

        fn reset(&mut self) {
            self.values.clear();
            self.current = None;
        }

        fn min_periods(&self) -> usize {
            0
        }

        fn snapshot(&self) -> IndicatorRawValues {
            IndicatorRawValues::Scalar(self.value())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_candle() -> Candle {
        use chrono::Utc;
        use rust_decimal_macros::dec;
        Candle {
            product_code: "BTC_JPY".into(),
            open_time: Utc::now(),
            resolution_secs: 60,
            open: dec!(9000000),
            high: dec!(9001000),
            low: dec!(8999000),
            close: dec!(9000500),
            volume: dec!(1),
        }
    }

    #[test]
    fn mock_indicator_returns_values_in_order() {
        use mock::MockIndicator;
        let candle = dummy_candle();
        let mut ind = MockIndicator::new("test", vec![Some(0.5), None, Some(-0.3)]);

        ind.update(&candle);
        assert_eq!(ind.value(), Some(0.5));
        ind.update(&candle);
        assert_eq!(ind.value(), None);
        ind.update(&candle);
        assert_eq!(ind.value(), Some(-0.3));
    }

    #[test]
    fn mock_indicator_returns_none_when_exhausted() {
        use mock::MockIndicator;
        let candle = dummy_candle();
        let mut ind = MockIndicator::new("empty", vec![]);
        ind.update(&candle);
        assert!(ind.value().is_none());
    }

    mod smoothed_signal_tests {
        use super::*;

        #[test]
        fn ewma_alpha_matches_half_life_formula() {
            // alpha = 1 - 0.5^(1/30) = 1 - exp(-ln2/30) ≈ 0.0228400316
            let alpha = ewma_alpha();
            assert!((alpha - 0.0228400316_f64).abs() < 1e-9, "alpha={}", alpha);
            // By construction (1 - alpha)^half_life must be exactly one half.
            let decayed = (1.0 - alpha).powf(EWMA_HALF_LIFE_BARS);
            assert!((decayed - 0.5).abs() < 1e-12, "decayed={}", decayed);
        }

        #[test]
        fn first_value_seeds_filter_unsmoothed() {
            let mut f = SmoothedSignal::new();
            assert_eq!(f.value(), None);
            // The first value must pass straight through — no pull toward 0.5.
            let out = f.update(0.9);
            assert_eq!(out, 0.9);
            assert_eq!(f.value(), Some(0.9));
        }

        #[test]
        fn constant_input_converges_to_that_constant() {
            let mut f = SmoothedSignal::new();
            f.update(0.2); // seed away from the target so convergence is real
            for _ in 0..2000 {
                f.update(0.73);
            }
            let out = f.update(0.73);
            assert!((out - 0.73).abs() < 1e-6, "out={}", out);
        }

        #[test]
        fn step_reaches_half_after_exactly_half_life_updates() {
            // Seed at 0.0, then step to 1.0: after EWMA_HALF_LIFE_BARS updates
            // at the new level the output must be ~0.5 (the half-life property).
            let mut f = SmoothedSignal::new();
            f.update(0.0);
            let mut out = 0.0;
            for _ in 0..(EWMA_HALF_LIFE_BARS as usize) {
                out = f.update(1.0);
            }
            assert!((out - 0.5).abs() < 1e-9, "after 30 updates out={}", out);
            // And it is genuinely a *lag*: one update short of the half-life it
            // has covered strictly less than half the step.
            let mut g = SmoothedSignal::new();
            g.update(0.0);
            let mut short = 0.0;
            for _ in 0..(EWMA_HALF_LIFE_BARS as usize - 1) {
                short = g.update(1.0);
            }
            assert!(short < 0.5, "after 29 updates short={}", short);
        }

        #[test]
        fn warmup_none_leaves_state_unchanged() {
            // A warmup candle is expressed by the caller simply not calling
            // update(): the state must be neither advanced nor decayed.
            let mut f = SmoothedSignal::new();
            f.update(0.8);
            let before = f.value();
            // ... several "None" candles happen here; no update() call ...
            assert_eq!(f.value(), before);
            // The next Some resumes from exactly the retained state.
            let out = f.update(0.8);
            assert!((out - 0.8).abs() < 1e-12, "out={}", out);
        }

        #[test]
        fn output_stays_within_input_range() {
            // Convex combination ⇒ the filter can never leave [min, max] of its
            // inputs, so a normalized signal in [0,1] stays in [0,1].
            let mut f = SmoothedSignal::new();
            let inputs = [0.0, 1.0, 0.0, 1.0, 0.35, 0.99, 0.01];
            for x in inputs {
                let out = f.update(x);
                assert!((0.0..=1.0).contains(&out), "out={}", out);
            }
        }
    }

    mod zone_tests {
        use super::*;
        use crate::config::ZoneConfig;

        fn zone_4() -> ZoneConfig {
            ZoneConfig { range_max: 4.0, hold_jpy_below: 1.0, hold_btc_above: 3.0 }
        }

        #[test]
        fn apply_zone_zone_a_returns_zero() {
            assert_eq!(apply_zone(0.5, &zone_4()), 0.0);
            assert_eq!(apply_zone(0.0, &zone_4()), 0.0);
        }

        #[test]
        fn apply_zone_zone_b_midpoint() {
            let eff = apply_zone(2.0, &zone_4());
            assert!((eff - 0.5).abs() < 1e-9, "got {}", eff);
        }

        #[test]
        fn apply_zone_zone_b_lower_bound() {
            let eff = apply_zone(1.0, &zone_4());
            assert!((eff - 0.0).abs() < 1e-9, "got {}", eff);
        }

        #[test]
        fn apply_zone_zone_b_upper_bound() {
            let eff = apply_zone(3.0, &zone_4());
            assert!((eff - 1.0).abs() < 1e-9, "got {}", eff);
        }

        #[test]
        fn apply_zone_zone_c_returns_one() {
            assert_eq!(apply_zone(3.5, &zone_4()), 1.0);
            assert_eq!(apply_zone(4.0, &zone_4()), 1.0);
        }

        #[test]
        fn apply_zone_default_zones() {
            let zone = ZoneConfig::default();
            assert_eq!(apply_zone(0.0, &zone), 0.0);
            assert_eq!(apply_zone(0.1, &zone), 0.0);
            let mid = apply_zone(0.5, &zone);
            assert!((mid - 0.5).abs() < 1e-9, "mid={}", mid);
            assert_eq!(apply_zone(1.0, &zone), 1.0);
            assert_eq!(apply_zone(0.9, &zone), 1.0);
        }
    }
}
