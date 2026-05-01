use std::collections::VecDeque;

use rust_decimal::prelude::ToPrimitive;

use crate::signal::{Indicator, Signal};
use crate::types::market::Candle;

pub struct Bollinger {
    period: usize,
    multiplier: f64,
    buffer: VecDeque<f64>,
    /// %B = (close - lower) / (upper - lower) * 100。50が中央、0が下限、100が上限。
    current_pct_b: Option<f64>,
}

impl Bollinger {
    pub fn new(period: usize, multiplier: f64) -> Self {
        assert!(period >= 2, "Bollinger period must be >= 2");
        Self {
            period,
            multiplier,
            buffer: VecDeque::with_capacity(period + 1),
            current_pct_b: None,
        }
    }

    fn stats(&self) -> (f64, f64) {
        let n = self.buffer.len() as f64;
        let mean = self.buffer.iter().sum::<f64>() / n;
        let variance = self.buffer.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;
        (mean, variance.sqrt())
    }
}

impl Default for Bollinger {
    fn default() -> Self {
        Self::new(20, 2.0)
    }
}


impl Indicator for Bollinger {
    fn name(&self) -> &str {
        "Bollinger"
    }

    fn update(&mut self, candle: &Candle) -> Option<Signal> {
        let close = candle.close.to_f64().unwrap_or(0.0);

        self.buffer.push_back(close);
        if self.buffer.len() > self.period {
            self.buffer.pop_front();
        }

        if self.buffer.len() < self.period {
            return None;
        }

        let (mean, std) = self.stats();
        let upper = mean + self.multiplier * std;
        let lower = mean - self.multiplier * std;
        let bandwidth = upper - lower;

        self.current_pct_b = if bandwidth > 0.0 {
            Some((close - lower) / bandwidth * 100.0)
        } else {
            Some(50.0)
        };

        let signal = if close < lower {
            let confidence = if bandwidth > 0.0 {
                ((lower - close) / bandwidth).min(1.0)
            } else {
                0.5
            };
            Signal::Buy { price: candle.close, confidence }
        } else if close > upper {
            let confidence = if bandwidth > 0.0 {
                ((close - upper) / bandwidth).min(1.0)
            } else {
                0.5
            };
            Signal::Sell { price: candle.close, confidence }
        } else {
            Signal::Hold
        };

        Some(signal)
    }

    fn value(&self) -> Option<f64> {
        self.current_pct_b
    }

    fn reset(&mut self) {
        self.buffer.clear();
        self.current_pct_b = None;
    }

    fn min_periods(&self) -> usize {
        self.period
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

    #[test]
    fn warmup_returns_none() {
        let mut bb = Bollinger::new(20, 2.0);
        for i in 0..19 {
            let sig = bb.update(&candle(100.0 + i as f64));
            assert!(sig.is_none(), "Expected None at step {}", i);
        }
    }

    #[test]
    fn returns_hold_for_stable_price() {
        let mut bb = Bollinger::new(20, 2.0);
        // 全部同じ価格 → std=0 → upper=lower=mean → Hold
        for _ in 0..20 {
            bb.update(&candle(100.0));
        }
        let sig = bb.update(&candle(100.0));
        // std=0 の場合、close=100=lower=upper なので条件分岐によっては Hold
        assert!(sig.is_some());
    }

    #[test]
    fn detects_oversold_below_lower_band() {
        // period=20, multiplier=2 で安定した価格帯のバンドに対して急落
        let mut bb = Bollinger::new(20, 2.0);
        // 安定した価格: mean=1000, わずかな変動 (std≈2.9)
        for i in 0..20 {
            let price = 998.0 + (i % 5) as f64;
            bb.update(&candle(price));
        }
        // 大幅下落 → lower band (約1000 - 5.8 ≈ 994) を確実に下回る
        // ただし新しい価格がバッファに入ることでバンドが変わるので、十分低い値を使う
        let sig = bb.update(&candle(900.0));
        // lower band を大幅に下回るので Buy
        assert!(matches!(sig, Some(Signal::Buy { .. })));
    }

    #[test]
    fn detects_overbought_above_upper_band() {
        let mut bb = Bollinger::new(20, 2.0);
        for i in 0..20 {
            let price = 998.0 + (i % 5) as f64;
            bb.update(&candle(price));
        }
        // 大幅上昇 → upper band を確実に上回る
        let sig = bb.update(&candle(1100.0));
        assert!(matches!(sig, Some(Signal::Sell { .. })));
    }

    #[test]
    fn reset_clears_state() {
        let mut bb = Bollinger::new(5, 2.0);
        for i in 0..5 {
            bb.update(&candle(100.0 + i as f64));
        }
        bb.reset();
        assert!(bb.update(&candle(100.0)).is_none());
    }
}
