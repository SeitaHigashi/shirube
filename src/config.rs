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
    /// これ未満のraw_signalは effective=0.0（全JPY）。デフォルト 0.2
    pub hold_jpy_below: f64,
    /// これ超のraw_signalは effective=1.0（全BTC）。デフォルト 0.8
    pub hold_btc_above: f64,
}

impl Default for ZoneConfig {
    fn default() -> Self {
        Self { range_max: 1.0, hold_jpy_below: 0.2, hold_btc_above: 0.8 }
    }
}

/// 取引設定。インジケータ周期・シグナル重み・ゾーン設定・リバランス間隔を保持する。
/// リスク管理パラメータ（ポジション上限・ドローダウン）は RiskParams で別管理。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradingConfig {
    /// 配分変更の最小しきい値。|delta| がこれ未満なら注文しない（取引所最小取引量の目安）。
    /// デフォルト 0.15 (15%)
    pub allocation_threshold: f64,

    /// サーキットブレーカーを有効化するか。デフォルト true。
    /// 既存DBに保存された古い設定JSONにはこのフィールドが存在しないため、
    /// 起動時のリスク保護が欠落しないよう `serde(default)` で true にフォールバックする。
    #[serde(default = "default_circuit_breaker_enabled")]
    pub circuit_breaker_enabled: bool,
    /// サーキットブレーカーが発動する日次ドローダウン率。デフォルト 0.05 (5%)
    #[serde(default = "default_max_daily_drawdown")]
    pub max_daily_drawdown: f64,

    // インジケータ設定
    pub sma_period: usize,
    pub ema_period: usize,
    pub rsi_period: usize,
    pub macd_fast: usize,
    pub macd_slow: usize,
    pub macd_signal: usize,
    pub bollinger_period: usize,
    pub bollinger_std: f64,

    // シグナル重み（ta_weight + sentiment_weight = 1.0）
    pub ta_weight: f64,
    pub sentiment_weight: f64,

    /// ゾーン制配分の設定。デフォルトは現行互換（[0.0, 1.0]）
    #[serde(default)]
    pub zone: ZoneConfig,

    /// 定期リバランスチェックの間隔（秒）。
    /// 価格変動による配分ドリフトを検出するため、シグナル受信とは独立して定期的に
    /// 現在配分 vs 目標配分を確認する。デフォルト: 60秒。
    #[serde(default = "default_rebalance_interval")]
    pub rebalance_interval_secs: u64,
}

fn default_rebalance_interval() -> u64 { 60 }
fn default_circuit_breaker_enabled() -> bool { true }
fn default_max_daily_drawdown() -> f64 { 0.05 }

impl Default for TradingConfig {
    fn default() -> Self {
        Self {
            allocation_threshold: 0.15,
            circuit_breaker_enabled: true,
            max_daily_drawdown: 0.05,
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
            rebalance_interval_secs: 60,
        }
    }
}

impl TradingConfig {
    /// RiskParams へ変換する。最小注文サイズは固定値（0.001 BTC）を使用する。
    pub fn to_risk_params(&self) -> RiskParams {
        RiskParams {
            min_order_size: dec!(0.001),
            circuit_breaker_enabled: self.circuit_breaker_enabled,
            max_daily_drawdown: self.max_daily_drawdown,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if !(0.0..=1.0).contains(&self.allocation_threshold) {
            return Err("allocation_threshold は 0.0〜1.0 の範囲で指定してください".into());
        }
        if !(0.0..=1.0).contains(&self.max_daily_drawdown) {
            return Err("max_daily_drawdown は 0.0〜1.0 の範囲で指定してください".into());
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
        assert_eq!(cfg.allocation_threshold, 0.15);
        assert_eq!(cfg.sma_period, 200);
        assert_eq!(cfg.ta_weight, 0.7);
    }

    #[test]
    fn serde_roundtrip() {
        let cfg = TradingConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: TradingConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.allocation_threshold, cfg.allocation_threshold);
        assert_eq!(back.sma_period, cfg.sma_period);
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
    fn to_risk_params_sets_min_order_size() {
        let cfg = TradingConfig::default();
        let params = cfg.to_risk_params();
        assert_eq!(params.min_order_size, dec!(0.001));
    }

    #[test]
    fn to_risk_params_carries_circuit_breaker_settings() {
        let mut cfg = TradingConfig::default();
        cfg.circuit_breaker_enabled = true;
        cfg.max_daily_drawdown = 0.1;
        let params = cfg.to_risk_params();
        assert!(params.circuit_breaker_enabled);
        assert_eq!(params.max_daily_drawdown, 0.1);
    }

    #[test]
    fn validate_rejects_bad_max_daily_drawdown() {
        let mut cfg = TradingConfig::default();
        cfg.max_daily_drawdown = 1.5;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn missing_fields_in_stored_json_fall_back_to_safe_defaults() {
        // Simulates a config JSON persisted before circuit_breaker_enabled /
        // max_daily_drawdown existed. Must still deserialize, and must default
        // to the breaker being ON rather than silently disabled.
        let cfg = TradingConfig::default();
        let mut json: serde_json::Value = serde_json::to_value(&cfg).unwrap();
        json.as_object_mut().unwrap().remove("circuit_breaker_enabled");
        json.as_object_mut().unwrap().remove("max_daily_drawdown");
        let loaded: TradingConfig = serde_json::from_value(json).unwrap();
        assert!(loaded.circuit_breaker_enabled);
        assert_eq!(loaded.max_daily_drawdown, 0.05);
    }
}
