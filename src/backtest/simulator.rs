use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

use crate::error::{Error, Result};
use crate::exchange::ExchangeClient;
use crate::risk::{RiskManager, RiskParams};
use crate::signal::{Indicator, Signal};
use crate::storage::db::Database;
use crate::types::{
    balance::{Balance, Position},
    market::{Candle, Ticker},
    order::{Order, OrderRequest, OrderSide, OrderType},
};

use super::{BacktestConfig, BacktestReport};

// ──────────────────────────────────────────────────────────────────────────────
// SimulatedExchange
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct FilledTrade {
    pub side: OrderSide,
    pub price: Decimal,
    pub size: Decimal,
    pub fee: Decimal,
}

struct SimState {
    jpy: Decimal,
    btc: Decimal,
    current_price: Decimal,
    slippage_pct: f64,
    fee_pct: f64,
    filled_trades: Vec<FilledTrade>,
}

/// バックテスト用の仮想取引所。
/// `set_price()` で現在の Candle の終値を設定してから注文を受け付ける。
pub struct SimulatedExchange {
    state: Arc<Mutex<SimState>>,
}

impl SimulatedExchange {
    pub fn new(initial_jpy: Decimal, slippage_pct: f64, fee_pct: f64) -> Self {
        Self {
            state: Arc::new(Mutex::new(SimState {
                jpy: initial_jpy,
                btc: Decimal::ZERO,
                current_price: Decimal::ZERO,
                slippage_pct,
                fee_pct,
                filled_trades: Vec::new(),
            })),
        }
    }

    /// Candle の終値を現在価格として設定する
    pub fn set_price(&self, price: Decimal) {
        self.state.lock().unwrap().current_price = price;
    }

    /// 約定済みトレード一覧を返す
    pub fn filled_trades(&self) -> Vec<FilledTrade> {
        self.state.lock().unwrap().filled_trades.clone()
    }

    /// 現在の JPY 残高
    pub fn jpy_balance(&self) -> Decimal {
        self.state.lock().unwrap().jpy
    }

    /// 現在の BTC 残高
    pub fn btc_balance(&self) -> Decimal {
        self.state.lock().unwrap().btc
    }
}

#[async_trait]
impl ExchangeClient for SimulatedExchange {
    async fn get_ticker(&self, product_code: &str) -> Result<Ticker> {
        let state = self.state.lock().unwrap();
        Ok(Ticker {
            product_code: product_code.to_string(),
            timestamp: Utc::now(),
            best_bid: state.current_price,
            best_ask: state.current_price,
            best_bid_size: Decimal::ONE,
            best_ask_size: Decimal::ONE,
            ltp: state.current_price,
            volume: Decimal::ONE,
            volume_by_product: Decimal::ONE,
        })
    }

    async fn get_balance(&self) -> Result<Vec<Balance>> {
        let state = self.state.lock().unwrap();
        Ok(vec![
            Balance {
                currency_code: "JPY".into(),
                amount: state.jpy,
                available: state.jpy,
            },
            Balance {
                currency_code: "BTC".into(),
                amount: state.btc,
                available: state.btc,
            },
        ])
    }

    async fn get_positions(&self, _product_code: &str) -> Result<Vec<Position>> {
        Ok(vec![])
    }

    async fn send_order(&self, req: &OrderRequest) -> Result<String> {
        let mut state = self.state.lock().unwrap();

        if state.current_price == Decimal::ZERO {
            return Err(Error::Other(anyhow::anyhow!("current price not set")));
        }

        // スリッページを適用した約定価格
        let slippage = Decimal::try_from(state.slippage_pct).unwrap_or(Decimal::ZERO);
        let exec_price = match req.side {
            OrderSide::Buy => state.current_price * (Decimal::ONE + slippage),
            OrderSide::Sell => state.current_price * (Decimal::ONE - slippage),
        };

        let cost = exec_price * req.size;
        let fee_rate = Decimal::try_from(state.fee_pct).unwrap_or(Decimal::ZERO);
        let fee = cost * fee_rate;

        match req.side {
            OrderSide::Buy => {
                let total_cost = cost + fee;
                if state.jpy < total_cost {
                    return Err(Error::Other(anyhow::anyhow!("insufficient JPY balance")));
                }
                state.jpy -= total_cost;
                state.btc += req.size;
            }
            OrderSide::Sell => {
                if state.btc < req.size {
                    return Err(Error::Other(anyhow::anyhow!("insufficient BTC balance")));
                }
                state.btc -= req.size;
                state.jpy += cost - fee;
            }
        }

        state.filled_trades.push(FilledTrade {
            side: req.side.clone(),
            price: exec_price,
            size: req.size,
            fee,
        });

        Ok(format!("sim-{}", state.filled_trades.len()))
    }

    async fn cancel_all_orders(&self, _product_code: &str) -> Result<()> {
        Ok(())
    }

    async fn get_orders(
        &self,
        _product_code: &str,
        _status: Option<&str>,
        _count: Option<u32>,
    ) -> Result<Vec<Order>> {
        Ok(vec![])
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Simulator
// ──────────────────────────────────────────────────────────────────────────────

pub struct Simulator {
    config: BacktestConfig,
    db: Database,
}

impl Simulator {
    pub fn new(config: BacktestConfig, db: Database) -> Self {
        Self { config, db }
    }

    /// Candle 列を受け取りバックテストを実行して `BacktestReport` を返す
    pub async fn run(
        &self,
        candles: Vec<Candle>,
        indicators: Vec<Box<dyn Indicator>>,
        risk_params: RiskParams,
    ) -> Result<BacktestReport> {
        use std::sync::Arc;

        let exchange = Arc::new(SimulatedExchange::new(
            self.config.initial_jpy,
            self.config.slippage_pct,
            self.config.fee_pct,
        ));

        let mut risk_manager = RiskManager::new(risk_params);
        let mut indicators = indicators;

        // 初期残高で日次リセット
        risk_manager.reset_daily(self.config.initial_jpy);

        // equity curve（各 Candle 終了時の JPY 換算総資産）
        let mut equity_curve: Vec<f64> = Vec::with_capacity(candles.len());

        for candle in &candles {
            exchange.set_price(candle.close);

            // 各インジケータから Signal を収集
            let signals: Vec<Option<Signal>> =
                indicators.iter_mut().map(|ind| ind.update(candle)).collect();

            let aggregated = crate::signal::engine::SignalEngine::aggregate(&signals);

            match aggregated {
                Signal::Hold => {}
                signal @ (Signal::Buy { .. } | Signal::Sell { .. }) => {
                    let balances = exchange.get_balance().await?;
                    let jpy = balances
                        .iter()
                        .find(|b| b.currency_code == "JPY")
                        .map(|b| b.available)
                        .unwrap_or(Decimal::ZERO);
                    let btc = exchange.btc_balance();

                    let order_req = signal_to_order(&signal, jpy, btc, &self.config.product_code, risk_manager.params().min_order_size);

                    if let Some(req) = order_req {
                        match risk_manager.evaluate(req, btc, jpy) {
                            crate::risk::RiskDecision::Allow(r) => {
                                let _ = exchange.send_order(&r).await;
                            }
                            crate::risk::RiskDecision::Reject(_) => {}
                            crate::risk::RiskDecision::CircuitBreaker { .. } => {}
                        }
                    }
                }
            }

            // equity = JPY + BTC * 現在価格
            let btc_val = exchange.btc_balance()
                * candle.close;
            let equity = exchange.jpy_balance() + btc_val;
            equity_curve.push(equity.to_f64().unwrap_or(0.0));
        }

        let filled = exchange.filled_trades();
        let report = compute_report(
            &filled,
            &equity_curve,
            self.config.initial_jpy.to_f64().unwrap_or(1.0),
        );

        // DB に保存
        self.db
            .backtest_runs()
            .insert(&self.config, &report)
            .await?;

        Ok(report)
    }
}

fn signal_to_order(
    signal: &Signal,
    jpy: Decimal,
    btc: Decimal,
    product_code: &str,
    min_size: Decimal,
) -> Option<OrderRequest> {
    match signal {
        Signal::Buy { .. } => {
            let _ = jpy;
            Some(OrderRequest {
                product_code: product_code.to_string(),
                side: OrderSide::Buy,
                order_type: OrderType::Market,
                price: None,
                size: min_size,
                minute_to_expire: None,
                time_in_force: None,
            })
        }
        Signal::Sell { .. } => {
            if btc < min_size {
                return None;
            }
            Some(OrderRequest {
                product_code: product_code.to_string(),
                side: OrderSide::Sell,
                order_type: OrderType::Market,
                price: None,
                size: min_size,
                minute_to_expire: None,
                time_in_force: None,
            })
        }
        Signal::Hold => None,
    }
}

fn compute_report(trades: &[FilledTrade], equity_curve: &[f64], initial_jpy: f64) -> BacktestReport {
    let total_trades = trades.len() as u32;

    let final_equity = equity_curve.last().copied().unwrap_or(initial_jpy);
    let total_return_pct = (final_equity - initial_jpy) / initial_jpy * 100.0;

    let max_drawdown_pct = calculate_max_drawdown(equity_curve);
    let sharpe_ratio = calculate_sharpe(equity_curve);
    let win_rate = calculate_win_rate(trades);

    BacktestReport {
        total_return_pct,
        sharpe_ratio,
        max_drawdown_pct,
        win_rate,
        total_trades,
    }
}

fn calculate_max_drawdown(equity: &[f64]) -> f64 {
    if equity.is_empty() {
        return 0.0;
    }
    let mut peak = equity[0];
    let mut max_dd = 0.0f64;
    for &e in equity {
        if e > peak {
            peak = e;
        }
        let dd = (peak - e) / peak * 100.0;
        if dd > max_dd {
            max_dd = dd;
        }
    }
    max_dd
}

fn calculate_sharpe(equity: &[f64]) -> f64 {
    if equity.len() < 2 {
        return 0.0;
    }
    let returns: Vec<f64> = equity
        .windows(2)
        .map(|w| (w[1] - w[0]) / w[0])
        .collect();
    let n = returns.len() as f64;
    let mean = returns.iter().sum::<f64>() / n;
    let variance = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / n;
    let std_dev = variance.sqrt();
    if std_dev == 0.0 {
        return 0.0;
    }
    // 年率化: 60秒足 → 1年 = 525_600 本
    let annualize = (525_600.0f64).sqrt();
    mean / std_dev * annualize
}

fn calculate_win_rate(trades: &[FilledTrade]) -> f64 {
    // Buy と対応する Sell のペアで損益を計算する
    // 簡易実装: Buy 後に Sell が来た場合に (sell_price - buy_price) > 0 なら勝ち
    let mut buy_prices: Vec<Decimal> = Vec::new();
    let mut wins = 0u32;
    let mut total_closed = 0u32;

    for trade in trades {
        match trade.side {
            OrderSide::Buy => buy_prices.push(trade.price),
            OrderSide::Sell => {
                if let Some(buy_price) = buy_prices.pop() {
                    total_closed += 1;
                    if trade.price > buy_price {
                        wins += 1;
                    }
                }
            }
        }
    }

    if total_closed == 0 {
        return 0.0;
    }
    wins as f64 / total_closed as f64
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use rust_decimal_macros::dec;

    fn make_candle(close: Decimal) -> Candle {
        Candle {
            product_code: "BTC_JPY".into(),
            open_time: Utc::now(),
            resolution_secs: 60,
            open: close,
            high: close,
            low: close,
            close,
            volume: dec!(1),
        }
    }

    // ── SimulatedExchange ──

    #[tokio::test]
    async fn buy_order_deducts_jpy_and_adds_btc() {
        let exchange = SimulatedExchange::new(dec!(1_000_000), 0.0, 0.0);
        exchange.set_price(dec!(9_000_000));

        let req = OrderRequest {
            product_code: "BTC_JPY".into(),
            side: OrderSide::Buy,
            order_type: OrderType::Market,
            price: None,
            size: dec!(0.001),
            minute_to_expire: None,
            time_in_force: None,
        };
        exchange.send_order(&req).await.unwrap();

        // 9_000_000 * 0.001 = 9_000 JPY
        assert_eq!(exchange.jpy_balance(), dec!(991_000));
        assert_eq!(exchange.btc_balance(), dec!(0.001));
    }

    #[tokio::test]
    async fn sell_order_adds_jpy_and_deducts_btc() {
        let exchange = SimulatedExchange::new(dec!(1_000_000), 0.0, 0.0);
        exchange.set_price(dec!(9_000_000));

        // 先に BTC を買う
        let buy = OrderRequest {
            product_code: "BTC_JPY".into(),
            side: OrderSide::Buy,
            order_type: OrderType::Market,
            price: None,
            size: dec!(0.001),
            minute_to_expire: None,
            time_in_force: None,
        };
        exchange.send_order(&buy).await.unwrap();

        // 売る
        let sell = OrderRequest {
            product_code: "BTC_JPY".into(),
            side: OrderSide::Sell,
            order_type: OrderType::Market,
            price: None,
            size: dec!(0.001),
            minute_to_expire: None,
            time_in_force: None,
        };
        exchange.send_order(&sell).await.unwrap();

        // 買値と同価格で売ったので JPY は元に戻る（手数料 0 のため）
        assert_eq!(exchange.jpy_balance(), dec!(1_000_000));
        assert_eq!(exchange.btc_balance(), dec!(0));
    }

    #[tokio::test]
    async fn fee_reduces_return() {
        let exchange = SimulatedExchange::new(dec!(1_000_000), 0.0, 0.001); // 0.1% fee
        exchange.set_price(dec!(9_000_000));

        let buy = OrderRequest {
            product_code: "BTC_JPY".into(),
            side: OrderSide::Buy,
            order_type: OrderType::Market,
            price: None,
            size: dec!(0.001),
            minute_to_expire: None,
            time_in_force: None,
        };
        exchange.send_order(&buy).await.unwrap();

        // cost = 9000, fee = 9, total deducted = 9009
        assert_eq!(exchange.jpy_balance(), dec!(1_000_000) - dec!(9_009));
    }

    #[tokio::test]
    async fn order_fails_without_price_set() {
        let exchange = SimulatedExchange::new(dec!(1_000_000), 0.0, 0.0);
        let req = OrderRequest {
            product_code: "BTC_JPY".into(),
            side: OrderSide::Buy,
            order_type: OrderType::Market,
            price: None,
            size: dec!(0.001),
            minute_to_expire: None,
            time_in_force: None,
        };
        let result = exchange.send_order(&req).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn insufficient_jpy_returns_error() {
        let exchange = SimulatedExchange::new(dec!(100), 0.0, 0.0); // 100 JPY だけ
        exchange.set_price(dec!(9_000_000));

        let req = OrderRequest {
            product_code: "BTC_JPY".into(),
            side: OrderSide::Buy,
            order_type: OrderType::Market,
            price: None,
            size: dec!(0.001), // 9000 JPY 必要
            minute_to_expire: None,
            time_in_force: None,
        };
        let result = exchange.send_order(&req).await;
        assert!(result.is_err());
    }

    // ── calculate_* helpers ──

    #[test]
    fn max_drawdown_from_peak() {
        let equity = vec![100.0, 110.0, 90.0, 95.0]; // peak=110 → 90 = 18.18%
        let dd = calculate_max_drawdown(&equity);
        assert!((dd - 18.18).abs() < 0.1, "got {dd}");
    }

    #[test]
    fn max_drawdown_no_loss() {
        let equity = vec![100.0, 110.0, 120.0];
        assert_eq!(calculate_max_drawdown(&equity), 0.0);
    }

    #[test]
    fn sharpe_zero_for_flat_equity() {
        let equity = vec![100.0; 10];
        assert_eq!(calculate_sharpe(&equity), 0.0);
    }

    #[test]
    fn win_rate_correct() {
        let trades = vec![
            FilledTrade {
                side: OrderSide::Buy,
                price: dec!(9_000_000),
                size: dec!(0.001),
                fee: dec!(0),
            },
            FilledTrade {
                side: OrderSide::Sell,
                price: dec!(9_100_000), // 勝ち
                size: dec!(0.001),
                fee: dec!(0),
            },
            FilledTrade {
                side: OrderSide::Buy,
                price: dec!(9_100_000),
                size: dec!(0.001),
                fee: dec!(0),
            },
            FilledTrade {
                side: OrderSide::Sell,
                price: dec!(9_000_000), // 負け
                size: dec!(0.001),
                fee: dec!(0),
            },
        ];
        assert_eq!(calculate_win_rate(&trades), 0.5);
    }

    // ── Simulator integration ──

    #[tokio::test]
    async fn simulator_runs_and_stores_report() {
        use crate::signal::Signal;
        use crate::signal::mock::MockIndicator;

        let db = crate::storage::db::Database::open_in_memory().await.unwrap();
        let from = Utc::now();
        let to = Utc::now();
        let config = BacktestConfig {
            product_code: "BTC_JPY".into(),
            from,
            to,
            resolution_secs: 60,
            slippage_pct: 0.0,
            fee_pct: 0.0,
            initial_jpy: dec!(1_000_000),
        };

        // 5本のCandle: Buy→Hold→Sell→Hold→Hold
        let prices = [dec!(9_000_000), dec!(9_100_000), dec!(9_200_000), dec!(9_100_000), dec!(9_050_000)];
        let candles: Vec<Candle> = prices.iter().map(|&p| make_candle(p)).collect();

        let signals = vec![
            Some(Signal::Buy { price: dec!(9_000_000), confidence: 0.8 }),
            None,
            Some(Signal::Sell { price: dec!(9_200_000), confidence: 0.8 }),
            None,
            None,
        ];
        let indicator = Box::new(MockIndicator::new("mock", signals));

        let simulator = Simulator::new(config, db.clone());
        let report = simulator
            .run(candles, vec![indicator], RiskParams::default())
            .await
            .unwrap();

        // レポートが DB に保存されているはず
        let runs = db.backtest_runs().list(10).await.unwrap();
        assert_eq!(runs.len(), 1);

        // 取引が発生している
        assert!(report.total_trades > 0);
    }
}
