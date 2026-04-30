pub mod manager;

pub use manager::RiskManager;

use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::types::order::OrderRequest;

// ---- RiskParams ----

#[derive(Debug, Clone)]
pub struct RiskParams {
    /// 最大保有 BTC 量
    pub max_position_btc: Decimal,
    /// 日次最大損失率 (0.05 = 5%)
    pub max_daily_drawdown: f64,
    /// ストップロス率 (0.02 = 2%)
    pub stop_loss_pct: f64,
    /// 最小注文サイズ
    pub min_order_size: Decimal,
}

impl Default for RiskParams {
    fn default() -> Self {
        Self {
            max_position_btc: dec!(0.1),
            max_daily_drawdown: 0.05,
            stop_loss_pct: 0.02,
            min_order_size: dec!(0.001),
        }
    }
}

// ---- RiskDecision ----

#[derive(Debug)]
pub enum RiskDecision {
    Allow(OrderRequest),
    Reject(String),
    /// サーキットブレーカー発動（日次損失上限超過）
    CircuitBreaker { drawdown_pct: f64 },
}
