use std::collections::VecDeque;

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

use crate::signal::{Indicator, Signal};
use crate::types::market::Candle;

pub struct Sma {
    period: usize,
    buffer: VecDeque<Decimal>,
    prev_close: Option<Decimal>,
    prev_sma: Option<f64>,
}

impl Sma {
    pub fn new(period: usize) -> Self {
        assert!(period >= 2, "SMA period must be >= 2");
        Self {
            period,
            buffer: VecDeque::with_capacity(period + 1),
            prev_close: None,
            prev_sma: None,
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

    fn update(&mut self, candle: &Candle) -> Option<Signal> {
        let close = candle.close;

        self.buffer.push_back(close);
        if self.buffer.len() > self.period {
            self.buffer.pop_front();
        }

        if self.buffer.len() < self.period {
            self.prev_close = Some(close);
            return None;
        }

        let sma = self.compute_sma();
        let close_f = close.to_f64().unwrap_or(0.0);

        let signal = match (self.prev_close, self.prev_sma) {
            (Some(pc), Some(ps)) => {
                let pc_f = pc.to_f64().unwrap_or(0.0);
                if close_f > sma && pc_f <= ps {
                    Signal::Buy { price: close, confidence: 0.5 }
                } else if close_f < sma && pc_f >= ps {
                    Signal::Sell { price: close, confidence: 0.5 }
                } else {
                    Signal::Hold
                }
            }
            _ => Signal::Hold,
        };

        self.prev_close = Some(close);
        self.prev_sma = Some(sma);

        Some(signal)
    }

    fn reset(&mut self) {
        self.buffer.clear();
        self.prev_close = None;
        self.prev_sma = None;
    }

    fn min_periods(&self) -> usize {
        self.period
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
        assert!(sma.update(&candle(dec!(100))).is_none());
        assert!(sma.update(&candle(dec!(200))).is_none());
    }

    #[test]
    fn returns_some_after_period_candles() {
        let mut sma = Sma::new(3);
        sma.update(&candle(dec!(100)));
        sma.update(&candle(dec!(200)));
        let sig = sma.update(&candle(dec!(300)));
        assert!(sig.is_some());
    }

    #[test]
    fn detects_golden_cross() {
        let mut sma = Sma::new(3);
        // 最初 3本でウォームアップ (close < sma になるよう設定)
        // [100, 100, 100] → sma=100, close=100 → Hold
        sma.update(&candle(dec!(100)));
        sma.update(&candle(dec!(100)));
        sma.update(&candle(dec!(100))); // sma=100, close=100 → Hold (prev_sma 未設定)
        // 次の本: [100, 100, 50] → sma≈83.3, close=50 < sma → prev_close=100, prev_sma=100
        sma.update(&candle(dec!(50)));
        // 次の本: [100, 50, 200] → sma=116.7, close=200 > sma, prev_close=50 < prev_sma=83.3 → Buy
        let sig = sma.update(&candle(dec!(200)));
        assert!(matches!(sig, Some(Signal::Buy { .. })));
    }

    #[test]
    fn reset_clears_state() {
        let mut sma = Sma::new(2);
        sma.update(&candle(dec!(100)));
        sma.update(&candle(dec!(200)));
        sma.reset();
        // リセット後は再びウォームアップが必要
        assert!(sma.update(&candle(dec!(100))).is_none());
    }
}
