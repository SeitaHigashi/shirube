use tokio::sync::broadcast;
use tracing::{debug, warn};

use super::{AllocationSignal, Indicator, IndicatorSignal, Signal, SignalDetail};
use crate::types::market::Candle;

pub struct SignalEngine {
    indicators: Vec<Box<dyn Indicator>>,
    candle_rx: broadcast::Receiver<Candle>,
    signal_tx: broadcast::Sender<SignalDetail>,
    threshold: f64,
}

impl SignalEngine {
    pub fn new(
        indicators: Vec<Box<dyn Indicator>>,
        candle_rx: broadcast::Receiver<Candle>,
        threshold: f64,
    ) -> (Self, broadcast::Sender<SignalDetail>, broadcast::Receiver<SignalDetail>) {
        let (signal_tx, signal_rx) = broadcast::channel(256);
        (
            Self { indicators, candle_rx, signal_tx: signal_tx.clone(), threshold },
            signal_tx,
            signal_rx,
        )
    }

    /// 非同期タスクとして動かす。
    /// candle_rx からCandleを受け取り、各インジケータを更新し、集計シグナルを送信する。
    pub async fn run(mut self) {
        loop {
            match self.candle_rx.recv().await {
                Ok(candle) => {
                    let indicator_signals: Vec<IndicatorSignal> = self.indicators
                        .iter_mut()
                        .map(|ind| {
                            let signal = ind.update(&candle);
                            let value = ind.value();
                            IndicatorSignal { name: ind.name().to_string(), signal, value }
                        })
                        .collect();
                    let raw: Vec<Option<Signal>> = indicator_signals.iter()
                        .map(|is| is.signal.clone())
                        .collect();
                    let aggregated = Self::aggregate(&raw, self.threshold);
                    debug!(signal = ?aggregated, "SignalEngine aggregated");
                    let detail = SignalDetail {
                        aggregate: aggregated,
                        indicators: indicator_signals,
                    };
                    let _ = self.signal_tx.send(detail);
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("SignalEngine lagged by {} candles", n);
                }
                Err(broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }
    }

    /// 複数インジケータのシグナルを合成して目標BTC配分率を返す（純粋関数）。
    /// - None (ウォームアップ中) は集計から除外
    /// - Buy は target_pct を 1.0 方向へ、Sell は 0.0 方向へ押す
    /// - 中立（インジケータなし）は 0.5
    pub fn aggregate(signals: &[Option<Signal>], _threshold: f64) -> AllocationSignal {
        let mut delta_sum = 0.0f64;
        let mut active = 0usize;
        let mut directional = 0usize;

        for sig in signals.iter().flatten() {
            active += 1;
            match sig {
                Signal::Buy { confidence, .. } => {
                    delta_sum += confidence;
                    directional += 1;
                }
                Signal::Sell { confidence, .. } => {
                    delta_sum -= confidence;
                    directional += 1;
                }
                Signal::Hold => {}
            }
        }

        if active == 0 {
            return AllocationSignal::neutral();
        }

        // Buy(1.0) 1本のみ → 0.5 + 1.0/(1*2) = 1.0
        // Sell(1.0) 1本のみ → 0.5 - 1.0/(1*2) = 0.0
        let target_pct = (0.5 + delta_sum / (active as f64 * 2.0)).clamp(0.0, 1.0);
        let confidence = directional as f64 / active as f64;
        AllocationSignal { target_pct, confidence }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn buy(conf: f64) -> Option<Signal> {
        Some(Signal::Buy { price: dec!(9000000), confidence: conf })
    }

    fn sell(conf: f64) -> Option<Signal> {
        Some(Signal::Sell { price: dec!(9000000), confidence: conf })
    }

    #[test]
    fn empty_signals_returns_neutral() {
        let result = SignalEngine::aggregate(&[], 0.3);
        assert_eq!(result, AllocationSignal::neutral());
    }

    #[test]
    fn all_none_returns_neutral() {
        let result = SignalEngine::aggregate(&[None, None, None], 0.3);
        assert_eq!(result, AllocationSignal::neutral());
    }

    #[test]
    fn buy_dominates() {
        let signals = vec![buy(0.8), sell(0.3), Some(Signal::Hold)];
        let result = SignalEngine::aggregate(&signals, 0.3);
        assert!(result.target_pct > 0.5, "got {}", result.target_pct);
    }

    #[test]
    fn sell_dominates() {
        let signals = vec![buy(0.2), sell(0.9)];
        let result = SignalEngine::aggregate(&signals, 0.3);
        assert!(result.target_pct < 0.5, "got {}", result.target_pct);
    }

    #[test]
    fn weak_buy_stays_near_neutral() {
        // delta = 0.1, active=3 → 0.5 + 0.1/6 ≈ 0.517
        let signals = vec![buy(0.1), Some(Signal::Hold), Some(Signal::Hold)];
        let result = SignalEngine::aggregate(&signals, 0.3);
        assert!(result.target_pct > 0.5 && result.target_pct < 0.6,
            "got {}", result.target_pct);
    }

    #[test]
    fn tied_returns_neutral() {
        let signals = vec![buy(0.5), sell(0.5)];
        let result = SignalEngine::aggregate(&signals, 0.3);
        assert!((result.target_pct - 0.5).abs() < 1e-9, "got {}", result.target_pct);
    }

    #[test]
    fn single_strong_buy() {
        let signals = vec![buy(1.0)];
        let result = SignalEngine::aggregate(&signals, 0.3);
        assert!((result.target_pct - 1.0).abs() < 1e-9, "got {}", result.target_pct);
    }

    #[test]
    fn single_strong_sell() {
        let signals = vec![sell(1.0)];
        let result = SignalEngine::aggregate(&signals, 0.3);
        assert!((result.target_pct - 0.0).abs() < 1e-9, "got {}", result.target_pct);
    }

    #[tokio::test]
    async fn engine_broadcasts_aggregated_signal() {
        use crate::signal::mock::MockIndicator;
        use chrono::Utc;
        use rust_decimal_macros::dec;
        use crate::types::market::Candle;

        let candle = Candle {
            product_code: "BTC_JPY".into(),
            open_time: Utc::now(),
            resolution_secs: 60,
            open: dec!(9000000), high: dec!(9001000),
            low: dec!(8999000), close: dec!(9000500),
            volume: dec!(1),
        };

        let (candle_tx, candle_rx) = broadcast::channel::<Candle>(16);
        let mock = MockIndicator::new("test", vec![
            Some(Signal::Buy { price: dec!(9000000), confidence: 1.0 }),
        ]);
        let indicators: Vec<Box<dyn Indicator>> = vec![Box::new(mock)];

        let (engine, _signal_tx, mut signal_rx) = SignalEngine::new(indicators, candle_rx, 0.3);
        tokio::spawn(engine.run());

        candle_tx.send(candle).unwrap();

        let sig = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            async { signal_rx.recv().await.unwrap() }
        ).await.expect("timeout");

        assert!(sig.aggregate.target_pct > 0.5, "expected bullish allocation");
        assert_eq!(sig.indicators.len(), 1);
        assert_eq!(sig.indicators[0].name, "test");
    }
}
