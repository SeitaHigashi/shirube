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
    /// Realized volatility: population standard deviation of the last
    /// `REALIZED_VOL_WINDOW` close-to-close simple returns (dimensionless,
    /// per-bar; ~0.0005 for 1-minute BTC/JPY bars). `None` until
    /// `REALIZED_VOL_WINDOW + 1` closes have been seen, and always `None` on
    /// the live `SignalEngine` path, which does not track it (see
    /// `TradingEngine::compute_btc_target`, where `None` means "no scaling").
    pub realized_vol: Option<f64>,
}

/// Number of close-to-close returns in the rolling realized-volatility window.
///
/// NOTE: hypothesis `realized-vol-scaled-exposure-1m-recalibrated`. 1,440 bars
/// = 24h at the pipeline's 1-minute resolution (live `BASE_RESOLUTION = 60`).
pub const REALIZED_VOL_WINDOW: usize = 1440;

/// O(1)-per-update rolling population standard deviation of simple returns.
///
/// Keeps the last `window` returns in a ring buffer together with their
/// running sum and sum of squares, so each new close costs one push and at
/// most one eviction regardless of window length.
struct RollingReturnVol {
    window: usize,
    prev_close: Option<f64>,
    returns: std::collections::VecDeque<f64>,
    sum: f64,
    sum_sq: f64,
}

impl RollingReturnVol {
    fn new(window: usize) -> Self {
        Self {
            window,
            prev_close: None,
            returns: std::collections::VecDeque::with_capacity(window + 1),
            sum: 0.0,
            sum_sq: 0.0,
        }
    }

    /// Feed the next close and return the current realized vol, or `None`
    /// while fewer than `window` returns (i.e. `window + 1` closes) are held.
    ///
    /// # Formula
    ///   r_t   = close_t / close_{t-1} - 1
    ///   sigma = sqrt( E[r^2] - E[r]^2 )   over the last `window` returns
    /// A non-positive or non-finite close breaks the return chain: it is not
    /// used as a reference price, so no infinite/NaN return enters the sums.
    fn update(&mut self, close: Option<f64>) -> Option<f64> {
        let close = close.filter(|c| c.is_finite() && *c > 0.0);
        if let (Some(prev), Some(cur)) = (self.prev_close, close) {
            let r = cur / prev - 1.0;
            self.returns.push_back(r);
            self.sum += r;
            self.sum_sq += r * r;
            if self.returns.len() > self.window {
                if let Some(old) = self.returns.pop_front() {
                    self.sum -= old;
                    self.sum_sq -= old * old;
                }
            }
        }
        if close.is_some() {
            self.prev_close = close;
        }
        if self.returns.len() < self.window {
            return None;
        }
        let n = self.window as f64;
        let mean = self.sum / n;
        // max(0.0): running-sum cancellation can push the variance a hair
        // below zero for a near-constant series.
        let var = (self.sum_sq / n - mean * mean).max(0.0);
        Some(var.sqrt())
    }
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
    let mut rv = RollingReturnVol::new(REALIZED_VOL_WINDOW);

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
            realized_vol: rv.update(c.close.to_f64()),
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
    fn compute_indicators_realized_vol_matches_naive_population_stdev() {
        use rust_decimal::Decimal;
        use rust_decimal::prelude::FromPrimitive;
        // Deterministic pseudo-random walk long enough to roll the window.
        let n = REALIZED_VOL_WINDOW + 300;
        let mut price = 9_000_000.0f64;
        let mut closes = Vec::with_capacity(n);
        let mut seed: u64 = 42;
        for _ in 0..n {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let u = (seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5;
            price *= 1.0 + u * 0.002;
            closes.push(price.round());
        }
        let candles: Vec<Candle> = closes.iter().map(|c| Candle {
            close: Decimal::from_f64(*c).unwrap(),
            ..dummy_candle()
        }).collect();
        let points = compute_indicators(&candles, &crate::config::TradingConfig::default());

        // None until REALIZED_VOL_WINDOW + 1 closes have been seen.
        assert!(points[..REALIZED_VOL_WINDOW].iter().all(|p| p.realized_vol.is_none()));
        assert!(points[REALIZED_VOL_WINDOW..].iter().all(|p| p.realized_vol.is_some()));

        // Compare against a naive two-pass population stdev at a few indices.
        for &i in &[REALIZED_VOL_WINDOW, REALIZED_VOL_WINDOW + 1, n - 1] {
            let rets: Vec<f64> = (i + 1 - REALIZED_VOL_WINDOW..=i)
                .map(|j| closes[j] / closes[j - 1] - 1.0)
                .collect();
            let mean = rets.iter().sum::<f64>() / rets.len() as f64;
            let var = rets.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / rets.len() as f64;
            let got = points[i].realized_vol.unwrap();
            assert!((got - var.sqrt()).abs() < 1e-12, "i={} got {} want {}", i, got, var.sqrt());
        }
    }

    #[test]
    fn mock_indicator_returns_none_when_exhausted() {
        use mock::MockIndicator;
        let candle = dummy_candle();
        let mut ind = MockIndicator::new("empty", vec![]);
        ind.update(&candle);
        assert!(ind.value().is_none());
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
