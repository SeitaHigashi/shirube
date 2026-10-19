use rust_decimal::prelude::ToPrimitive;

use crate::signal::{Indicator, IndicatorRawValues};
use crate::types::market::Candle;

/// Wilder 平滑化フェーズの状態。
///
/// NOTE: `avg_gain` / `avg_loss` / `prev_close` は必ず同時にセットされ、
/// 常に揃って存在するか揃って不在かのどちらかである。個別の `Option` に
/// 分けているとこの不変条件がコンパイラから見えず、更新側の `unwrap()` が
/// 「他フィールドの `is_none()` チェック」に暗黙依存してしまう。1つの
/// `Option<WilderState>` に畳むことで、その不変条件を型で保証する。
struct WilderState {
    avg_gain: f64,
    avg_loss: f64,
    prev_close: f64,
}

impl WilderState {
    /// RSI = 100 - 100 / (1 + avg_gain / avg_loss)（範囲: 0.0〜100.0）
    fn rsi(&self) -> f64 {
        if self.avg_loss == 0.0 {
            return 100.0;
        }
        100.0 - 100.0 / (1.0 + self.avg_gain / self.avg_loss)
    }
}

pub struct Rsi {
    period: usize,
    closes: Vec<f64>,
    /// ウォームアップ完了後のみ `Some`。
    state: Option<WilderState>,
    current_rsi: Option<f64>,
}

impl Rsi {
    pub fn new(period: usize) -> Self {
        assert!(period >= 2, "RSI period must be >= 2");
        Self {
            period,
            closes: Vec::with_capacity(period + 2),
            state: None,
            current_rsi: None,
        }
    }
}

impl Indicator for Rsi {
    fn name(&self) -> &str {
        "RSI"
    }

    /// RSI を更新する。売買判断は行わない。
    fn update(&mut self, candle: &Candle) {
        let close = candle.close.to_f64().unwrap_or(0.0);

        let state = match &mut self.state {
            // ウォームアップフェーズ: closes を蓄積
            None => {
                self.closes.push(close);
                if self.closes.len() <= self.period {
                    return;
                }
                // period+1 本目で最初の avg_gain / avg_loss を計算
                let gains: f64 = self.closes.windows(2)
                    .map(|w| (w[1] - w[0]).max(0.0))
                    .sum::<f64>() / self.period as f64;
                let losses: f64 = self.closes.windows(2)
                    .map(|w| (w[0] - w[1]).max(0.0))
                    .sum::<f64>() / self.period as f64;
                self.state.insert(WilderState {
                    avg_gain: gains,
                    avg_loss: losses,
                    prev_close: close,
                })
            }
            // Wilder の平滑化
            Some(state) => {
                let change = close - state.prev_close;
                let gain = change.max(0.0);
                let loss = (-change).max(0.0);

                state.avg_gain =
                    (state.avg_gain * (self.period as f64 - 1.0) + gain) / self.period as f64;
                state.avg_loss =
                    (state.avg_loss * (self.period as f64 - 1.0) + loss) / self.period as f64;
                state.prev_close = close;
                state
            }
        };
        self.current_rsi = Some(state.rsi());
    }

    fn value(&self) -> Option<f64> {
        self.current_rsi
    }

    fn reset(&mut self) {
        self.closes.clear();
        self.state = None;
        self.current_rsi = None;
    }

    fn min_periods(&self) -> usize {
        self.period + 1
    }

    fn snapshot(&self) -> IndicatorRawValues {
        IndicatorRawValues::Scalar(self.current_rsi)
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
        let mut rsi = Rsi::new(14);
        for i in 0..14 {
            rsi.update(&candle(100.0 + i as f64));
            assert!(rsi.value().is_none(), "Expected None at step {}", i);
        }
    }

    #[test]
    fn returns_some_after_period_plus_one() {
        let mut rsi = Rsi::new(14);
        for i in 0..=15 {
            rsi.update(&candle(100.0 + i as f64));
        }
        assert!(rsi.value().is_some());
    }

    #[test]
    fn all_gains_gives_rsi_100() {
        let mut rsi = Rsi::new(3);
        // 全部上昇 → RSI ≈ 100
        for i in 0..=3 {
            rsi.update(&candle(100.0 + i as f64 * 10.0));
        }
        rsi.update(&candle(150.0));
        // RSI=100 → value > 70
        assert!(rsi.value().unwrap() > 70.0);
    }

    #[test]
    fn all_losses_gives_oversold() {
        let mut rsi = Rsi::new(3);
        // 全部下落 → RSI ≈ 0
        for i in 0..=3 {
            rsi.update(&candle(100.0 - i as f64 * 10.0));
        }
        rsi.update(&candle(50.0));
        // RSI ≈ 0 → value < 30
        assert!(rsi.value().unwrap() < 30.0);
    }

    #[test]
    fn reset_clears_state() {
        let mut rsi = Rsi::new(3);
        for i in 0..=3 {
            rsi.update(&candle(100.0 + i as f64));
        }
        rsi.reset();
        rsi.update(&candle(100.0));
        assert!(rsi.value().is_none());
    }
}
