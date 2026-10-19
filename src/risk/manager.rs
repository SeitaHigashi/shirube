use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;

use crate::types::order::OrderRequest;
use super::{RiskDecision, RiskParams};

/// Stateful guard that validates orders before they reach the exchange.
///
/// Validates minimum order size, tracks daily portfolio drawdown, and
/// checks the circuit-breaker flag. All validation logic in `evaluate` is
/// intentionally pure with respect to async I/O — it only mutates internal
/// state and returns a decision.
pub struct RiskManager {
    params: RiskParams,
    /// `true` after a circuit-breaker event; cleared on the next day change
    /// observed via `observe_time` (or explicitly via `reset_daily`)
    circuit_broken: bool,
    /// Total portfolio value (JPY + BTC value) recorded at the start of the
    /// current trading day; the baseline for daily drawdown calculations.
    daily_start_value: Decimal,
    /// Date on which `daily_start_value` was last set, expressed in the
    /// timestamp of whatever data is being processed (candle/indicator
    /// time) rather than wall-clock. Driving the day boundary off data time
    /// instead of `Utc::now()` lets the same code reset once per simulated
    /// day in a backtest and once per real day in live trading.
    last_reset_date: Option<NaiveDate>,
}

impl RiskManager {
    pub fn new(params: RiskParams) -> Self {
        Self {
            params,
            circuit_broken: false,
            daily_start_value: Decimal::ZERO,
            last_reset_date: None,
        }
    }

    /// 明示的にサーキットブレーカーフラグをリセットする（プロセス起動時など、
    /// まだ `observe_time` を呼ぶ前に使う）。
    pub fn reset_daily(&mut self) {
        self.circuit_broken = false;
    }

    /// 処理中データの時刻を元に「日」の変化を検知し、変化していれば
    /// 日次ドローダウンのベースラインを再設定してサーキットブレーカーを解除する。
    ///
    /// NOTE: 呼び出し側は必ずこの時刻を `Utc::now()`（ライブ取引）または
    /// キャンドルの `open_time`（バックテスト）から渡すこと。壁時計に依存すると
    /// バックテストでは実行中に「日」が進まないため、一度発動したブレーカーが
    /// シミュレーション全体に渡って解除されなくなる。
    pub fn observe_time(&mut self, time: DateTime<Utc>, current_total_value: Decimal) {
        let date = time.date_naive();
        if self.last_reset_date != Some(date) {
            self.last_reset_date = Some(date);
            self.daily_start_value = current_total_value;
            self.circuit_broken = false;
        }
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

    /// 現在の総資産（JPY + BTC評価額）を日次ベースラインと比較し、
    /// 損失率が `max_daily_drawdown` を超えていればサーキットブレーカーを発動する。
    ///
    /// 注文の有無に関わらず毎サイクル呼ぶこと。シグナルが変化せず注文が
    /// 発生しないサイクルでも、価格下落によるドローダウンは検知できる必要がある。
    ///
    ///   drawdown = (daily_start_value - current_total_value) / daily_start_value
    pub fn check_drawdown(&mut self, current_total_value: Decimal) -> Option<RiskDecision> {
        if !self.params.circuit_breaker_enabled || self.circuit_broken {
            return None;
        }
        if self.daily_start_value <= Decimal::ZERO {
            return None;
        }
        let diff = self.daily_start_value - current_total_value;
        let drawdown_pct = (diff / self.daily_start_value).to_f64().unwrap_or(0.0);
        if drawdown_pct > self.params.max_daily_drawdown {
            self.circuit_broken = true;
            return Some(RiskDecision::CircuitBreaker { drawdown_pct });
        }
        None
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

    fn breaker_enabled_params() -> RiskParams {
        RiskParams {
            min_order_size: dec!(0.001),
            circuit_breaker_enabled: true,
            max_daily_drawdown: 0.05,
        }
    }

    fn day(y: i32, m: u32, d: u32) -> DateTime<Utc> {
        use chrono::TimeZone;
        Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap()
    }

    #[test]
    fn rejects_when_circuit_broken() {
        let mut rm = RiskManager::new(breaker_enabled_params());
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
        let mut rm = RiskManager::new(breaker_enabled_params());
        rm.circuit_broken = true;
        rm.reset_daily();
        assert!(!rm.is_circuit_broken());
    }

    #[test]
    fn check_drawdown_trips_breaker_past_threshold() {
        // 5% max drawdown; portfolio drops from 1_000_000 to 900_000 (10% loss)
        let mut rm = RiskManager::new(breaker_enabled_params());
        rm.observe_time(day(2026, 1, 1), dec!(1_000_000));
        let decision = rm.check_drawdown(dec!(900_000));
        assert!(matches!(decision, Some(RiskDecision::CircuitBreaker { .. })));
        assert!(rm.is_circuit_broken());
    }

    #[test]
    fn check_drawdown_does_not_trip_within_threshold() {
        let mut rm = RiskManager::new(breaker_enabled_params());
        rm.observe_time(day(2026, 1, 1), dec!(1_000_000));
        // 2% loss, below the 5% threshold
        let decision = rm.check_drawdown(dec!(980_000));
        assert!(decision.is_none());
        assert!(!rm.is_circuit_broken());
    }

    #[test]
    fn check_drawdown_disabled_never_trips() {
        let mut rm = default_manager(); // circuit_breaker_enabled: false
        rm.observe_time(day(2026, 1, 1), dec!(1_000_000));
        let decision = rm.check_drawdown(dec!(1));
        assert!(decision.is_none());
        assert!(!rm.is_circuit_broken());
    }

    #[test]
    fn observe_time_resets_baseline_and_breaker_on_new_day() {
        let mut rm = RiskManager::new(breaker_enabled_params());
        rm.observe_time(day(2026, 1, 1), dec!(1_000_000));
        rm.check_drawdown(dec!(900_000)); // trips breaker
        assert!(rm.is_circuit_broken());

        // Same day again: breaker stays tripped, baseline unchanged.
        rm.observe_time(day(2026, 1, 1), dec!(900_000));
        assert!(rm.is_circuit_broken());

        // A new day (by data timestamp, not wall-clock) clears the breaker
        // and re-baselines against the current value.
        rm.observe_time(day(2026, 1, 2), dec!(900_000));
        assert!(!rm.is_circuit_broken());
        // Freshly re-baselined at 900_000; a further 4% drop should not trip.
        let decision = rm.check_drawdown(dec!(870_000));
        assert!(decision.is_none());
    }

    #[test]
    fn observe_time_drives_reset_from_data_time_not_wall_clock() {
        // Regression test: a multi-day backtest processed within a single
        // real-time second must still reset once per *simulated* day.
        let mut rm = RiskManager::new(breaker_enabled_params());
        rm.observe_time(day(2026, 1, 1), dec!(1_000_000));
        rm.check_drawdown(dec!(940_000)); // 6% drop → trips
        assert!(rm.is_circuit_broken());

        // Even though wall-clock time barely advanced, the next candle's
        // *data* timestamp is a new simulated day, so the breaker clears.
        rm.observe_time(day(2026, 1, 2), dec!(940_000));
        assert!(!rm.is_circuit_broken());
    }
}
