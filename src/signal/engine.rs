use std::time::Duration;

use chrono::Utc;
use rust_decimal::prelude::ToPrimitive;
use tokio::sync::{broadcast, watch};
use tokio::time::{interval, MissedTickBehavior};
use tracing::{info, warn};

use super::{Indicator, IndicatorOutput, IndicatorPoint, IndicatorRawValues, IndicatorSignal};
use super::indicators::{bollinger::Bollinger, ema::Ema, macd::Macd, rsi::Rsi, sma::Sma};
use crate::config::TradingConfig;
use crate::types::market::Candle;

/// インジケータ計算エンジン。Candle を受信してインジケータを更新し、
/// 250ms 周期で IndicatorOutput をブロードキャストする。
///
/// # 責務
/// - インジケータの計算値（IndicatorPoint）の生成
/// - 各インジケータの個別値（IndicatorSignal）の収集
/// - 設定変更に応じたインジケータ再構築
///
/// # 非責務（TradingEngine が担う）
/// - 配分率の計算（compute_btc_target）
/// - 発注判断・ガードロジック
pub struct SignalEngine {
    indicators: Vec<Box<dyn Indicator>>,
    candle_rx: broadcast::Receiver<Candle>,
    /// IndicatorOutput をブロードキャストするチャネル（計算値のみ、判断は含まない）
    output_tx: broadcast::Sender<IndicatorOutput>,
    /// ライブ設定変更を受信するチャネル
    config_rx: watch::Receiver<TradingConfig>,
    /// 最後に受信したキャンドルから計算したインジケータ結果（タイマー再送信用）
    last_indicator_signals: Option<Vec<IndicatorSignal>>,
    /// 最後に計算した生インジケータ値（タイマー再送信用）
    last_raw_indicators: Option<IndicatorPoint>,
}

impl SignalEngine {
    pub fn new(
        indicators: Vec<Box<dyn Indicator>>,
        candle_rx: broadcast::Receiver<Candle>,
        config_rx: watch::Receiver<TradingConfig>,
    ) -> (Self, broadcast::Sender<IndicatorOutput>, broadcast::Receiver<IndicatorOutput>) {
        let (output_tx, output_rx) = broadcast::channel(256);
        (
            Self {
                indicators,
                candle_rx,
                output_tx: output_tx.clone(),
                config_rx,
                last_indicator_signals: None,
                last_raw_indicators: None,
            },
            output_tx,
            output_rx,
        )
    }

    /// 設定からインジケータセットを再構築する。
    fn build_indicators(cfg: &TradingConfig) -> Vec<Box<dyn Indicator>> {
        vec![
            Box::new(Sma::new(cfg.sma_period)),
            Box::new(Ema::new(cfg.ema_period)),
            Box::new(Rsi::new(cfg.rsi_period)),
            Box::new(Macd::new(cfg.macd_fast, cfg.macd_slow, cfg.macd_signal)),
            Box::new(Bollinger::new(cfg.bollinger_period, cfg.bollinger_std)),
        ]
    }

    /// インジケータ周期パラメータが変化したかどうかを判定する（純粋関数）。
    fn indicators_changed(a: &TradingConfig, b: &TradingConfig) -> bool {
        a.sma_period != b.sma_period
            || a.ema_period != b.ema_period
            || a.rsi_period != b.rsi_period
            || a.macd_fast != b.macd_fast
            || a.macd_slow != b.macd_slow
            || a.macd_signal != b.macd_signal
            || a.bollinger_period != b.bollinger_period
            || a.bollinger_std != b.bollinger_std
    }

    /// 非同期タスクとして動かす。
    /// キャンドル受信時はインジケータを更新してキャッシュし、
    /// 250ms タイマーで IndicatorOutput をブロードキャストする。
    /// config_rx で設定変更を検知するとインジケータを再構築する。
    pub async fn run(mut self) {
        let mut ticker = interval(Duration::from_millis(250));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // 設定変更比較用に現在の設定を保持する
        let mut current_cfg = self.config_rx.borrow().clone();

        loop {
            tokio::select! {
                // キャンドル受信: インジケータを更新してキャッシュ（送信はしない）
                result = self.candle_rx.recv() => {
                    match result {
                        Ok(candle) => {
                            let mut point = IndicatorPoint {
                                time: candle.open_time,
                                close: candle.close.to_f64(),
                                sma: None, ema: None, rsi: None,
                                macd_line: None, signal_line: None, histogram: None,
                                bb_upper: None, bb_middle: None, bb_lower: None,
                            };
                            let indicator_signals: Vec<IndicatorSignal> = self.indicators
                                .iter_mut()
                                .map(|ind| {
                                    // update() は void: インジケータは Buy/Sell を返さない
                                    ind.update(&candle);
                                    let value = ind.value();
                                    // スナップショットで生値を収集して IndicatorPoint に格納
                                    match ind.snapshot() {
                                        IndicatorRawValues::Scalar(v) => match ind.name() {
                                            "SMA" => point.sma = v,
                                            "EMA" => point.ema = v,
                                            "RSI" => point.rsi = v,
                                            _ => {}
                                        },
                                        IndicatorRawValues::Macd { macd_line, signal_line, histogram } => {
                                            point.macd_line = macd_line;
                                            point.signal_line = signal_line;
                                            point.histogram = histogram;
                                        }
                                        IndicatorRawValues::BollingerBands { upper, middle, lower } => {
                                            point.bb_upper = upper;
                                            point.bb_middle = middle;
                                            point.bb_lower = lower;
                                        }
                                    }
                                    IndicatorSignal { name: ind.name().to_string(), value }
                                })
                                .collect();
                            self.last_raw_indicators = Some(point);
                            self.last_indicator_signals = Some(indicator_signals);
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!("SignalEngine lagged by {} candles", n);
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            break;
                        }
                    }
                }
                // 250ms タイマー: キャッシュ済みインジケータ結果を IndicatorOutput として送信
                // NOTE: 集計・発注判断は行わない。TradingEngine が担う。
                _ = ticker.tick() => {
                    if let (Some(ref ind_signals), Some(ref raw)) =
                        (&self.last_indicator_signals, &self.last_raw_indicators)
                    {
                        let output = IndicatorOutput {
                            indicators: ind_signals.clone(),
                            raw: raw.clone(),
                            calculated_at: Utc::now(),
                        };
                        let _ = self.output_tx.send(output);
                    }
                    // last_indicator_signals が None の場合は無送信（データ待ち）
                }
                // 設定変更: インジケータ周期が変わった場合は再構築
                Ok(()) = self.config_rx.changed() => {
                    let new_cfg = self.config_rx.borrow_and_update().clone();
                    if Self::indicators_changed(&current_cfg, &new_cfg) {
                        info!(
                            sma = new_cfg.sma_period,
                            ema = new_cfg.ema_period,
                            rsi = new_cfg.rsi_period,
                            "Config changed: rebuilding indicators"
                        );
                        self.indicators = Self::build_indicators(&new_cfg);
                        // ウォームアップ完了まで古いキャッシュをクリアする
                        self.last_indicator_signals = None;
                        self.last_raw_indicators = None;
                    }
                    current_cfg = new_cfg;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast;

    #[tokio::test]
    async fn engine_broadcasts_indicator_output() {
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
        // MockIndicator takes Vec<Option<f64>> — no Buy/Sell judgment
        let mock = MockIndicator::new("test", vec![Some(0.7_f64)]);
        let indicators: Vec<Box<dyn Indicator>> = vec![Box::new(mock)];

        let cfg = crate::config::TradingConfig::default();
        let (config_tx, config_rx) = tokio::sync::watch::channel(cfg);
        let _ = config_tx; // keep sender alive
        let (engine, _output_tx, mut output_rx) = SignalEngine::new(indicators, candle_rx, config_rx);
        tokio::spawn(engine.run());

        candle_tx.send(candle).unwrap();

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            async { output_rx.recv().await.unwrap() }
        ).await.expect("timeout");

        // IndicatorOutput にはインジケータ計算値が含まれる（集計は含まない）
        assert_eq!(output.indicators.len(), 1);
        assert_eq!(output.indicators[0].name, "test");
        assert_eq!(output.indicators[0].value, Some(0.7));
    }

    #[test]
    fn indicators_changed_detects_period_diff() {
        let a = crate::config::TradingConfig::default();
        let mut b = a.clone();
        b.sma_period = 50;
        assert!(SignalEngine::indicators_changed(&a, &b));
        assert!(!SignalEngine::indicators_changed(&a, &a));
    }

    #[test]
    fn indicators_changed_detects_bollinger_std_diff() {
        let a = crate::config::TradingConfig::default();
        let mut b = a.clone();
        b.bollinger_std = 3.0;
        assert!(SignalEngine::indicators_changed(&a, &b));
    }

    #[tokio::test]
    async fn config_update_rebuilds_indicators() {
        use crate::signal::mock::MockIndicator;
        use crate::types::market::Candle;
        use chrono::Utc;
        use rust_decimal_macros::dec;

        let candle = Candle {
            product_code: "BTC_JPY".into(),
            open_time: Utc::now(),
            resolution_secs: 60,
            open: dec!(9000000), high: dec!(9001000),
            low: dec!(8999000), close: dec!(9000500),
            volume: dec!(1),
        };

        let (candle_tx, candle_rx) = broadcast::channel::<Candle>(16);
        let mock = MockIndicator::new("test", vec![Some(0.5_f64)]);
        let indicators: Vec<Box<dyn Indicator>> = vec![Box::new(mock)];

        let cfg = crate::config::TradingConfig::default();
        let (config_tx, config_rx) = tokio::sync::watch::channel(cfg.clone());
        let (engine, _output_tx, mut output_rx) = SignalEngine::new(indicators, candle_rx, config_rx);
        tokio::spawn(engine.run());

        // 設定を更新してインジケータ周期を変更する
        let mut updated = cfg;
        updated.sma_period = 50;
        config_tx.send(updated).unwrap();

        candle_tx.send(candle).unwrap();

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            async { output_rx.recv().await.unwrap() }
        ).await.expect("timeout");

        // 設定更新後もブロードキャストが正常に届くことを確認
        assert_eq!(output.indicators.len(), 1);
    }
}
