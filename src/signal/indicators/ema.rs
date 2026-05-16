use std::collections::VecDeque;

use rust_decimal::prelude::ToPrimitive;

use crate::signal::{Indicator, IndicatorRawValues};
use crate::types::market::Candle;

pub struct Ema {
    period: usize,
    k: f64,
    /// SMA 初期化バッファ
    init_buffer: VecDeque<f64>,
    current: Option<f64>,
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

    /// EMA を更新する。売買判断は行わない。
    fn update(&mut self, candle: &Candle) {
        let close_f = candle.close.to_f64().unwrap_or(0.0);
        self.feed(close_f);
    }

    fn value(&self) -> Option<f64> {
        self.current
    }

    fn reset(&mut self) {
        self.init_buffer.clear();
        self.current = None;
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
        ema.update(&candle(100.0));
        assert!(ema.value().is_none());
        ema.update(&candle(200.0));
        assert!(ema.value().is_none());
    }

    #[test]
    fn returns_some_after_period_candles() {
        let mut ema = Ema::new(3);
        ema.update(&candle(100.0));
        ema.update(&candle(200.0));
        ema.update(&candle(300.0));
        assert!(ema.value().is_some());
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
