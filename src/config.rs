use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

use crate::risk::RiskParams;

/// シグナル値をBTC配分率に変換するゾーン設定。
/// Zone A: raw < hold_jpy_below  → 0.0（全JPY）
/// Zone B: hold_jpy_below ≤ raw ≤ hold_btc_above → 線形補間（0.0〜1.0 BTC）
/// Zone C: raw > hold_btc_above  → 1.0（全BTC）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZoneConfig {
    /// 生シグナルの最大値。デフォルト 1.0
    pub range_max: f64,
    /// これ未満のraw_signalは effective=0.0（全JPY）。デフォルト 0.0
    pub hold_jpy_below: f64,
    /// これ超のraw_signalは effective=1.0（全BTC）。デフォルト 1.0
    pub hold_btc_above: f64,
}

impl Default for ZoneConfig {
    fn default() -> Self {
        Self { range_max: 1.0, hold_jpy_below: 0.2, hold_btc_above: 0.8 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradingConfig {
    // リスク管理
    pub max_position_btc: f64,
    pub max_daily_drawdown: f64,
    pub stop_loss_pct: f64,
    pub min_order_size: f64,

    // シグナル閾値
    pub signal_threshold: f64,
    /// 配分変更の dead-band。|delta| がこれ未満なら注文しない。デフォルト 0.05 (5%)
    pub allocation_threshold: f64,

    // インジケータ設定
    pub sma_period: usize,
    pub ema_period: usize,
    pub rsi_period: usize,
    pub macd_fast: usize,
    pub macd_slow: usize,
    pub macd_signal: usize,
    pub bollinger_period: usize,
    pub bollinger_std: f64,

    // シグナル重み
    pub ta_weight: f64,
    pub sentiment_weight: f64,

    /// ゾーン制配分の設定。デフォルトは現行互換（[0.0, 1.0]）
    #[serde(default)]
    pub zone: ZoneConfig,
}

impl Default for TradingConfig {
    fn default() -> Self {
        Self {
            max_position_btc: 0.1,
            max_daily_drawdown: 0.05,
            stop_loss_pct: 0.02,
            min_order_size: 0.001,
            signal_threshold: 0.4,
            allocation_threshold: 0.05,
            sma_period: 200,
            ema_period: 100,
            rsi_period: 42,
            macd_fast: 52,
            macd_slow: 104,
            macd_signal: 18,
            bollinger_period: 60,
            bollinger_std: 2.0,
            ta_weight: 0.7,
            sentiment_weight: 0.3,
            zone: ZoneConfig::default(),
        }
    }
}

impl TradingConfig {
    pub fn to_risk_params(&self) -> RiskParams {
        RiskParams {
            max_position_btc: Decimal::try_from(self.max_position_btc).unwrap_or(dec!(0.1)),
            max_daily_drawdown: self.max_daily_drawdown,
            stop_loss_pct: self.stop_loss_pct,
            min_order_size: Decimal::try_from(self.min_order_size).unwrap_or(dec!(0.001)),
            circuit_breaker_enabled: false,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if !(0.0..=1.0).contains(&self.signal_threshold) {
            return Err("signal_threshold は 0.0〜1.0 の範囲で指定してください".into());
        }
        if !(0.0..=1.0).contains(&self.allocation_threshold) {
            return Err("allocation_threshold は 0.0〜1.0 の範囲で指定してください".into());
        }
        if !(0.0..=1.0).contains(&self.max_daily_drawdown) {
            return Err("max_daily_drawdown は 0.0〜1.0 の範囲で指定してください".into());
        }
        if !(0.0..=1.0).contains(&self.stop_loss_pct) {
            return Err("stop_loss_pct は 0.0〜1.0 の範囲で指定してください".into());
        }
        if self.max_position_btc <= 0.0 {
            return Err("max_position_btc は 0 より大きい値を指定してください".into());
        }
        if self.min_order_size <= 0.0 {
            return Err("min_order_size は 0 より大きい値を指定してください".into());
        }
        if self.macd_slow <= self.macd_fast {
            return Err("macd_slow は macd_fast より大きい値を指定してください".into());
        }
        if self.sma_period < 2 || self.ema_period < 2 || self.rsi_period < 2
            || self.macd_fast < 2 || self.bollinger_period < 2
        {
            return Err("各インジケータのperiodは 2 以上を指定してください".into());
        }
        let weight_sum = self.ta_weight + self.sentiment_weight;
        if (weight_sum - 1.0).abs() > 0.01 {
            return Err("ta_weight + sentiment_weight の合計は 1.0 にしてください".into());
        }
        if self.zone.range_max <= 0.0 {
            return Err("zone.range_max は 0 より大きい値を指定してください".into());
        }
        if !(0.0..=self.zone.range_max).contains(&self.zone.hold_jpy_below) {
            return Err("zone.hold_jpy_below は [0, range_max] の範囲で指定してください".into());
        }
        if !(0.0..=self.zone.range_max).contains(&self.zone.hold_btc_above) {
            return Err("zone.hold_btc_above は [0, range_max] の範囲で指定してください".into());
        }
        if self.zone.hold_jpy_below >= self.zone.hold_btc_above {
            return Err("zone.hold_jpy_below < zone.hold_btc_above を満たす必要があります".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_values_are_correct() {
        let cfg = TradingConfig::default();
        assert_eq!(cfg.signal_threshold, 0.4);
        assert_eq!(cfg.sma_period, 200);
        assert_eq!(cfg.ta_weight, 0.7);
    }

    #[test]
    fn serde_roundtrip() {
        let cfg = TradingConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: TradingConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.signal_threshold, cfg.signal_threshold);
        assert_eq!(back.sma_period, cfg.sma_period);
    }

    #[test]
    fn validate_rejects_invalid_threshold() {
        let mut cfg = TradingConfig::default();
        cfg.signal_threshold = 1.5;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_bad_macd() {
        let mut cfg = TradingConfig::default();
        cfg.macd_fast = 30;
        cfg.macd_slow = 20;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_bad_weights() {
        let mut cfg = TradingConfig::default();
        cfg.ta_weight = 0.5;
        cfg.sentiment_weight = 0.3;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn to_risk_params_converts_correctly() {
        let cfg = TradingConfig::default();
        let params = cfg.to_risk_params();
        assert_eq!(params.max_daily_drawdown, 0.05);
        assert_eq!(params.stop_loss_pct, 0.02);
    }
}
