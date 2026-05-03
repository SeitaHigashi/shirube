pub mod engine;
pub mod indicators;

use rust_decimal::Decimal;
use serde::Serialize;

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

/// 目標BTC配分率。target_pct ∈ [0.0, 1.0]（0.0=全JPY, 0.5=中立, 1.0=全BTC）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AllocationSignal {
    pub target_pct: f64,
    pub confidence: f64,
}

impl AllocationSignal {
    pub fn neutral() -> Self {
        Self { target_pct: 0.5, confidence: 0.0 }
    }

    pub fn is_bullish(&self) -> bool { self.target_pct > 0.5 }
    pub fn is_bearish(&self) -> bool { self.target_pct < 0.5 }
}

// ---- SignalDetail — API レスポンス用（集計 + 個別） ----

#[derive(Debug, Clone, Serialize)]
pub struct SignalDetail {
    pub aggregate: AllocationSignal,
    pub indicators: Vec<IndicatorSignal>,
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
}
