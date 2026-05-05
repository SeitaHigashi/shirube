pub mod engine;
pub mod indicators;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Serialize;

use crate::config::TradingConfig;
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

/// 目標BTC配分率。
/// - raw_signal: ゾーン変換前の生シグナル値。ZoneConfig.range_max までの範囲を取る。
/// - target_pct: ゾーン変換後の実効BTC配分率 ∈ [0.0, 1.0]（TradingEngineはこれを使う）
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AllocationSignal {
    /// 変換前の生シグナル値。デフォルトゾーンでは target_pct と同値。
    pub raw_signal: f64,
    pub target_pct: f64,
    pub confidence: f64,
}

impl AllocationSignal {
    pub fn neutral() -> Self {
        Self { raw_signal: 0.5, target_pct: 0.5, confidence: 0.0 }
    }

    /// レガシー互換コンストラクタ: raw_signal = target_pct として生成
    pub fn from_effective(target_pct: f64, confidence: f64) -> Self {
        Self { raw_signal: target_pct, target_pct, confidence }
    }

    pub fn is_bullish(&self) -> bool { self.target_pct > 0.5 }
    pub fn is_bearish(&self) -> bool { self.target_pct < 0.5 }
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

// ---- SignalDetail — API レスポンス用（集計 + 個別） ----

#[derive(Debug, Clone, Serialize)]
pub struct SignalDetail {
    pub aggregate: AllocationSignal,
    pub indicators: Vec<IndicatorSignal>,
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
