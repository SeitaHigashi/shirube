use crate::types::order::OrderRequest;
use super::{RiskDecision, RiskParams};

/// Stateful guard that validates orders before they reach the exchange.
///
/// Validates minimum order size and checks the circuit-breaker flag.
/// All validation logic in `evaluate` is intentionally pure with respect to
/// async I/O — it only mutates internal state and returns a decision.
pub struct RiskManager {
    params: RiskParams,
    /// `true` after a circuit-breaker event; cleared by `reset_daily`
    circuit_broken: bool,
}

impl RiskManager {
    pub fn new(params: RiskParams) -> Self {
        Self { params, circuit_broken: false }
    }

    /// サーキットブレーカーフラグをリセットする（UTCミッドナイト呼び出し用）。
    pub fn reset_daily(&mut self) {
        self.circuit_broken = false;
    }

    pub fn is_circuit_broken(&self) -> bool {
        self.circuit_broken
    }

    pub fn params(&self) -> &RiskParams {
        &self.params
    }

    pub fn update_params(&mut self, params: RiskParams) {
        self.params = params;
    }

    /// Signal から生成した OrderRequest を検証する。
    ///
    /// - サーキットブレーカー有効時は Reject を返す
    /// - 最小注文サイズ未満は Reject を返す
    /// - それ以外は Allow
    pub fn evaluate(&mut self, order_req: OrderRequest) -> RiskDecision {
        // 1. サーキットブレーカー
        if self.circuit_broken && self.params.circuit_breaker_enabled {
            return RiskDecision::Reject("circuit breaker active".into());
        }

        // 2. 注文サイズが最小以上か（dust order 防止）
        if order_req.size < self.params.min_order_size {
            return RiskDecision::Reject(format!(
                "order size {} is below minimum {}",
                order_req.size, self.params.min_order_size
            ));
        }

        RiskDecision::Allow(order_req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    fn make_buy(size: Decimal) -> OrderRequest {
        OrderRequest {
            product_code: "BTC_JPY".into(),
            side: crate::types::order::OrderSide::Buy,
            order_type: crate::types::order::OrderType::Market,
            price: None,
            size,
            minute_to_expire: None,
            time_in_force: None,
        }
    }

    fn make_sell(size: Decimal) -> OrderRequest {
        OrderRequest {
            product_code: "BTC_JPY".into(),
            side: crate::types::order::OrderSide::Sell,
            order_type: crate::types::order::OrderType::Market,
            price: None,
            size,
            minute_to_expire: None,
            time_in_force: None,
        }
    }

    fn default_manager() -> RiskManager {
        RiskManager::new(RiskParams::default())
    }

    #[test]
    fn allows_valid_buy_order() {
        let mut rm = default_manager();
        let result = rm.evaluate(make_buy(dec!(0.001)));
        assert!(matches!(result, RiskDecision::Allow(_)));
    }

    #[test]
    fn allows_valid_sell_order() {
        let mut rm = default_manager();
        let result = rm.evaluate(make_sell(dec!(0.001)));
        assert!(matches!(result, RiskDecision::Allow(_)));
    }

    #[test]
    fn rejects_when_circuit_broken() {
        let mut rm = RiskManager::new(RiskParams {
            min_order_size: dec!(0.001),
            circuit_breaker_enabled: true,
        });
        rm.circuit_broken = true;
        let result = rm.evaluate(make_buy(dec!(0.001)));
        assert!(matches!(result, RiskDecision::Reject(_)));
    }

    #[test]
    fn rejects_order_below_min_size() {
        let mut rm = default_manager();
        let result = rm.evaluate(make_buy(dec!(0.0009)));
        assert!(matches!(result, RiskDecision::Reject(_)));
    }

    #[test]
    fn reset_daily_clears_circuit_breaker() {
        let mut rm = RiskManager::new(RiskParams {
            min_order_size: dec!(0.001),
            circuit_breaker_enabled: true,
        });
        rm.circuit_broken = true;
        rm.reset_daily();
        assert!(!rm.is_circuit_broken());
    }
}
