use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

use crate::signal::{Indicator, Signal};
use crate::types::market::Candle;

pub struct Rsi {
    period: usize,
    closes: Vec<f64>,
    avg_gain: Option<f64>,
    avg_loss: Option<f64>,
    prev_close: Option<f64>,
}

impl Rsi {
    pub fn new(period: usize) -> Self {
        assert!(period >= 2, "RSI period must be >= 2");
        Self {
            period,
            closes: Vec::with_capacity(period + 2),
            avg_gain: None,
            avg_loss: None,
            prev_close: None,
        }
    }

    fn compute_rsi(&self) -> f64 {
        let ag = self.avg_gain.unwrap_or(0.0);
        let al = self.avg_loss.unwrap_or(0.0);
        if al == 0.0 {
            return 100.0;
        }
        100.0 - 100.0 / (1.0 + ag / al)
    }
}

impl Indicator for Rsi {
    fn name(&self) -> &str {
        "RSI"
    }

    fn update(&mut self, candle: &Candle) -> Option<Signal> {
        let close = candle.close.to_f64().unwrap_or(0.0);

        // ウォームアップフェーズ: closes を蓄積
        if self.avg_gain.is_none() {
            self.closes.push(close);
            if self.closes.len() <= self.period {
                return None;
            }
            // period+1 本目で最初の avg_gain / avg_loss を計算
            let gains: f64 = self.closes.windows(2)
                .map(|w| (w[1] - w[0]).max(0.0))
                .sum::<f64>() / self.period as f64;
            let losses: f64 = self.closes.windows(2)
                .map(|w| (w[0] - w[1]).max(0.0))
                .sum::<f64>() / self.period as f64;
            self.avg_gain = Some(gains);
            self.avg_loss = Some(losses);
            self.prev_close = Some(close);
            let rsi = self.compute_rsi();
            return Some(rsi_to_signal(rsi, candle.close));
        }

        // Wilder の平滑化
        let prev = self.prev_close.unwrap();
        let change = close - prev;
        let gain = change.max(0.0);
        let loss = (-change).max(0.0);

        let ag = (self.avg_gain.unwrap() * (self.period as f64 - 1.0) + gain) / self.period as f64;
        let al = (self.avg_loss.unwrap() * (self.period as f64 - 1.0) + loss) / self.period as f64;
        self.avg_gain = Some(ag);
        self.avg_loss = Some(al);
        self.prev_close = Some(close);

        let rsi = self.compute_rsi();
        Some(rsi_to_signal(rsi, candle.close))
    }

    fn reset(&mut self) {
        self.closes.clear();
        self.avg_gain = None;
        self.avg_loss = None;
        self.prev_close = None;
    }

    fn min_periods(&self) -> usize {
        self.period + 1
    }
}

fn rsi_to_signal(rsi: f64, price: Decimal) -> Signal {
    if rsi < 30.0 {
        Signal::Buy { price, confidence: (30.0 - rsi) / 30.0 }
    } else if rsi > 70.0 {
        Signal::Sell { price, confidence: (rsi - 70.0) / 30.0 }
    } else {
        Signal::Hold
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
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
        let mut rsi = Rsi::new(14);
        for i in 0..14 {
            let sig = rsi.update(&candle(100.0 + i as f64));
            assert!(sig.is_none(), "Expected None at step {}", i);
        }
    }

    #[test]
    fn returns_some_after_period_plus_one() {
        let mut rsi = Rsi::new(14);
        for i in 0..=14 {
            rsi.update(&candle(100.0 + i as f64));
        }
        let sig = rsi.update(&candle(115.0));
        assert!(sig.is_some());
    }

    #[test]
    fn all_gains_gives_rsi_100() {
        let mut rsi = Rsi::new(3);
        // 全部上昇 → RSI ≈ 100
        for i in 0..=3 {
            rsi.update(&candle(100.0 + i as f64 * 10.0));
        }
        let sig = rsi.update(&candle(150.0));
        // RSI=100 → Sell (>70)
        assert!(matches!(sig, Some(Signal::Sell { .. })));
    }

    #[test]
    fn all_losses_gives_oversold() {
        let mut rsi = Rsi::new(3);
        // 全部下落 → RSI ≈ 0 → Buy
        for i in 0..=3 {
            rsi.update(&candle(100.0 - i as f64 * 10.0));
        }
        let sig = rsi.update(&candle(50.0));
        assert!(matches!(sig, Some(Signal::Buy { .. })));
    }

    #[test]
    fn reset_clears_state() {
        let mut rsi = Rsi::new(3);
        for i in 0..=3 {
            rsi.update(&candle(100.0 + i as f64));
        }
        rsi.reset();
        assert!(rsi.update(&candle(100.0)).is_none());
    }
}
