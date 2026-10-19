pub mod manager;

pub use manager::RiskManager;

use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::types::order::OrderRequest;

// ---- RiskParams ----

/// リスク管理パラメータ。最小注文サイズ・サーキットブレーカー設定・
/// 日次ドローダウン上限を保持する。
///
/// NOTE: ポジション上限 (max_position_btc) は zone-based allocation の導入後、
/// 意図的に再実装していない。ゾーン設定 (`ZoneConfig`) が既にポートフォリオ
/// 全体に対する BTC 配分率の上限を担っており、固定 BTC 量の上限は現在の
/// 配分方式と噛み合わないため。
#[derive(Debug, Clone)]
pub struct RiskParams {
    /// dust order 防止用の最小注文サイズ (BTC)
    pub min_order_size: Decimal,
    /// false にするとサーキットブレーカーを完全無効化（デバッグ用）
    pub circuit_breaker_enabled: bool,
    /// サーキットブレーカーが発動する日次ドローダウン率のしきい値 (例: 0.05 = 5%)
    pub max_daily_drawdown: f64,
}

impl Default for RiskParams {
    fn default() -> Self {
        Self {
            min_order_size: dec!(0.001),
            circuit_breaker_enabled: false,
            max_daily_drawdown: 0.05,
        }
    }
}

// ---- RiskDecision ----

#[derive(Debug)]
pub enum RiskDecision {
    Allow(OrderRequest),
    Reject(String),
    /// サーキットブレーカー発動（将来の拡張用）
    CircuitBreaker { drawdown_pct: f64 },
}
