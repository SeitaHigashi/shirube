use rust_decimal::prelude::ToPrimitive;

use super::ema::Ema;
use crate::signal::{Indicator, Signal};
use crate::types::market::Candle;

pub struct Macd {
    fast: Ema,
    slow: Ema,
    signal_ema: Ema,
    prev_histogram: Option<f64>,
    /// 最後に計算した (macd_line, signal_line, histogram)。API 向けに保持。
    last_components: Option<(f64, f64, f64)>,
}

impl Macd {
    pub fn new(fast_period: usize, slow_period: usize, signal_period: usize) -> Self {
        Self {
            fast: Ema::new(fast_period),
            slow: Ema::new(slow_period),
            signal_ema: Ema::new(signal_period),
            prev_histogram: None,
            last_components: None,
        }
    }

    /// 最後に計算した (macd_line, signal_line, histogram) を返す。ウォームアップ中は None。
    pub fn macd_components(&self) -> Option<(f64, f64, f64)> {
        self.last_components
    }
}

impl Default for Macd {
    fn default() -> Self {
        Self::new(12, 26, 9)
    }
}

impl Indicator for Macd {
    fn name(&self) -> &str {
        "MACD"
    }

    fn update(&mut self, candle: &Candle) -> Option<Signal> {
        let close = candle.close.to_f64().unwrap_or(0.0);

        // fast と slow は常に feed する (slow の init バッファが溜まるよう)
        let fast_val = self.fast.feed(close);
        let slow_val = self.slow.feed(close);

        let (fast_val, slow_val) = match (fast_val, slow_val) {
            (Some(f), Some(s)) => (f, s),
            _ => return None,
        };
        let macd_line = fast_val - slow_val;

        let signal_val = self.signal_ema.feed(macd_line)?;
        let histogram = macd_line - signal_val;

        let signal = match self.prev_histogram {
            Some(prev) => {
                if histogram > 0.0 && prev <= 0.0 {
                    Signal::Buy { price: candle.close, confidence: (histogram.abs() / (histogram.abs() + 1.0)).min(1.0) }
                } else if histogram < 0.0 && prev >= 0.0 {
                    Signal::Sell { price: candle.close, confidence: (histogram.abs() / (histogram.abs() + 1.0)).min(1.0) }
                } else {
                    Signal::Hold
                }
            }
            None => Signal::Hold,
        };

        self.prev_histogram = Some(histogram);
        self.last_components = Some((macd_line, signal_val, histogram));
        Some(signal)
    }

    fn value(&self) -> Option<f64> {
        self.prev_histogram
    }

    fn reset(&mut self) {
        self.fast.reset();
        self.slow.reset();
        self.signal_ema.reset();
        self.prev_histogram = None;
        self.last_components = None;
    }

    fn min_periods(&self) -> usize {
        // slow(26) + signal(9) - 1 = 34
        26 + 9 - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    fn candle(close: f64) -> Candle {
        let c = Decimal::try_from(close).unwrap();
        Candle {
            product_code: "BTC_JPY".into(),
            open_time: Utc::now(),
            resolution_secs: 60,
            open: c, high: c, low: c, close: c,
            volume: dec!(1),
        }
    }

    fn feed_n(macd: &mut Macd, price: f64, n: usize) {
        for _ in 0..n {
            macd.update(&candle(price));
        }
    }

    #[test]
    fn warmup_returns_none() {
        let mut macd = Macd::default();
        // 34本未満 (slow=26, signal=9 → 26+9-1=34本目まで None)
        for i in 0..33 {
            let sig = macd.update(&candle(100.0 + i as f64));
            assert!(sig.is_none(), "Expected None at step {}", i);
        }
    }

    #[test]
    fn returns_some_after_warmup() {
        let mut macd = Macd::default();
        // 34本フィードして Some が返ること
        for i in 0..34 {
            macd.update(&candle(100.0 + i as f64));
        }
        let sig = macd.update(&candle(134.0));
        assert!(sig.is_some());
    }

    #[test]
    fn hold_signal_during_trend() {
        // ウォームアップ後に Hold シグナルが返ることを確認
        let mut macd = Macd::default();
        for i in 0..35 {
            macd.update(&candle(100.0 + i as f64 * 0.5));
        }
        let sig = macd.update(&candle(117.5));
        // ウォームアップ完了後は Some が返る
        assert!(sig.is_some());
    }

    #[test]
    fn reset_clears_state() {
        let mut macd = Macd::default();
        feed_n(&mut macd, 100.0, 40);
        macd.reset();
        assert!(macd.update(&candle(100.0)).is_none());
    }
}
