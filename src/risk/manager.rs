use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

use crate::types::order::{OrderRequest, OrderSide};
use super::{RiskDecision, RiskParams};

pub struct RiskManager {
    params: RiskParams,
    circuit_broken: bool,
    /// 当日始値 JPY 残高
    daily_start_jpy: Decimal,
}

impl RiskManager {
    pub fn new(params: RiskParams) -> Self {
        Self {
            params,
            circuit_broken: false,
            daily_start_jpy: Decimal::ZERO,
        }
    }

    /// 現在の JPY 残高で日次ベースラインを設定する（起動時 / 00:00 UTC に呼ぶ）
    pub fn reset_daily(&mut self, current_jpy: Decimal) {
        self.circuit_broken = false;
        self.daily_start_jpy = current_jpy;
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
    /// - `current_btc`: 現在の BTC 保有量（正 = ロング）
    /// - `current_jpy`: 現在の JPY 残高（日次損失率の計算に使用）
    pub fn evaluate(
        &mut self,
        order_req: OrderRequest,
        current_btc: Decimal,
        current_jpy: Decimal,
    ) -> RiskDecision {
        // 1. サーキットブレーカー
        if self.circuit_broken {
            return RiskDecision::Reject("circuit breaker active".into());
        }

        // 2. 注文サイズが最小以上か
        if order_req.size < self.params.min_order_size {
            return RiskDecision::Reject(format!(
                "order size {} is below minimum {}",
                order_req.size, self.params.min_order_size
            ));
        }

        // 3. Buy 注文で最大ポジション超過
        if order_req.side == OrderSide::Buy {
            if current_btc + order_req.size > self.params.max_position_btc {
                return RiskDecision::Reject(format!(
                    "buy would exceed max position: {} + {} > {}",
                    current_btc, order_req.size, self.params.max_position_btc
                ));
            }
        }

        // 4. 日次損失率チェック（ベースラインが設定されている場合のみ）
        if self.daily_start_jpy > Decimal::ZERO {
            let drawdown = self.daily_drawdown_pct(current_jpy);
            if drawdown > self.params.max_daily_drawdown {
                self.circuit_broken = true;
                return RiskDecision::CircuitBreaker {
                    drawdown_pct: drawdown,
                };
            }
        }

        RiskDecision::Allow(order_req)
    }

    fn daily_drawdown_pct(&self, current_jpy: Decimal) -> f64 {
        if self.daily_start_jpy == Decimal::ZERO {
            return 0.0;
        }
        let diff = self.daily_start_jpy - current_jpy;
        let ratio = diff / self.daily_start_jpy;
        ratio.to_f64().unwrap_or(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn make_buy(size: Decimal) -> OrderRequest {
        OrderRequest {
            product_code: "BTC_JPY".into(),
            side: OrderSide::Buy,
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
            side: OrderSide::Sell,
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
        let result = rm.evaluate(make_buy(dec!(0.001)), dec!(0), dec!(1_000_000));
        assert!(matches!(result, RiskDecision::Allow(_)));
    }

    #[test]
    fn rejects_when_circuit_broken() {
        let mut rm = default_manager();
        rm.circuit_broken = true;
        let result = rm.evaluate(make_buy(dec!(0.001)), dec!(0), dec!(1_000_000));
        assert!(matches!(result, RiskDecision::Reject(_)));
    }

    #[test]
    fn rejects_order_below_min_size() {
        let mut rm = default_manager();
        let result = rm.evaluate(make_buy(dec!(0.0009)), dec!(0), dec!(1_000_000));
        assert!(matches!(result, RiskDecision::Reject(_)));
    }

    #[test]
    fn rejects_buy_exceeding_max_position() {
        let mut rm = default_manager();
        // current 0.095 + order 0.01 = 0.105 > 0.1
        let result = rm.evaluate(make_buy(dec!(0.01)), dec!(0.095), dec!(1_000_000));
        assert!(matches!(result, RiskDecision::Reject(_)));
    }

    #[test]
    fn sell_does_not_check_max_position() {
        let mut rm = default_manager();
        let result = rm.evaluate(make_sell(dec!(0.001)), dec!(0), dec!(1_000_000));
        assert!(matches!(result, RiskDecision::Allow(_)));
    }

    #[test]
    fn triggers_circuit_breaker_on_drawdown() {
        let mut rm = default_manager();
        // ベースライン 1,000,000 JPY → 現在 940,000 JPY = 6% 損失 > 5%
        rm.reset_daily(dec!(1_000_000));
        let result = rm.evaluate(make_buy(dec!(0.001)), dec!(0), dec!(940_000));
        assert!(matches!(result, RiskDecision::CircuitBreaker { .. }));
        assert!(rm.is_circuit_broken());
    }

    #[test]
    fn reset_daily_clears_circuit_breaker() {
        let mut rm = default_manager();
        rm.circuit_broken = true;
        rm.reset_daily(dec!(1_000_000));
        assert!(!rm.is_circuit_broken());
    }

    #[test]
    fn drawdown_within_limit_is_allowed() {
        let mut rm = default_manager();
        // ベースライン 1,000,000 → 現在 960,000 = 4% 損失 < 5%
        rm.reset_daily(dec!(1_000_000));
        let result = rm.evaluate(make_buy(dec!(0.001)), dec!(0), dec!(960_000));
        assert!(matches!(result, RiskDecision::Allow(_)));
    }
}
