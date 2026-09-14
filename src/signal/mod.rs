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

// ---- TimeSmoothedSignal — 実時間減衰 EWMA フィルタ ----

/// Half-life, in wall-clock seconds, of the exponential decay applied to the
/// composite normalized signal. 1800 s = 30 minutes.
///
/// NOTE: 1800.0 is an a priori round figure (half an hour) chosen before any
/// backtest was run. It is NOT fitted on the holdout window, nor on any other
/// window; if it turns out to be the wrong scale, that is a result to report
/// rather than a value to retune.
pub(crate) const EWMA_HALF_LIFE_SECS: f64 = 1800.0;

/// Exponentially weighted moving average of the composite normalized signal
/// whose decay is a function of ELAPSED WALL-CLOCK TIME between consecutive
/// updates, not of the number of updates.
///
/// # Formula
///
/// With `dt = (now - self.at)` in seconds (clamped to `>= 0.0`):
///
/// ```text
///   decay = 0.5 ^ (dt / EWMA_HALF_LIFE_SECS)     ∈ (0.0, 1.0]
///   value = decay * prev + (1.0 - decay) * new   ∈ [0.0, 1.0] for inputs in [0.0, 1.0]
/// ```
///
/// After exactly `EWMA_HALF_LIFE_SECS` of elapsed time the weight on the old
/// state is 0.5, whatever cadence the updates arrived at. Because the output is
/// a convex combination of the previous state and the new input, it stays
/// inside the input range — so a `normalized` in `[0.0, 1.0]` yields a smoothed
/// value in `[0.0, 1.0]` and the downstream `range_max` scaling / `apply_zone`
/// mapping see exactly the domain they already expect.
///
/// # NOTE: why wall-clock and not bars
///
/// This is the wall-clock reformulation of the earlier bar-indexed
/// `ewma-smoothed-composite-signal` variant, which expressed its half-life in
/// BARS (30 bars). That formulation means 30 minutes only when one update is
/// one 60 s bar, which is true in the backtest simulator but NOT live: the live
/// `SignalEngine` re-evaluates at roughly 4 Hz, so a 30-update half-life there
/// would have been ~7.5 seconds of market time — a completely different filter
/// from the one the backtest measured. Deriving the decay from elapsed time
/// makes the filter mean the same thing at both cadences (one update per 60 s
/// bar in the simulator, roughly one per 250 ms live) WITHOUT changing live
/// scheduling. The ~4 Hz live evaluation rate itself is a separate open
/// question and is deliberately left alone here.
#[derive(Debug, Clone, Default)]
pub(crate) struct TimeSmoothedSignal {
    /// Current smoothed value. `None` until the first update seeds it.
    value: Option<f64>,
    /// Timestamp of the update that produced `value`. `None` until seeded.
    at: Option<DateTime<Utc>>,
}

impl TimeSmoothedSignal {
    /// Create an unseeded filter.
    pub(crate) fn new() -> Self {
        Self { value: None, at: None }
    }

    /// Feed one composite normalized value observed at `now` and return the
    /// smoothed value to act on.
    ///
    /// The first value the filter ever sees is returned unchanged and becomes
    /// the initial state, so the first evaluated candle trades on an unsmoothed
    /// signal rather than on one biased toward an arbitrary 0.5 seed.
    ///
    /// A non-monotonic or duplicate timestamp yields `dt = 0`, hence
    /// `decay = 1.0`, which leaves the state at its previous value: out-of-order
    /// input can never pull the filter backwards in time.
    pub(crate) fn update(&mut self, normalized: f64, now: DateTime<Utc>) -> f64 {
        match (self.value, self.at) {
            (Some(prev), Some(prev_at)) => {
                // Elapsed seconds since the last update, at sub-second
                // resolution (live updates arrive ~4x per second, so an
                // integer-second dt would quantise almost all of them to 0).
                let dt_secs =
                    ((now - prev_at).num_milliseconds() as f64 / 1000.0).max(0.0);
                let decay = 0.5f64.powf(dt_secs / EWMA_HALF_LIFE_SECS);
                let smoothed = decay * prev + (1.0 - decay) * normalized;
                self.value = Some(smoothed);
                self.at = Some(now);
                smoothed
            }
            // Seed: adopt the first observation verbatim.
            _ => {
                self.value = Some(normalized);
                self.at = Some(now);
                normalized
            }
        }
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

    mod time_smoothed_signal_tests {
        use super::*;
        use chrono::{Duration, TimeZone};

        fn base() -> DateTime<Utc> {
            Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
        }

        #[test]
        fn first_value_is_returned_unsmoothed_and_seeds_the_filter() {
            let mut f = TimeSmoothedSignal::new();
            let out = f.update(0.8, base());
            assert_eq!(out, 0.8, "the first observation must pass through verbatim");
            assert_eq!(f.value, Some(0.8));
            assert_eq!(f.at, Some(base()));
        }

        #[test]
        fn constant_input_converges_to_that_constant() {
            let mut f = TimeSmoothedSignal::new();
            f.update(0.2, base());
            let mut last = 0.2;
            for i in 1..=200 {
                last = f.update(0.2, base() + Duration::minutes(i));
            }
            assert!((last - 0.2).abs() < 1e-12, "got {last}");
        }

        #[test]
        fn step_reaches_half_after_one_half_life_at_60s_cadence() {
            // 0.0 → step to 1.0, delivered as 30 updates of 60 s = 1800 s.
            let mut f = TimeSmoothedSignal::new();
            f.update(0.0, base());
            let mut out = 0.0;
            for i in 1..=30 {
                out = f.update(1.0, base() + Duration::seconds(60 * i));
            }
            assert!((out - 0.5).abs() < 1e-9, "60s cadence gave {out}, expected ~0.5");
        }

        #[test]
        fn step_reaches_half_after_one_half_life_at_4hz_cadence() {
            // Same 1800 s of elapsed time, delivered as 7200 updates of 250 ms.
            // This cadence-invariance is the whole point of the hypothesis:
            // the result must match the 60 s case above, not decay 240x faster.
            let mut f = TimeSmoothedSignal::new();
            f.update(0.0, base());
            let mut out = 0.0;
            for i in 1..=7200 {
                out = f.update(1.0, base() + Duration::milliseconds(250 * i));
            }
            assert!((out - 0.5).abs() < 1e-9, "4 Hz cadence gave {out}, expected ~0.5");
        }

        #[test]
        fn cadence_invariance_the_two_paths_agree() {
            // Directly compare the simulator-like and live-like cadences over
            // the same wall-clock span: they must land on the same value.
            let mut slow = TimeSmoothedSignal::new();
            slow.update(0.0, base());
            let mut slow_out = 0.0;
            for i in 1..=30 {
                slow_out = slow.update(1.0, base() + Duration::seconds(60 * i));
            }

            let mut fast = TimeSmoothedSignal::new();
            fast.update(0.0, base());
            let mut fast_out = 0.0;
            for i in 1..=7200 {
                fast_out = fast.update(1.0, base() + Duration::milliseconds(250 * i));
            }

            assert!(
                (slow_out - fast_out).abs() < 1e-9,
                "60s cadence {slow_out} vs 4Hz cadence {fast_out}"
            );
        }

        #[test]
        fn zero_dt_leaves_the_value_unchanged() {
            let mut f = TimeSmoothedSignal::new();
            f.update(0.3, base());
            let out = f.update(1.0, base());
            assert!((out - 0.3).abs() < 1e-12, "dt=0 must give decay=1.0, got {out}");
        }

        #[test]
        fn out_of_order_timestamp_is_clamped_to_zero_dt() {
            let mut f = TimeSmoothedSignal::new();
            f.update(0.3, base() + Duration::minutes(10));
            let out = f.update(1.0, base());
            assert!((out - 0.3).abs() < 1e-12, "negative dt must clamp to 0, got {out}");
        }

        #[test]
        fn warmup_point_leaves_state_untouched() {
            // A warmup candle is one where compute_btc_target returns None; the
            // caller simply does not call update(), so neither value nor `at`
            // moves and no decay toward any neutral value happens.
            let mut f = TimeSmoothedSignal::new();
            f.update(0.75, base());
            let before = (f.value, f.at);
            let warmup: Option<f64> = None;
            if let Some(n) = warmup {
                f.update(n, base() + Duration::minutes(5));
            }
            assert_eq!((f.value, f.at), before);

            // And the next real update decays from the preserved state using
            // the full elapsed time across the skipped candles.
            let out = f.update(0.75, base() + Duration::minutes(5));
            assert!((out - 0.75).abs() < 1e-12, "got {out}");
        }

        #[test]
        fn output_stays_within_the_input_range() {
            let mut f = TimeSmoothedSignal::new();
            let inputs = [0.0, 1.0, 0.5, 0.0, 1.0, 0.25, 0.9];
            for (i, v) in inputs.iter().enumerate() {
                let out = f.update(*v, base() + Duration::seconds(60 * i as i64));
                assert!((0.0..=1.0).contains(&out), "out of range: {out}");
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
