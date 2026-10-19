use std::collections::VecDeque;

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

use crate::signal::{Indicator, IndicatorRawValues};
use crate::types::market::Candle;

pub struct Sma {
    period: usize,
    buffer: VecDeque<Decimal>,
    /// 直近の SMA 計算値。ウォームアップ完了後に設定される。
    current_sma: Option<f64>,
}

impl Sma {
    pub fn new(period: usize) -> Self {
        assert!(period >= 2, "SMA period must be >= 2");
        Self {
            period,
            buffer: VecDeque::with_capacity(period + 1),
            current_sma: None,
        }
    }

    fn compute_sma(&self) -> f64 {
        let sum: Decimal = self.buffer.iter().sum();
        (sum / Decimal::from(self.buffer.len()))
            .to_f64()
            .unwrap_or(0.0)
    }
}

impl Indicator for Sma {
    fn name(&self) -> &str {
        "SMA"
    }

    /// SMA を更新する。売買判断は行わない。
    fn update(&mut self, candle: &Candle) {
        let close = candle.close;

        self.buffer.push_back(close);
        if self.buffer.len() > self.period {
            self.buffer.pop_front();
        }

        if self.buffer.len() >= self.period {
            self.current_sma = Some(self.compute_sma());
        }
    }

    fn value(&self) -> Option<f64> {
        self.current_sma
    }

    fn reset(&mut self) {
        self.buffer.clear();
        self.current_sma = None;
    }

    fn min_periods(&self) -> usize {
        self.period
    }

    fn snapshot(&self) -> IndicatorRawValues {
        IndicatorRawValues::Scalar(self.value())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use rust_decimal_macros::dec;

    fn candle(close: Decimal) -> Candle {
        Candle {
            product_code: "BTC_JPY".into(),
            open_time: Utc::now(),
            resolution_secs: 60,
            open: close,
            high: close,
            low: close,
            close,
            volume: dec!(1),
        }
    }

    #[test]
    fn warmup_returns_none() {
        let mut sma = Sma::new(3);
        sma.update(&candle(dec!(100)));
        assert!(sma.value().is_none());
        sma.update(&candle(dec!(200)));
        assert!(sma.value().is_none());
    }

    #[test]
    fn returns_some_after_period_candles() {
        let mut sma = Sma::new(3);
        sma.update(&candle(dec!(100)));
        sma.update(&candle(dec!(200)));
        sma.update(&candle(dec!(300)));
        assert!(sma.value().is_some());
    }

    #[test]
    fn computes_correct_sma() {
        let mut sma = Sma::new(3);
        sma.update(&candle(dec!(100)));
        sma.update(&candle(dec!(200)));
        sma.update(&candle(dec!(300)));
        // SMA of [100, 200, 300] = 200
        let v = sma.value().unwrap();
        assert!((v - 200.0).abs() < 1e-6, "got {}", v);
    }

    #[test]
    fn reset_clears_state() {
        let mut sma = Sma::new(2);
        sma.update(&candle(dec!(100)));
        sma.update(&candle(dec!(200)));
        sma.reset();
        sma.update(&candle(dec!(100)));
        assert!(sma.value().is_none());
    }
}
