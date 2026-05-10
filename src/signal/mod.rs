pub mod engine;
pub mod indicators;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Serialize;

use crate::config::{TradingConfig, ZoneConfig};
use crate::types::market::Candle;

// ---- Signal ----

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Signal {
    Buy { price: Decimal, confidence: f64 },
    Sell { price: Decimal, confidence: f64 },
    Hold,
}

// ---- IndicatorSignal — 各インジケータの個別シグナル ----

#[derive(Debug, Clone, Serialize)]
pub struct IndicatorSignal {
    pub name: String,
    pub signal: Option<Signal>,
    /// 現在の計算値（SMA値、RSI値、MACDヒストグラム等）。ウォームアップ中は None。
    pub value: Option<f64>,
}

// ---- AllocationSignal — 連続配分シグナル ----

/// インジケータ集計結果。ゾーン変換前の生シグナル値と信頼度のみを持つ。
/// ゾーン変換（raw_signal → target_pct）は TradingEngine の compute_btc_target で行う。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AllocationSignal {
    /// ゾーン変換前の生シグナル値 [0.0, range_max]。
    /// Buy優勢 → range_max 方向、Sell優勢 → 0.0 方向、中立 → range_max / 2.0。
    pub raw_signal: f64,
    /// インジケータのうち方向性を持つもの（Buy/Sell）の割合 [0.0, 1.0]。
    /// 0.0 = 全インジケータが Hold、1.0 = 全インジケータが方向性あり。
    pub confidence: f64,
}

impl AllocationSignal {
    /// 中立シグナル: raw_signal = range_max/2, confidence = 0
    pub fn neutral() -> Self {
        Self { raw_signal: 0.5, confidence: 0.0 }
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
/// TradingEngine はこれを受け取り aggregate_with_zone() で AllocationSignal を計算する。
#[derive(Debug, Clone)]
pub struct IndicatorOutput {
    pub indicators: Vec<IndicatorSignal>,
    pub raw: IndicatorPoint,
    pub calculated_at: DateTime<Utc>,
}

// ---- 集計関数（純粋関数） ----

/// 複数インジケータのシグナルを ZoneConfig に従って AllocationSignal に合成する（純粋関数）。
/// TradingEngine が発注判断に使用する。
///
/// - None（ウォームアップ中）は集計から除外
/// - Buy は raw_signal を range_max 方向へ、Sell は 0.0 方向へ押す
/// - 中立（インジケータなし）は range_max / 2.0
pub fn aggregate_with_zone(signals: &[Option<Signal>], zone: &ZoneConfig) -> AllocationSignal {
    let mut delta_sum = 0.0f64;
    let mut active = 0usize;
    let mut directional = 0usize;

    for sig in signals.iter().flatten() {
        active += 1;
        match sig {
            Signal::Buy { confidence, .. } => {
                delta_sum += confidence;
                directional += 1;
            }
            Signal::Sell { confidence, .. } => {
                delta_sum -= confidence;
                directional += 1;
            }
            Signal::Hold => {}
        }
    }

    if active == 0 {
        return AllocationSignal::neutral();
    }

    // 正規化後の値 [0.0, 1.0] を range_max にスケール
    // Buy(1.0) 1本のみ → (0.5 + 0.5) * range_max = range_max
    // Sell(1.0) 1本のみ → (0.5 - 0.5) * range_max = 0.0
    let normalized = (0.5 + delta_sum / (active as f64 * 2.0)).clamp(0.0, 1.0);
    let raw_signal = normalized * zone.range_max;
    let confidence = directional as f64 / active as f64;
    // NOTE: apply_zone() is intentionally NOT called here.
    // Zone conversion (raw_signal → target_pct) is the responsibility of
    // TradingEngine::compute_btc_target(), keeping strategy judgment out of the signal layer.
    AllocationSignal { raw_signal, confidence }
}

/// デフォルト ZoneConfig（range_max=1.0）で集計する。
pub fn aggregate(signals: &[Option<Signal>]) -> AllocationSignal {
    aggregate_with_zone(signals, &ZoneConfig::default())
}

// ---- SignalDetail — API レスポンス用（集計 + 個別） ----

#[derive(Debug, Clone, Serialize)]
pub struct SignalDetail {
    pub aggregate: AllocationSignal,
    /// TradingEngine が compute_btc_target で算出した目標 BTC 配分率 [0.0, 1.0]。
    /// sticky_target を反映するため、Hold シグナル時も最後の有効値を保持する。
    /// sticky_target が未設定（ウォームアップ中）は 0.5（中立）を返す。
    pub target_pct: f64,
    pub indicators: Vec<IndicatorSignal>,
    /// 生インジケータ値（ウォームアップ中は None）。TradingEngine のガードロジックで使用。
    /// None の場合は JSON に含まれないため API 互換性を維持する。
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
#[derive(Debug, Clone, Serialize)]
pub struct IndicatorPoint {
    pub time: DateTime<Utc>,
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

    let mut sma = Sma::new(cfg.sma_period);
    let mut ema = Ema::new(cfg.ema_period);
    let mut rsi = Rsi::new(cfg.rsi_period);
    let mut macd = Macd::new(cfg.macd_fast, cfg.macd_slow, cfg.macd_signal);
    let mut bb = Bollinger::new(cfg.bollinger_period, cfg.bollinger_std);

    candles.iter().map(|c| {
        // 各インジケーターを更新（戻り値は Signal なので無視し、value/band_values を使う）
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
    /// Candle を受け取り、シグナルを返す。ウォームアップ中は None。
    fn update(&mut self, candle: &Candle) -> Option<Signal>;
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
    use std::collections::VecDeque;

    /// テスト用インジケータ。コンストラクト時に渡したシグナル列を順番に返す。
    pub struct MockIndicator {
        name: String,
        signals: VecDeque<Option<Signal>>,
    }

    impl MockIndicator {
        pub fn new(name: impl Into<String>, signals: Vec<Option<Signal>>) -> Self {
            Self {
                name: name.into(),
                signals: signals.into(),
            }
        }
    }

    impl Indicator for MockIndicator {
        fn name(&self) -> &str {
            &self.name
        }

        fn update(&mut self, _candle: &Candle) -> Option<Signal> {
            self.signals.pop_front().flatten()
        }

        fn value(&self) -> Option<f64> {
            None
        }

        fn reset(&mut self) {
            self.signals.clear();
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
    use mock::MockIndicator;
    use rust_decimal_macros::dec;

    fn dummy_candle() -> Candle {
        use chrono::Utc;
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
    fn signal_equality() {
        let s1 = Signal::Buy { price: dec!(9000000), confidence: 0.8 };
        let s2 = Signal::Buy { price: dec!(9000000), confidence: 0.8 };
        assert_eq!(s1, s2);
        assert_ne!(Signal::Hold, Signal::Buy { price: dec!(1), confidence: 0.5 });
    }

    #[test]
    fn mock_indicator_returns_signals_in_order() {
        let candle = dummy_candle();
        let signals = vec![
            Some(Signal::Buy { price: dec!(9000000), confidence: 0.8 }),
            None,
            Some(Signal::Hold),
        ];
        let mut ind = MockIndicator::new("test", signals);

        assert!(matches!(ind.update(&candle), Some(Signal::Buy { .. })));
        assert!(ind.update(&candle).is_none());
        assert!(matches!(ind.update(&candle), Some(Signal::Hold)));
    }

    #[test]
    fn mock_indicator_returns_none_when_exhausted() {
        let candle = dummy_candle();
        let mut ind = MockIndicator::new("empty", vec![]);
        assert!(ind.update(&candle).is_none());
    }

    mod aggregate_tests {
        use super::*;
        use rust_decimal_macros::dec;

        fn buy(conf: f64) -> Option<Signal> {
            Some(Signal::Buy { price: dec!(9000000), confidence: conf })
        }

        fn sell(conf: f64) -> Option<Signal> {
            Some(Signal::Sell { price: dec!(9000000), confidence: conf })
        }

        #[test]
        fn empty_signals_returns_neutral() {
            let result = aggregate(&[]);
            assert_eq!(result, AllocationSignal::neutral());
        }

        #[test]
        fn all_none_returns_neutral() {
            let result = aggregate(&[None, None, None]);
            assert_eq!(result, AllocationSignal::neutral());
        }

        #[test]
        fn buy_dominates() {
            let signals = vec![buy(0.8), sell(0.3), Some(Signal::Hold)];
            let result = aggregate(&signals);
            assert!(result.raw_signal > 0.5, "got {}", result.raw_signal);
        }

        #[test]
        fn sell_dominates() {
            let signals = vec![buy(0.2), sell(0.9)];
            let result = aggregate(&signals);
            assert!(result.raw_signal < 0.5, "got {}", result.raw_signal);
        }

        #[test]
        fn weak_buy_stays_near_neutral() {
            // delta = 0.1, active=3 → 0.5 + 0.1/6 ≈ 0.517
            let signals = vec![buy(0.1), Some(Signal::Hold), Some(Signal::Hold)];
            let result = aggregate(&signals);
            assert!(result.raw_signal > 0.5 && result.raw_signal < 0.6,
                "got {}", result.raw_signal);
        }

        #[test]
        fn tied_returns_neutral() {
            let signals = vec![buy(0.5), sell(0.5)];
            let result = aggregate(&signals);
            assert!((result.raw_signal - 0.5).abs() < 1e-9, "got {}", result.raw_signal);
        }

        #[test]
        fn single_strong_buy() {
            let signals = vec![buy(1.0)];
            let result = aggregate(&signals);
            assert!((result.raw_signal - 1.0).abs() < 1e-9, "got {}", result.raw_signal);
        }

        #[test]
        fn single_strong_sell() {
            let signals = vec![sell(1.0)];
            let result = aggregate(&signals);
            assert!((result.raw_signal - 0.0).abs() < 1e-9, "got {}", result.raw_signal);
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
            // デフォルト: range_max=1.0, hold_jpy_below=0.2, hold_btc_above=0.8
            let zone = ZoneConfig::default();
            // Zone A: < 0.2 → 0.0
            assert_eq!(apply_zone(0.0, &zone), 0.0);
            assert_eq!(apply_zone(0.1, &zone), 0.0);
            // Zone B: 0.2〜0.8 → 線形補間
            let mid = apply_zone(0.5, &zone);
            assert!((mid - 0.5).abs() < 1e-9, "mid={}", mid);
            // Zone C: > 0.8 → 1.0
            assert_eq!(apply_zone(1.0, &zone), 1.0);
            assert_eq!(apply_zone(0.9, &zone), 1.0);
        }
    }
}
