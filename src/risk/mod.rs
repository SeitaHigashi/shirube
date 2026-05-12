pub mod manager;

pub use manager::RiskManager;

use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::types::order::OrderRequest;

// ---- RiskParams ----

/// リスク管理パラメータ。最小注文サイズとサーキットブレーカー設定のみを保持する。
/// ポジション上限・ドローダウン制限は TradingConfig から分離済み。
#[derive(Debug, Clone)]
pub struct RiskParams {
    /// dust order 防止用の最小注文サイズ (BTC)
    pub min_order_size: Decimal,
    /// false にするとサーキットブレーカーを完全無効化（デバッグ用）
    pub circuit_breaker_enabled: bool,
}

impl Default for RiskParams {
    fn default() -> Self {
        Self {
            min_order_size: dec!(0.001),
            circuit_breaker_enabled: false,
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
