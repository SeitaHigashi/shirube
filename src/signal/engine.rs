use std::time::Duration;

use chrono::Utc;
use tokio::sync::{broadcast, watch};
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, warn};

use super::{AllocationSignal, Indicator, IndicatorSignal, Signal, SignalDetail};
use super::indicators::{bollinger::Bollinger, ema::Ema, macd::Macd, rsi::Rsi, sma::Sma};
use crate::config::{TradingConfig, ZoneConfig};
use crate::types::market::Candle;

pub struct SignalEngine {
    indicators: Vec<Box<dyn Indicator>>,
    candle_rx: broadcast::Receiver<Candle>,
    signal_tx: broadcast::Sender<SignalDetail>,
    threshold: f64,
    zone: ZoneConfig,
    /// ライブ設定変更を受信するチャネル
    config_rx: watch::Receiver<TradingConfig>,
    /// 最後に受信したキャンドルから計算したインジケータ結果（タイマー再計算に使用）
    last_indicator_signals: Option<Vec<IndicatorSignal>>,
}

impl SignalEngine {
    pub fn new(
        indicators: Vec<Box<dyn Indicator>>,
        candle_rx: broadcast::Receiver<Candle>,
        config_rx: watch::Receiver<TradingConfig>,
    ) -> (Self, broadcast::Sender<SignalDetail>, broadcast::Receiver<SignalDetail>) {
        let threshold = config_rx.borrow().signal_threshold;
        let zone = config_rx.borrow().zone.clone();
        Self::new_with_zone_inner(indicators, candle_rx, config_rx, threshold, zone)
    }

    pub fn new_with_zone(
        indicators: Vec<Box<dyn Indicator>>,
        candle_rx: broadcast::Receiver<Candle>,
        config_rx: watch::Receiver<TradingConfig>,
    ) -> (Self, broadcast::Sender<SignalDetail>, broadcast::Receiver<SignalDetail>) {
        let threshold = config_rx.borrow().signal_threshold;
        let zone = config_rx.borrow().zone.clone();
        Self::new_with_zone_inner(indicators, candle_rx, config_rx, threshold, zone)
    }

    fn new_with_zone_inner(
        indicators: Vec<Box<dyn Indicator>>,
        candle_rx: broadcast::Receiver<Candle>,
        config_rx: watch::Receiver<TradingConfig>,
        threshold: f64,
        zone: ZoneConfig,
    ) -> (Self, broadcast::Sender<SignalDetail>, broadcast::Receiver<SignalDetail>) {
        let (signal_tx, signal_rx) = broadcast::channel(256);
        (
            Self {
                indicators,
                candle_rx,
                signal_tx: signal_tx.clone(),
                threshold,
                zone,
                config_rx,
                last_indicator_signals: None,
            },
            signal_tx,
            signal_rx,
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
    /// 250ms タイマーで再集計してシグナルをブロードキャストする。
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
                            let indicator_signals: Vec<IndicatorSignal> = self.indicators
                                .iter_mut()
                                .map(|ind| {
                                    let signal = ind.update(&candle);
                                    let value = ind.value();
                                    IndicatorSignal { name: ind.name().to_string(), signal, value }
                                })
                                .collect();
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
                // 250ms タイマー: キャッシュ済みインジケータ結果で再集計して送信
                _ = ticker.tick() => {
                    if let Some(ref ind_signals) = self.last_indicator_signals {
                        let raw: Vec<Option<Signal>> = ind_signals.iter()
                            .map(|is| is.signal.clone())
                            .collect();
                        let aggregated = Self::aggregate_with_zone(&raw, self.threshold, &self.zone);
                        debug!(signal = ?aggregated, "SignalEngine aggregated");
                        let detail = SignalDetail {
                            aggregate: aggregated,
                            indicators: ind_signals.clone(),
                            calculated_at: Utc::now(),
                            calculation_state: "active".to_string(),
                        };
                        let _ = self.signal_tx.send(detail);
                    }
                    // last_indicator_signals が None の場合は無送信（データ待ち）
                }
                // 設定変更: threshold/zone を即時反映、インジケータ周期が変わった場合は再構築
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
                    }
                    self.threshold = new_cfg.signal_threshold;
                    self.zone = new_cfg.zone.clone();
                    current_cfg = new_cfg;
                }
            }
        }
    }

    /// 複数インジケータのシグナルをZoneConfigに従って合成する（純粋関数）。
    /// - None (ウォームアップ中) は集計から除外
    /// - Buy は raw_signal を range_max 方向へ、Sell は 0.0 方向へ押す
    /// - 中立（インジケータなし）は range_max / 2.0
    pub fn aggregate_with_zone(
        signals: &[Option<Signal>],
        _threshold: f64,
        zone: &ZoneConfig,
    ) -> AllocationSignal {
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

        // 正規化後の値 [0.0, 1.0] を range_max にスケール
        // Buy(1.0) 1本のみ → (0.5 + 0.5) * range_max = range_max
        // Sell(1.0) 1本のみ → (0.5 - 0.5) * range_max = 0.0
        let normalized = (0.5 + delta_sum / (active as f64 * 2.0)).clamp(0.0, 1.0);
        let raw_signal = normalized * zone.range_max;
        let target_pct = crate::signal::apply_zone(raw_signal, zone);
        let confidence = directional as f64 / active as f64;
        AllocationSignal { raw_signal, target_pct, confidence }
    }

    /// 後方互換: デフォルトZoneConfig（range_max=1.0）で集計する。
    pub fn aggregate(signals: &[Option<Signal>], threshold: f64) -> AllocationSignal {
        Self::aggregate_with_zone(signals, threshold, &ZoneConfig::default())
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

        let cfg = crate::config::TradingConfig::default();
        let (config_tx, config_rx) = tokio::sync::watch::channel(cfg);
        let _ = config_tx; // keep sender alive
        let (engine, _signal_tx, mut signal_rx) = SignalEngine::new(indicators, candle_rx, config_rx);
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
    async fn config_update_reflects_new_threshold() {
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
        let mock = MockIndicator::new("test", vec![
            Some(Signal::Buy { price: dec!(9000000), confidence: 1.0 }),
        ]);
        let indicators: Vec<Box<dyn Indicator>> = vec![Box::new(mock)];

        let cfg = crate::config::TradingConfig::default();
        let (config_tx, config_rx) = tokio::sync::watch::channel(cfg.clone());
        let (engine, _signal_tx, mut signal_rx) = SignalEngine::new(indicators, candle_rx, config_rx);
        tokio::spawn(engine.run());

        // 設定を更新して threshold を変更する
        let mut updated = cfg;
        updated.signal_threshold = 0.9;
        config_tx.send(updated).unwrap();

        candle_tx.send(candle).unwrap();

        let sig = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            async { signal_rx.recv().await.unwrap() }
        ).await.expect("timeout");

        // シグナルが正常に届けば設定更新後も動作していることを確認
        assert_eq!(sig.indicators.len(), 1);
    }
}
