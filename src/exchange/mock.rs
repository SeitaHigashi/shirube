use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use rust_decimal_macros::dec;

use crate::error::{Error, Result};
use crate::exchange::rate_limiter::RateLimiter;
use crate::exchange::ExchangeClient;
use crate::storage::mock_state::MockStateRepository;
use crate::sync_ext::RwLockExt;
use crate::types::{
    balance::{Balance, Position},
    market::{MyExecution, Ticker},
    order::{Order, OrderRequest, OrderSide, OrderStatus, OrderType},
};

// ── 約定履歴 ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct FilledTrade {
    pub side: OrderSide,
    pub price: Decimal,
    pub size: Decimal,
    pub fee: Decimal,
}

/// bitFlyer が手数料ティアの判定に使う取引量の集計窓（直近30日）。
///
/// NOTE: added 2026-09-13. `volume_jpy` はそれまで**生涯累計**で、一度上がった
/// ティアが二度と戻らなかった。bitFlyer の手数料体系は直近30日の取引量基準
/// なので、長く走らせるほど手数料を過小評価する。30日を超えない期間の
/// バックテストでは窓の内外に差が出ないため、現行の14日ホールドアウトでは
/// 結果は完全に一致する（`fee_window_is_identical_within_30_days` で検証）。
const FEE_VOLUME_WINDOW_DAYS: i64 = 30;

// ── bitFlyer Lightning 現物 手数料ティアテーブル ──────────────────────────────
//
// 直近30日取引量（JPY）→ 手数料率
// 公式: https://bitflyer.com/ja-jp/s/commission
const FEE_TIERS: &[(u64, f64)] = &[
    (500_000_000, 0.0001), // 5億円以上
    (100_000_000, 0.0002), // 1〜5億円
    (50_000_000,  0.0003), // 5000万〜1億円
    (20_000_000,  0.0005), // 2000万〜5000万円
    (10_000_000,  0.0007), // 1000万〜2000万円
    (5_000_000,   0.0009), // 500万〜1000万円
    (2_000_000,   0.0010), // 200万〜500万円
    (1_000_000,   0.0011), // 100万〜200万円
    (500_000,     0.0012), // 50万〜100万円
    (200_000,     0.0013), // 20万〜50万円
    (100_000,     0.0014), // 10万〜20万円
    (0,           0.0015), // 10万円未満
];

/// Look up the bitFlyer fee tier rate for a given cumulative traded volume.
///
/// `pub(crate)` so `backtest::report::compute_report` can derive
/// `final_fee_tier_pct` from a run's final traded volume without
/// duplicating this table.
pub(crate) fn fee_from_volume(volume_jpy: Decimal) -> f64 {
    use rust_decimal::prelude::ToPrimitive;
    let vol = volume_jpy.to_u64().unwrap_or(0);
    for &(threshold, rate) in FEE_TIERS {
        if vol >= threshold {
            return rate;
        }
    }
    0.0015
}

// ── 約定モデル ───────────────────────────────────────────────────────────────

/// 成行注文がどう約定するかのモデル。
///
/// NOTE: added 2026-09-13. それまで `send_order` は「板の最良気配で、サイズに
/// 関係なく、常に全量が即座に約定する」という理想化を置いていた。つまり
/// 0.001 BTC の注文も 10 BTC の注文も同じ価格で約定し、板を食い上げる
/// コストが一切かからなかった。売買回転の高い戦略ほどこの理想化は結果を
/// 甘く見せるため、実際の板に近づけられるようにする。
///
/// **デフォルト（`FillModel::ideal()`）は変更前と完全に同一の挙動**で、
/// 有効化は明示的に行う。過去に `experiments/` へ記録された数値を静かに
/// 書き換えないための設計。
#[derive(Debug, Clone, PartialEq)]
pub struct FillModel {
    /// 板を食い上げる価格インパクトの係数。
    ///
    /// `impact_pct = impact_coefficient * (約定代金 / reference_depth_jpy)`
    /// を最良気配に上乗せする（買いは不利側 = 高く、売りは安く）。
    /// 線形インパクトモデル: 注文代金が参照板厚と同額なら
    /// `impact_coefficient` 分だけ不利になる。0.0 でインパクトなし。
    pub impact_coefficient: f64,
    /// 価格インパクトの基準となる板の厚み（JPY）。
    /// 0 以下の場合はインパクト計算を行わない（ゼロ除算回避）。
    pub reference_depth_jpy: Decimal,
    /// 1回の注文で約定できる最大サイズ（BTC）。`None` で無制限（全量約定）。
    ///
    /// これを超える注文は部分約定となり、残りは約定しない。エンジンは次の
    /// 評価で残高を読み直して残差デルタを再計算するため、部分約定は自然に
    /// 再試行される（`OrderStatus::Active` として記録される）。
    pub max_fill_size: Option<Decimal>,
}

impl FillModel {
    /// 変更前と完全に同一の理想化された約定（インパクトなし・全量即時約定）。
    pub fn ideal() -> Self {
        Self {
            impact_coefficient: 0.0,
            reference_depth_jpy: Decimal::ZERO,
            max_fill_size: None,
        }
    }

    /// このモデルが理想約定（＝変更前の挙動）と同一かどうか。
    pub fn is_ideal(&self) -> bool {
        *self == Self::ideal()
    }

    /// 約定代金に対する価格インパクト率を返す。返り値は常に 0 以上。
    ///
    /// 式: `impact_coefficient * (notional_jpy / reference_depth_jpy)`
    /// 範囲: [0, ∞)。係数または参照板厚が未設定なら 0。
    fn impact_pct(&self, notional_jpy: Decimal) -> f64 {
        use rust_decimal::prelude::ToPrimitive;
        if self.impact_coefficient <= 0.0 || self.reference_depth_jpy <= Decimal::ZERO {
            return 0.0;
        }
        let ratio = (notional_jpy / self.reference_depth_jpy).to_f64().unwrap_or(0.0);
        (self.impact_coefficient * ratio).max(0.0)
    }
}

impl Default for FillModel {
    fn default() -> Self {
        Self::ideal()
    }
}

// ── MockExchangeClient ────────────────────────────────────────────────────────

pub struct MockExchangeClient {
    ticker: Arc<RwLock<Ticker>>,
    balances: Arc<RwLock<Vec<Balance>>>,
    orders: Arc<RwLock<Vec<Order>>>,
    filled_trades: Arc<RwLock<Vec<FilledTrade>>>,
    order_counter: Arc<AtomicU64>,
    /// テスト用に手数料率を固定する場合は Some(rate) を指定する。
    /// None の場合は直近30日取引量に基づいてティア制手数料を計算する。
    override_fee: Option<f64>,
    /// 生涯累計のJPY建て取引量。レポートの `traded_volume_jpy` 用であって
    /// 手数料ティアの判定には使わない（ティアは下の `volume_window`）。
    volume_jpy: Arc<RwLock<Decimal>>,
    /// 手数料ティア判定用の (約定時刻, 代金) 履歴。`fee_pct()` が
    /// `FEE_VOLUME_WINDOW_DAYS` より古いエントリを捨ててから合計する。
    volume_window: Arc<RwLock<VecDeque<(DateTime<Utc>, Decimal)>>>,
    /// シミュレーション時刻。`None` のときは実時刻 `Utc::now()` を使う。
    ///
    /// バックテストはローソク足自身の時刻で進むため、30日窓の判定に実時刻を
    /// 使うとバックテスト全体が実時間の一瞬に収まり窓が一度も動かない。
    /// `Simulator::run` が各足の `open_time` をここに設定する。
    clock: Arc<RwLock<Option<DateTime<Utc>>>>,
    /// 成行注文の約定モデル。デフォルトは変更前と同一の理想約定。
    fill_model: FillModel,
    /// レート制限。`None` のとき無制限（バックテスト既定）。
    ///
    /// ペーパートレードでは本番 `BitFlyerRestClient` と同じ予算を共有させる
    /// ことで、モック側に委譲される `get_balance` / `get_positions` /
    /// `send_order` も予算を消費させる。これがないとペーパートレードは
    /// 本番の API 負荷を 1/3 に過小評価する（`PublicBitFlyerClient` 参照）。
    rate_limiter: Option<Arc<RateLimiter>>,
    db: Option<Arc<MockStateRepository>>,
}

impl Default for MockExchangeClient {
    fn default() -> Self {
        Self::new()
    }
}

impl MockExchangeClient {
    /// ティア制手数料（累計取引量ベース）でクライアントを生成する。
    pub fn new() -> Self {
        Self::new_inner(None)
    }

    /// 手数料率を固定値で指定してクライアントを生成する（テスト用）。
    pub fn with_fee(fee_pct: f64) -> Self {
        Self::new_inner(Some(fee_pct))
    }

    fn new_inner(override_fee: Option<f64>) -> Self {
        let now = Utc::now();
        let ticker = Ticker {
            product_code: "BTC_JPY".to_string(),
            timestamp: now,
            best_bid: dec!(9_000_000),
            best_ask: dec!(9_001_000),
            best_bid_size: dec!(0.1),
            best_ask_size: dec!(0.1),
            ltp: dec!(9_000_500),
            volume: dec!(100.0),
            volume_by_product: dec!(100.0),
        };
        let balances = vec![
            Balance {
                currency_code: "JPY".to_string(),
                amount: dec!(1_000_000),
                available: dec!(1_000_000),
            },
            Balance {
                currency_code: "BTC".to_string(),
                amount: dec!(0),
                available: dec!(0),
            },
        ];
        Self {
            ticker: Arc::new(RwLock::new(ticker)),
            balances: Arc::new(RwLock::new(balances)),
            orders: Arc::new(RwLock::new(Vec::new())),
            filled_trades: Arc::new(RwLock::new(Vec::new())),
            order_counter: Arc::new(AtomicU64::new(1)),
            override_fee,
            volume_jpy: Arc::new(RwLock::new(Decimal::ZERO)),
            volume_window: Arc::new(RwLock::new(VecDeque::new())),
            clock: Arc::new(RwLock::new(None)),
            fill_model: FillModel::ideal(),
            rate_limiter: None,
            db: None,
        }
    }

    /// 約定モデルを差し替えたクライアントを返す（ビルダー形式）。
    pub fn with_fill_model(mut self, fill_model: FillModel) -> Self {
        self.fill_model = fill_model;
        self
    }

    /// レート制限予算を共有させる（ビルダー形式）。
    ///
    /// 渡された `RateLimiter` は他のクライアントと共有されうる。bitFlyer の
    /// 制限はアカウント/IP 単位でありクライアントオブジェクト単位ではない
    /// ため、別々のバケットを持たせると実際の2倍の許容量を模擬してしまう。
    pub fn with_rate_limiter(mut self, limiter: Arc<RateLimiter>) -> Self {
        self.rate_limiter = Some(limiter);
        self
    }

    /// 現在の約定モデル。
    pub fn fill_model(&self) -> &FillModel {
        &self.fill_model
    }

    /// シミュレーション時刻を設定する（バックテスト用）。
    ///
    /// 設定すると手数料ティアの30日窓・注文のタイムスタンプがこの時刻を
    /// 基準に進む。設定しない限り実時刻が使われるので、ペーパートレードと
    /// 本番の挙動は変わらない。
    pub fn set_clock(&self, now: DateTime<Utc>) {
        *self.clock.write_or_recover() = Some(now);
    }

    /// 「現在時刻」。`set_clock` されていれば擬似時刻、なければ実時刻。
    fn now(&self) -> DateTime<Utc> {
        self.clock.read_or_recover().unwrap_or_else(Utc::now)
    }

    /// レート制限トークンを1つ消費する。リミッタ未設定なら即座に返る。
    async fn acquire(&self) {
        if let Some(limiter) = &self.rate_limiter {
            limiter.acquire().await;
        }
    }

    /// 手数料ティア判定に使う直近30日の取引量。
    ///
    /// 呼ぶたびに窓の外に出たエントリを捨てる。範囲: [0, ∞) JPY。
    pub fn windowed_volume_jpy(&self) -> Decimal {
        let cutoff = self.now() - ChronoDuration::days(FEE_VOLUME_WINDOW_DAYS);
        let mut window = self.volume_window.write_or_recover();
        // 時系列順に push されるので、先頭から落とせば十分。
        while let Some((ts, _)) = window.front() {
            if *ts < cutoff {
                window.pop_front();
            } else {
                break;
            }
        }
        window.iter().map(|(_, v)| *v).sum()
    }

    /// DBリポジトリを渡してDB永続化を有効にしたクライアントを生成する。
    /// 既存の残高・注文カウンター・約定履歴をDBから復元する。
    /// 手数料はティア制（volume_jpy ベース）を使用する。
    pub async fn new_with_db(repo: MockStateRepository) -> Result<Self> {
        let mut client = Self::new();

        let saved_balances = repo.load_balances().await?;
        if !saved_balances.is_empty() {
            client.set_balances(saved_balances);
        }

        let counter = repo.load_order_counter().await?;
        client.order_counter.store(counter, Ordering::SeqCst);

        let trades = repo.load_filled_trades().await?;
        // 累計取引量を約定履歴から再計算してティア制手数料を正しく復元する。
        //
        // NOTE: `FilledTrade` は約定時刻を保持していないため、復元した履歴を
        // 30日窓のどこに置くべきかが分からない。ティアを不当に軽くしない
        // 安全側に倒し、復元分はすべて「現在時刻」に発生したものとして窓に
        // 入れる（＝窓から出るのは復元から30日後）。約定時刻を
        // `FilledTrade` に持たせれば正確にできるが、DB スキーマ変更を伴う
        // ため別途対応とする。
        let volume: Decimal = trades.iter().map(|t| t.price * t.size).sum();
        *client.volume_jpy.write_or_recover() = volume;
        if volume > Decimal::ZERO {
            client
                .volume_window
                .write_or_recover()
                .push_back((Utc::now(), volume));
        }
        *client.filled_trades.write_or_recover() = trades;

        client.db = Some(Arc::new(repo));
        Ok(client)
    }

    /// ticker の ltp / bid / ask を一括更新する（バックテスト用）。
    pub fn set_price(&self, price: Decimal) {
        let mut t = self.ticker.write_or_recover();
        t.ltp = price;
        t.best_bid = price;
        t.best_ask = price;
        t.timestamp = Utc::now();
    }

    pub fn set_ticker(&self, ticker: Ticker) {
        *self.ticker.write_or_recover() = ticker;
    }

    pub fn set_balances(&self, balances: Vec<Balance>) {
        *self.balances.write_or_recover() = balances;
    }

    pub fn placed_orders(&self) -> Vec<Order> {
        self.orders.read_or_recover().clone()
    }

    pub fn filled_trades(&self) -> Vec<FilledTrade> {
        self.filled_trades.read_or_recover().clone()
    }

    /// Cumulative JPY-denominated traded volume accumulated so far.
    ///
    /// NOTE: this is only meaningfully tracked when the client is using the
    /// tier-based fee schedule (i.e. `override_fee` is `None`) — see
    /// `send_order`, which skips the `volume_jpy` accumulation entirely
    /// when a fixed fee override is active, since a fixed rate never
    /// depends on cumulative volume.
    pub fn cumulative_volume_jpy(&self) -> Decimal {
        *self.volume_jpy.read_or_recover()
    }

    pub fn jpy_balance(&self) -> Decimal {
        self.balances
            .read_or_recover()
            .iter()
            .find(|b| b.currency_code == "JPY")
            .map(|b| b.amount)
            .unwrap_or(Decimal::ZERO)
    }

    pub fn btc_balance(&self) -> Decimal {
        self.balances
            .read_or_recover()
            .iter()
            .find(|b| b.currency_code == "BTC")
            .map(|b| b.amount)
            .unwrap_or(Decimal::ZERO)
    }

    fn update_balance(&self, currency: &str, delta: Decimal) -> Result<()> {
        let mut balances = self.balances.write_or_recover();
        let bal = balances
            .iter_mut()
            .find(|b| b.currency_code == currency)
            .ok_or_else(|| Error::Other(anyhow::anyhow!("currency not found: {currency}")))?;
        let new_amount = bal.amount + delta;
        if new_amount < Decimal::ZERO {
            return Err(Error::Other(anyhow::anyhow!(
                "insufficient {currency} balance"
            )));
        }
        bal.amount = new_amount;
        bal.available = new_amount;
        Ok(())
    }
}

#[async_trait]
impl ExchangeClient for MockExchangeClient {
    async fn get_ticker(&self, product_code: &str) -> Result<Ticker> {
        self.acquire().await;
        let mut ticker = self.ticker.read_or_recover().clone();
        ticker.product_code = product_code.to_string();
        ticker.timestamp = self.now();
        Ok(ticker)
    }

    async fn get_balance(&self) -> Result<Vec<Balance>> {
        self.acquire().await;
        Ok(self.balances.read_or_recover().clone())
    }

    async fn get_positions(&self, _product_code: &str) -> Result<Vec<Position>> {
        self.acquire().await;
        Ok(vec![])
    }

    async fn send_order(&self, req: &OrderRequest) -> Result<String> {
        self.acquire().await;

        // Fill each side against the side of the book it would actually cross:
        // a buy lifts the ask, a sell hits the bid. Filling both sides at `ltp`
        // (as this did until 2026-09-09) makes a buy-then-sell round trip at an
        // unchanged price cost exactly zero in spread terms, so no caller —
        // paper trading or backtest — could model the cost of turnover at all.
        // A degenerate book (that side unset) falls back to `ltp`, which keeps
        // every existing caller that only calls `set_price` behaving as before.
        let top_price = {
            let ticker = self.ticker.read_or_recover();
            let side_price = match req.side {
                OrderSide::Buy => ticker.best_ask,
                OrderSide::Sell => ticker.best_bid,
            };
            if side_price == Decimal::ZERO {
                ticker.ltp
            } else {
                side_price
            }
        };
        if top_price == Decimal::ZERO {
            return Err(Error::Other(anyhow::anyhow!("current price not set")));
        }

        // Partial fill: an order larger than the model's per-order capacity
        // fills only up to it. The caller re-reads balances on its next
        // evaluation and sees the residual delta, so the remainder is retried
        // naturally rather than needing an open-order lifecycle here.
        // `max_fill_size: None` (the default) keeps the whole order filled.
        let fill_size = match self.fill_model.max_fill_size {
            Some(cap) if req.size > cap => cap,
            _ => req.size,
        };
        if fill_size <= Decimal::ZERO {
            return Err(Error::Other(anyhow::anyhow!(
                "fill model capacity is zero; order cannot fill"
            )));
        }
        let partial = fill_size < req.size;

        // Linear price impact: walking the book costs more the larger the
        // order is relative to the reference depth. Applied on the unfavourable
        // side (a buy pays more, a sell receives less) on top of the top-of-book
        // price, so it composes with the spread rather than replacing it.
        // With the default ideal model this is exactly 0 and `exec_price ==
        // top_price`, reproducing the pre-2026-09-13 behaviour bit for bit.
        let notional_at_top = top_price * fill_size;
        let impact_pct = self.fill_model.impact_pct(notional_at_top);
        let exec_price = if impact_pct == 0.0 {
            top_price
        } else {
            let impact = Decimal::from_f64(impact_pct).unwrap_or(Decimal::ZERO);
            match req.side {
                OrderSide::Buy => top_price * (Decimal::ONE + impact),
                OrderSide::Sell => top_price * (Decimal::ONE - impact),
            }
        };

        let cost = exec_price * fill_size;
        let fee_rate = Decimal::try_from(self.fee_pct()).unwrap_or(Decimal::ZERO);
        let fee = cost * fee_rate;

        // 取引量を加算（ティア制手数料の計算に使用）。
        // 生涯累計はレポート用、窓付きは手数料ティア判定用。
        if self.override_fee.is_none() {
            *self.volume_jpy.write_or_recover() += cost;
            self.volume_window.write_or_recover().push_back((self.now(), cost));
        }

        match req.side {
            OrderSide::Buy => {
                let total = cost + fee;
                // JPY の残高チェックと減算
                self.update_balance("JPY", -total)?;
                self.update_balance("BTC", fill_size)?;
            }
            OrderSide::Sell => {
                // BTC の残高チェックと減算
                self.update_balance("BTC", -fill_size)?;
                self.update_balance("JPY", cost - fee)?;
            }
        }

        let trade = FilledTrade {
            side: req.side.clone(),
            price: exec_price,
            size: fill_size,
            fee,
        };
        self.filled_trades.write_or_recover().push(trade.clone());

        let id = self.order_counter.fetch_add(1, Ordering::SeqCst);
        let acceptance_id = format!("MOCK-{:06}", id);
        let now = self.now();
        let order = Order {
            id: Some(id as i64),
            acceptance_id: acceptance_id.clone(),
            product_code: req.product_code.clone(),
            side: req.side.clone(),
            order_type: req.order_type.clone(),
            price: req.price,
            size: req.size,
            // A partially filled order still has an unfilled remainder, so it
            // is not Completed. `size` stays the *requested* size so the gap
            // against the recorded FilledTrade's size is visible.
            status: if partial { OrderStatus::Active } else { OrderStatus::Completed },
            created_at: now,
            updated_at: now,
        };
        self.orders.write_or_recover().push(order);

        // DB が設定されている場合は状態を永続化する（失敗しても注文自体は成功扱い）
        if let Some(db) = &self.db {
            let balances = self.balances.read_or_recover().clone();
            let next_counter = id + 1;
            if let Err(e) = db.save_balances(&balances).await {
                tracing::warn!("MockExchangeClient: failed to save balances to DB: {}", e);
            }
            if let Err(e) = db.save_order_counter(next_counter).await {
                tracing::warn!("MockExchangeClient: failed to save order counter to DB: {}", e);
            }
            if let Err(e) = db.insert_filled_trade(&trade).await {
                tracing::warn!("MockExchangeClient: failed to save filled trade to DB: {}", e);
            }
        }

        Ok(acceptance_id)
    }

    async fn cancel_all_orders(&self, _product_code: &str) -> Result<()> {
        self.acquire().await;
        let mut orders = self.orders.write_or_recover();
        for order in orders.iter_mut() {
            if order.status == OrderStatus::Active {
                order.status = OrderStatus::Canceled;
                order.updated_at = Utc::now();
            }
        }
        Ok(())
    }

    async fn get_orders(
        &self,
        product_code: &str,
        status: Option<&str>,
        count: Option<u32>,
    ) -> Result<Vec<Order>> {
        self.acquire().await;
        let orders = self.orders.read_or_recover();
        let mut result: Vec<Order> = orders
            .iter()
            .filter(|o| o.product_code == product_code)
            .filter(|o| {
                if let Some(s) = status {
                    let order_status = match &o.status {
                        OrderStatus::Active => "ACTIVE",
                        OrderStatus::Completed => "COMPLETED",
                        OrderStatus::Canceled => "CANCELED",
                        OrderStatus::Expired => "EXPIRED",
                        OrderStatus::Rejected => "REJECTED",
                    };
                    order_status == s
                } else {
                    true
                }
            })
            .cloned()
            .collect();
        if let Some(c) = count {
            result.truncate(c as usize);
        }
        Ok(result)
    }

    async fn cancel_order(&self, _product_code: &str, acceptance_id: &str) -> Result<()> {
        self.acquire().await;
        let mut orders = self.orders.write_or_recover();
        let order = orders
            .iter_mut()
            .find(|o| o.acceptance_id == acceptance_id)
            .ok_or_else(|| {
                Error::Other(anyhow::anyhow!("order not found: {acceptance_id}"))
            })?;
        if order.status != OrderStatus::Active {
            return Err(Error::Other(anyhow::anyhow!(
                "order {} is not active (status: {:?})",
                acceptance_id,
                order.status
            )));
        }
        order.status = OrderStatus::Canceled;
        order.updated_at = Utc::now();
        Ok(())
    }

    async fn get_executions(
        &self,
        _product_code: &str,
        count: Option<u32>,
        before: Option<i64>,
        after: Option<i64>,
    ) -> Result<Vec<MyExecution>> {
        self.acquire().await;
        let trades = self.filled_trades.read_or_recover();
        let mut result: Vec<MyExecution> = trades
            .iter()
            .enumerate()
            .map(|(i, t)| MyExecution {
                id: i as i64 + 1,
                exec_date: Utc::now(),
                side: t.side.clone(),
                price: t.price,
                size: t.size,
                commission: t.fee,
            })
            .filter(|e| before.map_or(true, |b| e.id < b))
            .filter(|e| after.map_or(true, |a| e.id > a))
            .collect();
        if let Some(c) = count {
            result.truncate(c as usize);
        }
        Ok(result)
    }

    async fn get_trading_commission(&self, _product_code: &str) -> Result<f64> {
        self.acquire().await;
        Ok(self.fee_pct())
    }

    fn fee_pct(&self) -> f64 {
        if let Some(rate) = self.override_fee {
            return rate;
        }
        // bitFlyer keys its tier on the *trailing 30 days* of volume, not on
        // lifetime volume — see FEE_VOLUME_WINDOW_DAYS. Using the lifetime
        // total (as this did until 2026-09-13) makes a tier ratchet down and
        // never decay, understating fees on any run longer than 30 days.
        fee_from_volume(self.windowed_volume_jpy())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::order::{OrderRequest, OrderSide, OrderType};
    use rust_decimal_macros::dec;

    fn buy_req(size: Decimal) -> OrderRequest {
        OrderRequest {
            product_code: "BTC_JPY".to_string(),
            side: OrderSide::Buy,
            order_type: OrderType::Market,
            price: None,
            size,
            minute_to_expire: None,
            time_in_force: None,
        }
    }

    fn sell_req(size: Decimal) -> OrderRequest {
        OrderRequest {
            product_code: "BTC_JPY".to_string(),
            side: OrderSide::Sell,
            order_type: OrderType::Market,
            price: None,
            size,
            minute_to_expire: None,
            time_in_force: None,
        }
    }

    #[tokio::test]
    async fn mock_get_ticker_returns_default() {
        let client = MockExchangeClient::new();
        let ticker = client.get_ticker("BTC_JPY").await.unwrap();
        assert_eq!(ticker.product_code, "BTC_JPY");
        assert_eq!(ticker.ltp, dec!(9_000_500));
    }

    #[tokio::test]
    async fn buy_order_deducts_jpy_adds_btc() {
        let client = MockExchangeClient::with_fee(0.0); // 手数料なし
        client.set_price(dec!(9_000_000));

        client.send_order(&buy_req(dec!(0.001))).await.unwrap();

        // 9_000_000 * 0.001 = 9_000 JPY
        assert_eq!(client.jpy_balance(), dec!(991_000));
        assert_eq!(client.btc_balance(), dec!(0.001));
    }

    #[tokio::test]
    async fn sell_order_adds_jpy_deducts_btc() {
        let client = MockExchangeClient::with_fee(0.0);
        client.set_price(dec!(9_000_000));

        client.send_order(&buy_req(dec!(0.001))).await.unwrap();
        client.send_order(&sell_req(dec!(0.001))).await.unwrap();

        // 同価格で売り → JPY は元に戻る（手数料0のため）
        assert_eq!(client.jpy_balance(), dec!(1_000_000));
        assert_eq!(client.btc_balance(), dec!(0));
    }

    #[tokio::test]
    async fn fee_reduces_balance() {
        let client = MockExchangeClient::with_fee(0.0015); // 0.15%
        client.set_price(dec!(9_000_000));

        client.send_order(&buy_req(dec!(0.001))).await.unwrap();

        // cost = 9_000, fee = 9_000 * 0.0015 = 13.5 → total = 9_013.5
        let expected_jpy = dec!(1_000_000) - dec!(9_013.5);
        assert_eq!(client.jpy_balance(), expected_jpy);
        assert_eq!(client.btc_balance(), dec!(0.001));

        let trades = client.filled_trades();
        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].fee, dec!(13.5));
    }

    #[tokio::test]
    async fn insufficient_jpy_returns_error() {
        let client = MockExchangeClient::with_fee(0.0);
        client.set_price(dec!(9_000_000));
        // 初期JPY=1_000_000、0.2BTC買おうとすると 1_800_000 JPY 必要
        let result = client.send_order(&buy_req(dec!(0.2))).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn insufficient_btc_returns_error() {
        let client = MockExchangeClient::with_fee(0.0);
        client.set_price(dec!(9_000_000));
        // BTC残高0のまま売ろうとする
        let result = client.send_order(&sell_req(dec!(0.001))).await;
        assert!(result.is_err());
    }

    /// The fee rate must never increase as cumulative volume increases —
    /// higher-volume tiers must charge a strictly lower or equal rate. This
    /// checks relative ordering across sample points spanning several tier
    /// boundaries rather than duplicating FEE_TIERS' rates verbatim, so a
    /// wrong rate in both places couldn't silently pass.
    #[test]
    fn fee_from_volume_is_monotonically_non_increasing() {
        let sample_volumes: &[u64] = &[
            0,
            50_000,
            100_000,
            500_000,
            1_000_000,
            5_000_000,
            10_000_000,
            50_000_000,
            100_000_000,
            500_000_000,
            1_000_000_000,
        ];
        let mut prev_rate = f64::MAX;
        for &vol in sample_volumes {
            let rate = fee_from_volume(Decimal::from(vol));
            assert!(
                rate <= prev_rate,
                "fee rate must not increase with volume: at {vol} JPY rate was {rate}, previous was {prev_rate}"
            );
            prev_rate = rate;
        }
        // The lowest and highest sampled volumes must land in different
        // (non-equal) tiers — otherwise this test would trivially pass even
        // if the table were flattened to a single rate.
        assert!(fee_from_volume(Decimal::from(0u64)) > fee_from_volume(Decimal::from(1_000_000_000u64)));
    }

    #[tokio::test]
    async fn tiered_fee_starts_at_max() {
        // 取引量ゼロ → 最高手数料ティア (0.15%)
        let client = MockExchangeClient::new();
        assert_eq!(client.fee_pct(), 0.0015);
    }

    #[tokio::test]
    async fn tiered_fee_decreases_with_volume() {
        // 取引量が増えるにつれ手数料が下がることを確認
        let client = MockExchangeClient::new();
        // 初期: 0.15% (10万円未満)
        assert_eq!(client.fee_pct(), 0.0015);

        // 初期残高を増やしてから取引
        client.set_balances(vec![
            crate::types::balance::Balance {
                currency_code: "JPY".to_string(),
                amount: dec!(5_000_000),
                available: dec!(5_000_000),
            },
            crate::types::balance::Balance {
                currency_code: "BTC".to_string(),
                amount: dec!(0),
                available: dec!(0),
            },
        ]);
        // 価格を 1,000,000 JPY に設定し、1 BTC 取引 → 累計 1,000,000 JPY
        client.set_price(dec!(1_000_000));
        client
            .send_order(&buy_req(dec!(1)))
            .await
            .unwrap();
        // 累計100万円 → 0.11% ティア
        assert_eq!(client.fee_pct(), 0.0011);
    }

    #[tokio::test]
    async fn cancel_order_cancels_single() {
        let client = MockExchangeClient::new();
        // Active な注文を2件手動挿入
        {
            let mut orders = client.orders.write_or_recover();
            for (i, id) in ["MOCK-A", "MOCK-B"].iter().enumerate() {
                orders.push(Order {
                    id: Some(i as i64 + 1),
                    acceptance_id: id.to_string(),
                    product_code: "BTC_JPY".to_string(),
                    side: OrderSide::Buy,
                    order_type: crate::types::order::OrderType::Limit,
                    price: Some(dec!(9_000_000)),
                    size: dec!(0.001),
                    status: OrderStatus::Active,
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                });
            }
        }
        client.cancel_order("BTC_JPY", "MOCK-A").await.unwrap();
        let orders = client.placed_orders();
        assert_eq!(orders[0].status, OrderStatus::Canceled);
        assert_eq!(orders[1].status, OrderStatus::Active); // MOCK-B はそのまま
    }

    #[tokio::test]
    async fn cancel_order_not_found_returns_error() {
        let client = MockExchangeClient::new();
        let result = client.cancel_order("BTC_JPY", "NONEXISTENT").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn cancel_order_non_active_returns_error() {
        let client = MockExchangeClient::new();
        client.set_price(dec!(9_000_000));
        let id = client.send_order(&buy_req(dec!(0.001))).await.unwrap();
        // send_order は即 Completed → キャンセル不可
        let result = client.cancel_order("BTC_JPY", &id).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn get_executions_returns_filled_trades() {
        let client = MockExchangeClient::with_fee(0.0);
        client.set_price(dec!(9_000_000));
        client.send_order(&buy_req(dec!(0.001))).await.unwrap();
        client.send_order(&buy_req(dec!(0.001))).await.unwrap();

        let execs = client.get_executions("BTC_JPY", None, None, None).await.unwrap();
        assert_eq!(execs.len(), 2);
        assert_eq!(execs[0].price, dec!(9_000_000));
    }

    #[tokio::test]
    async fn get_executions_respects_count() {
        let client = MockExchangeClient::with_fee(0.0);
        client.set_price(dec!(9_000_000));
        client.send_order(&buy_req(dec!(0.001))).await.unwrap();
        client.send_order(&buy_req(dec!(0.001))).await.unwrap();
        client.send_order(&buy_req(dec!(0.001))).await.unwrap();

        let execs = client.get_executions("BTC_JPY", Some(2), None, None).await.unwrap();
        assert_eq!(execs.len(), 2);
    }

    #[tokio::test]
    async fn get_trading_commission_equals_fee_pct() {
        let client = MockExchangeClient::with_fee(0.0005);
        let rate = client.get_trading_commission("BTC_JPY").await.unwrap();
        assert_eq!(rate, 0.0005);
    }

    #[tokio::test]
    async fn override_fee_ignores_volume() {
        // with_fee() で固定すると取引量に関わらず fee_pct が変わらない
        let client = MockExchangeClient::with_fee(0.0005);
        client.set_balances(vec![
            crate::types::balance::Balance {
                currency_code: "JPY".to_string(),
                amount: dec!(20_000_000),
                available: dec!(20_000_000),
            },
            crate::types::balance::Balance {
                currency_code: "BTC".to_string(),
                amount: dec!(0),
                available: dec!(0),
            },
        ]);
        client.set_price(dec!(10_000_000));
        client
            .send_order(&buy_req(dec!(1)))
            .await
            .unwrap();
        assert_eq!(client.fee_pct(), 0.0005);
    }

    #[tokio::test]
    async fn order_marked_as_completed() {
        let client = MockExchangeClient::new();
        client.set_price(dec!(9_000_000));

        let id = client.send_order(&buy_req(dec!(0.001))).await.unwrap();
        assert!(id.starts_with("MOCK-"));

        let orders = client.placed_orders();
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].status, OrderStatus::Completed);
    }

    #[tokio::test]
    async fn mock_send_order_records_it() {
        let client = MockExchangeClient::new();
        client.set_price(dec!(9_000_000));
        let req = OrderRequest {
            product_code: "BTC_JPY".to_string(),
            side: OrderSide::Buy,
            order_type: OrderType::Limit,
            price: Some(dec!(9_000_000)),
            size: dec!(0.001),
            minute_to_expire: None,
            time_in_force: None,
        };
        let id = client.send_order(&req).await.unwrap();
        assert!(id.starts_with("MOCK-"));

        let orders = client.placed_orders();
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].acceptance_id, id);
    }

    #[tokio::test]
    async fn mock_cancel_all_orders_cancels_active() {
        // cancel_all_orders はアクティブな注文をキャンセルするが、
        // send_order は即時Completedになるため、手動でActiveな注文を挿入してテスト
        let client = MockExchangeClient::new();
        {
            let mut orders = client.orders.write_or_recover();
            orders.push(Order {
                id: Some(99),
                acceptance_id: "MOCK-ACTIVE".to_string(),
                product_code: "BTC_JPY".to_string(),
                side: OrderSide::Buy,
                order_type: OrderType::Limit,
                price: Some(dec!(9_000_000)),
                size: dec!(0.001),
                status: OrderStatus::Active,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            });
        }
        client.cancel_all_orders("BTC_JPY").await.unwrap();

        let orders = client.placed_orders();
        assert_eq!(orders[0].status, OrderStatus::Canceled);
    }

    // ── 30日窓の手数料ティア（項目4） ─────────────────────────────────────

    /// Seed a client with plenty of JPY so large test orders can fill.
    fn funded_client() -> MockExchangeClient {
        let client = MockExchangeClient::new();
        client.set_balances(vec![
            Balance {
                currency_code: "JPY".to_string(),
                amount: dec!(500_000_000),
                available: dec!(500_000_000),
            },
            Balance {
                currency_code: "BTC".to_string(),
                amount: dec!(100),
                available: dec!(100),
            },
        ]);
        client
    }

    #[tokio::test]
    async fn fee_window_is_identical_within_30_days() {
        // The current holdout is 14 days, so nothing ever leaves the window:
        // the windowed volume must equal the lifetime volume exactly, which is
        // why this change cannot move any 14-day backtest number.
        let client = funded_client();
        let t0 = chrono::DateTime::parse_from_rfc3339("2026-08-29T19:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        for day in 0..14 {
            client.set_clock(t0 + ChronoDuration::days(day));
            client.send_order(&buy_req(dec!(0.05))).await.unwrap();
        }
        assert_eq!(
            client.windowed_volume_jpy(),
            client.cumulative_volume_jpy(),
            "within 30 days the window must contain every trade"
        );
    }

    #[tokio::test]
    async fn fee_window_drops_volume_older_than_30_days() {
        let client = funded_client();
        let t0 = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        // Day 0: enough volume to reach a cheaper tier.
        client.set_clock(t0);
        client.send_order(&buy_req(dec!(0.3))).await.unwrap(); // ~2.7M JPY
        let tier_with_volume = client.fee_pct();
        assert!(
            tier_with_volume < 0.0015,
            "expected a discounted tier, got {tier_with_volume}"
        );

        // 31 days later that volume is outside bitFlyer's trailing window, so
        // the tier must decay back to the entry rate. The lifetime accumulator
        // this replaced could never do that.
        client.set_clock(t0 + ChronoDuration::days(31));
        assert_eq!(client.windowed_volume_jpy(), Decimal::ZERO);
        assert_eq!(client.fee_pct(), 0.0015);

        // ...while the lifetime figure the report uses is untouched.
        assert!(client.cumulative_volume_jpy() > Decimal::ZERO);
    }

    #[tokio::test]
    async fn fee_window_uses_wall_clock_when_no_clock_is_set() {
        // Paper trading never calls set_clock, so behaviour there is unchanged:
        // a trade made "now" is inside the window.
        let client = funded_client();
        client.send_order(&buy_req(dec!(0.3))).await.unwrap();
        assert_eq!(client.windowed_volume_jpy(), client.cumulative_volume_jpy());
    }

    // ── 約定モデル（項目2） ───────────────────────────────────────────────

    #[test]
    fn ideal_fill_model_is_the_default_and_has_no_impact() {
        let m = FillModel::default();
        assert!(m.is_ideal());
        assert_eq!(m.impact_pct(dec!(1_000_000)), 0.0);
        assert_eq!(MockExchangeClient::new().fill_model(), &FillModel::ideal());
    }

    #[tokio::test]
    async fn ideal_fill_model_fills_at_top_of_book() {
        // Regression guard: the default must reproduce the pre-2026-09-13
        // behaviour exactly — full size, at best_ask for a buy.
        let client = funded_client();
        client.send_order(&buy_req(dec!(1.0))).await.unwrap();
        let trades = client.filled_trades();
        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].size, dec!(1.0));
        assert_eq!(trades[0].price, dec!(9_001_000)); // best_ask untouched
        assert_eq!(client.placed_orders()[0].status, OrderStatus::Completed);
    }

    #[test]
    fn price_impact_is_linear_in_order_notional() {
        // impact_pct = coefficient * (notional / reference_depth)
        let m = FillModel {
            impact_coefficient: 0.01,
            reference_depth_jpy: dec!(10_000_000),
            max_fill_size: None,
        };
        // Notional equal to the reference depth costs exactly the coefficient.
        assert!((m.impact_pct(dec!(10_000_000)) - 0.01).abs() < 1e-12);
        // Half the depth costs half as much — linearity.
        assert!((m.impact_pct(dec!(5_000_000)) - 0.005).abs() < 1e-12);
        // A tiny order is nearly free, which is the property the old model
        // wrongly extended to orders of every size.
        assert!(m.impact_pct(dec!(10_000)) < 1e-4);
    }

    #[tokio::test]
    async fn price_impact_moves_each_side_against_the_taker() {
        let depth = dec!(9_001_000); // one BTC of notional at the ask
        let model = FillModel {
            impact_coefficient: 0.01,
            reference_depth_jpy: depth,
            max_fill_size: None,
        };
        let buyer = funded_client().with_fill_model(model.clone());
        buyer.send_order(&buy_req(dec!(1.0))).await.unwrap();
        let buy_price = buyer.filled_trades()[0].price;
        assert!(
            buy_price > dec!(9_001_000),
            "a buy must pay more than the ask, got {buy_price}"
        );

        let seller = funded_client().with_fill_model(model);
        seller.send_order(&sell_req(dec!(1.0))).await.unwrap();
        let sell_price = seller.filled_trades()[0].price;
        assert!(
            sell_price < dec!(9_000_000),
            "a sell must receive less than the bid, got {sell_price}"
        );
    }

    #[tokio::test]
    async fn larger_orders_pay_strictly_more_impact() {
        // The headline defect: the old model charged a 0.001 BTC order and a
        // 10 BTC order the same price.
        let model = FillModel {
            impact_coefficient: 0.02,
            reference_depth_jpy: dec!(10_000_000),
            max_fill_size: None,
        };
        let small = funded_client().with_fill_model(model.clone());
        small.send_order(&buy_req(dec!(0.001))).await.unwrap();
        let big = funded_client().with_fill_model(model);
        big.send_order(&buy_req(dec!(2.0))).await.unwrap();
        assert!(
            big.filled_trades()[0].price > small.filled_trades()[0].price,
            "a larger order must fill worse"
        );
    }

    #[tokio::test]
    async fn order_beyond_capacity_fills_partially_and_stays_active() {
        let client = funded_client().with_fill_model(FillModel {
            impact_coefficient: 0.0,
            reference_depth_jpy: Decimal::ZERO,
            max_fill_size: Some(dec!(0.4)),
        });
        client.send_order(&buy_req(dec!(1.0))).await.unwrap();

        let trades = client.filled_trades();
        assert_eq!(trades[0].size, dec!(0.4), "only the capacity fills");

        let order = &client.placed_orders()[0];
        assert_eq!(order.size, dec!(1.0), "the request size is preserved");
        assert_eq!(
            order.status,
            OrderStatus::Active,
            "a partially filled order is not Completed"
        );
        // Balance moved by the filled amount only, so the caller's next
        // evaluation sees the residual delta and retries naturally.
        assert_eq!(client.btc_balance(), dec!(100) + dec!(0.4));
    }

    #[tokio::test]
    async fn order_within_capacity_still_completes() {
        let client = funded_client().with_fill_model(FillModel {
            impact_coefficient: 0.0,
            reference_depth_jpy: Decimal::ZERO,
            max_fill_size: Some(dec!(0.4)),
        });
        client.send_order(&buy_req(dec!(0.25))).await.unwrap();
        assert_eq!(client.filled_trades()[0].size, dec!(0.25));
        assert_eq!(client.placed_orders()[0].status, OrderStatus::Completed);
    }

    // ── レート制限（項目1） ───────────────────────────────────────────────

    #[tokio::test]
    async fn mock_has_no_rate_limit_by_default() {
        // The backtest must stay free of throttling: a Simulator run makes
        // millions of calls and any budget would both slow it and change
        // decisions.
        let client = funded_client();
        for _ in 0..50 {
            client.get_balance().await.unwrap();
        }
        assert!(client.rate_limiter.is_none());
    }

    #[tokio::test]
    async fn shared_limiter_counts_every_delegated_call() {
        // Paper trading's defect: get_balance / get_positions / send_order were
        // free, so it consumed one token per evaluation where production
        // consumes three.
        let limiter = Arc::new(RateLimiter::new(200));
        let client = funded_client().with_rate_limiter(Arc::clone(&limiter));

        client.get_balance().await.unwrap();
        client.get_positions("BTC_JPY").await.unwrap();
        client.send_order(&buy_req(dec!(0.001))).await.unwrap();

        assert_eq!(
            limiter.granted(),
            3,
            "all three calls in one handle_indicator cycle must be charged"
        );
    }

    #[tokio::test]
    async fn shared_limiter_is_one_budget_across_clients() {
        // bitFlyer limits per account/IP, not per client object.
        let limiter = Arc::new(RateLimiter::new(200));
        let a = funded_client().with_rate_limiter(Arc::clone(&limiter));
        let b = funded_client().with_rate_limiter(Arc::clone(&limiter));
        a.get_balance().await.unwrap();
        b.get_balance().await.unwrap();
        assert_eq!(limiter.granted(), 2);
    }
}
