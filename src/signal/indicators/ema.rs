use std::collections::VecDeque;

use rust_decimal::prelude::ToPrimitive;

use crate::signal::{Indicator, Signal};
use crate::types::market::Candle;

pub struct Ema {
    period: usize,
    k: f64,
    /// SMA 初期化バッファ
    init_buffer: VecDeque<f64>,
    current: Option<f64>,
    prev_close: Option<f64>,
    prev_ema: Option<f64>,
}

impl Ema {
    pub fn new(period: usize) -> Self {
        assert!(period >= 2, "EMA period must be >= 2");
        let k = 2.0 / (period as f64 + 1.0);
        Self {
            period,
            k,
            init_buffer: VecDeque::with_capacity(period + 1),
            current: None,
            prev_close: None,
            prev_ema: None,
        }
    }

    /// 現在の EMA 値を返す（他のインジケータから内部利用）
    pub fn value(&self) -> Option<f64> {
        self.current
    }

    /// close 値だけ食わせて EMA を更新する（MACD 内部利用）
    pub fn feed(&mut self, close: f64) -> Option<f64> {
        if self.current.is_none() {
            self.init_buffer.push_back(close);
            if self.init_buffer.len() < self.period {
                return None;
            }
            // SMA で初期値を計算
            let sum: f64 = self.init_buffer.iter().sum();
            let sma = sum / self.init_buffer.len() as f64;
            self.current = Some(sma);
            return self.current;
        }
        let ema = close * self.k + self.current.unwrap() * (1.0 - self.k);
        self.current = Some(ema);
        self.current
    }
}

impl Indicator for Ema {
    fn name(&self) -> &str {
        "EMA"
    }

    fn update(&mut self, candle: &Candle) -> Option<Signal> {
        let close_f = candle.close.to_f64().unwrap_or(0.0);

        let ema = self.feed(close_f)?;

        let signal = match (self.prev_close, self.prev_ema) {
            (Some(pc), Some(pe)) => {
                if close_f > ema && pc <= pe {
                    Signal::Buy { price: candle.close, confidence: 0.5 }
                } else if close_f < ema && pc >= pe {
                    Signal::Sell { price: candle.close, confidence: 0.5 }
                } else {
                    Signal::Hold
                }
            }
            _ => Signal::Hold,
        };

        self.prev_close = Some(close_f);
        self.prev_ema = Some(ema);

        Some(signal)
    }

    fn reset(&mut self) {
        self.init_buffer.clear();
        self.current = None;
        self.prev_close = None;
        self.prev_ema = None;
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
        let mut ema = Ema::new(3);
        assert!(ema.update(&candle(100.0)).is_none());
        assert!(ema.update(&candle(200.0)).is_none());
    }

    #[test]
    fn initializes_with_sma() {
        let mut ema = Ema::new(3);
        ema.feed(100.0);
        ema.feed(200.0);
        let v = ema.feed(300.0).unwrap();
        // SMA of [100, 200, 300] = 200
        assert!((v - 200.0).abs() < 1e-9);
    }

    #[test]
    fn ema_calculation() {
        let mut ema = Ema::new(3);
        // k = 2/(3+1) = 0.5
        ema.feed(100.0);
        ema.feed(200.0);
        let v0 = ema.feed(300.0).unwrap(); // SMA = 200
        // next: 400 * 0.5 + 200 * 0.5 = 300
        let v1 = ema.feed(400.0).unwrap();
        assert!((v1 - 300.0).abs() < 1e-9);
        let _ = v0;
    }

    #[test]
    fn reset_clears_state() {
        let mut ema = Ema::new(3);
        ema.feed(100.0);
        ema.feed(200.0);
        ema.feed(300.0);
        ema.reset();
        assert!(ema.value().is_none());
        assert!(ema.feed(100.0).is_none());
    }
}
