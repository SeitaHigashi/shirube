use tokio::sync::broadcast;
use tracing::{debug, warn};

use super::{Indicator, IndicatorSignal, Signal, SignalDetail};
use crate::types::market::Candle;

pub struct SignalEngine {
    indicators: Vec<Box<dyn Indicator>>,
    candle_rx: broadcast::Receiver<Candle>,
    signal_tx: broadcast::Sender<SignalDetail>,
}

impl SignalEngine {
    pub fn new(
        indicators: Vec<Box<dyn Indicator>>,
        candle_rx: broadcast::Receiver<Candle>,
    ) -> (Self, broadcast::Sender<SignalDetail>, broadcast::Receiver<SignalDetail>) {
        let (signal_tx, signal_rx) = broadcast::channel(256);
        (
            Self { indicators, candle_rx, signal_tx: signal_tx.clone() },
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
                        .map(|ind| IndicatorSignal {
                            name: ind.name().to_string(),
                            signal: ind.update(&candle),
                        })
                        .collect();
                    let raw: Vec<Option<Signal>> = indicator_signals.iter()
                        .map(|is| is.signal.clone())
                        .collect();
                    let aggregated = Self::aggregate(&raw, 0.3);
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

    /// 複数インジケータのシグナルを合成する（純粋関数）。
    /// - None (ウォームアップ中) は集計から除外
    /// - Buy/Sell の confidence を合算し、閾値を超えた方を採用
    pub fn aggregate(signals: &[Option<Signal>], threshold: f64) -> Signal {
        let mut buy_score = 0.0f64;
        let mut sell_score = 0.0f64;
        let mut active = 0usize;

        for sig in signals.iter().flatten() {
            match sig {
                Signal::Buy { confidence, .. } => {
                    buy_score += confidence;
                    active += 1;
                }
                Signal::Sell { confidence, .. } => {
                    sell_score += confidence;
                    active += 1;
                }
                Signal::Hold => {
                    active += 1;
                }
            }
        }

        if active == 0 {
            return Signal::Hold;
        }

        // 代表価格は None のため Hold の price は不要。Buy/Sell は後続で再取得される。
        // ここでは price を 0 にしておき、TradingEngine 側で現在価格で上書きする。
        use rust_decimal::Decimal;
        // Hold カウントは threshold 計算から除外し、Buy/Sell のみで判定
        let scored = signals.iter().flatten().filter(|s| !matches!(s, Signal::Hold)).count();
        let denom = scored.max(1) as f64;
        if buy_score > sell_score && buy_score / denom >= threshold {
            Signal::Buy { price: Decimal::ZERO, confidence: buy_score / denom }
        } else if sell_score > buy_score && sell_score / denom >= threshold {
            Signal::Sell { price: Decimal::ZERO, confidence: sell_score / denom }
        } else {
            Signal::Hold
        }
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
    fn empty_signals_returns_hold() {
        assert_eq!(SignalEngine::aggregate(&[], 0.3), Signal::Hold);
    }

    #[test]
    fn all_none_returns_hold() {
        assert_eq!(SignalEngine::aggregate(&[None, None, None], 0.3), Signal::Hold);
    }

    #[test]
    fn buy_dominates() {
        let signals = vec![buy(0.8), sell(0.3), Some(Signal::Hold)];
        let result = SignalEngine::aggregate(&signals, 0.3);
        assert!(matches!(result, Signal::Buy { .. }));
    }

    #[test]
    fn sell_dominates() {
        let signals = vec![buy(0.2), sell(0.9)];
        let result = SignalEngine::aggregate(&signals, 0.3);
        assert!(matches!(result, Signal::Sell { .. }));
    }

    #[test]
    fn below_threshold_returns_hold() {
        // buy_score=0.1/3=0.033 < 0.3
        let signals = vec![buy(0.1), Some(Signal::Hold), Some(Signal::Hold)];
        let result = SignalEngine::aggregate(&signals, 0.3);
        assert_eq!(result, Signal::Hold);
    }

    #[test]
    fn tied_returns_hold() {
        let signals = vec![buy(0.5), sell(0.5)];
        let result = SignalEngine::aggregate(&signals, 0.3);
        assert_eq!(result, Signal::Hold);
    }

    #[test]
    fn single_strong_buy() {
        let signals = vec![buy(0.9)];
        let result = SignalEngine::aggregate(&signals, 0.3);
        assert!(matches!(result, Signal::Buy { .. }));
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
            Some(Signal::Buy { price: dec!(9000000), confidence: 0.8 }),
        ]);
        let indicators: Vec<Box<dyn Indicator>> = vec![Box::new(mock)];

        let (engine, _signal_tx, mut signal_rx) = SignalEngine::new(indicators, candle_rx);
        tokio::spawn(engine.run());

        candle_tx.send(candle).unwrap();

        let sig = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            async { signal_rx.recv().await.unwrap() }
        ).await.expect("timeout");

        assert!(matches!(sig.aggregate, Signal::Buy { .. }));
        assert_eq!(sig.indicators.len(), 1);
        assert_eq!(sig.indicators[0].name, "test");
    }
}
