# trader2 - bitFlyer BTC/JPY Automated Trading Bot

## Overview

Automated BTC/JPY trading bot using the bitFlyer API. Combines technical analysis with AI-powered news sentiment analysis to generate trading signals.

## Tech Stack

- **Language**: Rust
- **Async Runtime**: tokio
- **Web Server**: axum
- **Frontend**: Leptos (WASM) + lightweight-charts (JS interop)
- **DB**: SQLite (rusqlite, WAL mode)
- **WebSocket**: tokio-tungstenite
- **News Feed**: feed-rs (RSS)
- **Sentiment Analysis**: Ollama HTTP API (local LLM)
- **Auth**: hmac + sha2 (bitFlyer API signature)

## Architecture

```
Browser Dashboard (Leptos WASM)
        ↕ WebSocket + REST
API Server (axum)
  ├── Trading Engine
  ├── Signal Engine (TA indicators)
  ├── News Analyzer (sentiment)
  ├── Risk Manager
  └── Backtest Engine
Market Data Bus (tokio broadcast channel)
bitFlyer Exchange Client (REST + WS)
Storage (SQLite)
```

## Trading Strategy

- **Technical Analysis**: Signal generation combining SMA, EMA, RSI, MACD, Bollinger Bands
- **AI News Analysis**: Fetch crypto news headlines via RSS, score sentiment with Ollama (local LLM)
- **Combined Signal**: Weighted score integrating TA + news sentiment for buy/sell decisions

## Implementation Phases

1. **Foundation**: bitFlyer Exchange Client (REST+WS auth), Storage, common type definitions
2. **Analysis + Strategy**: TA indicator calculation, signal synthesis, risk management (stop-loss, position sizing, drawdown limits)
3. **Backtesting**: Historical OHLCV data downloader, simulator, statistical output (total return, Sharpe ratio, max drawdown, win rate)
4. **Dashboard**: axum API server + Leptos SPA (charts, PnL, balances, logs)
5. **News AI**: RSS fetching, Ollama sentiment analysis, integration into strategy engine
6. **Hardening**: WS reconnection (exponential backoff), order state recovery, alerts, tests

## Risk Controls

- Mandatory paper trading phase before live trading
- Circuit breaker on max drawdown (e.g., daily -5% halts trading)
- Start with minimum lot size (0.001 BTC)
- API rate limit handling (token bucket)
- REST API gap-fill on WS disconnection

## Phase 1 実装済み (Foundation) ✅

- `src/types/` — Ticker, Trade, Candle, Order, Balance など共通型
- `src/exchange/bitflyer/` — REST クライアント（公開 API はキー不要）、WebSocket クライアント、HMAC-SHA256 認証
- `src/exchange/mock.rs` — MockExchangeClient（テスト・ペーパートレード用）
- `src/exchange/mod.rs` — ExchangeClient trait、PublicBitFlyerClient（キーなしでも実価格取得）
- `src/storage/` — SQLite WAL モード、CandleRepository、OrderRepository、TradeRepository
- `src/main.rs` — 起動エントリポイント（API キー有無で自動切り替え）
- **テスト: 27本 全パス**

## Phase 2 実装済み (Analysis + Strategy) ✅

### 実装内容

```
src/
  market/
    bus.rs              # MarketDataBus — WS + REST → Candle broadcast + DB 保存
    candle_aggregator.rs # WS executions / REST ticker → Candle 集計（bucket 管理）
  signal/
    mod.rs              # Signal 型、Indicator trait、MockIndicator
    indicators/
      sma.rs            # Simple Moving Average
      ema.rs            # Exponential Moving Average（MACD から内部利用）
      rsi.rs            # RSI（Wilder's smoothing, 14期）
      macd.rs           # MACD（fast=12, slow=26, signal=9）
      bollinger.rs      # Bollinger Bands（period=20, multiplier=2）
    engine.rs           # SignalEngine — 複数インジケータ合成 → Signal broadcast
  risk/
    mod.rs              # RiskParams（max_position=0.1BTC, drawdown=5%, min_size=0.001）
    manager.rs          # RiskManager — ポジション上限・日次 drawdown・circuit breaker
  trading/
    engine.rs           # TradingEngine — Signal → RiskManager → send_order
```

### 設計ポイント

- `Indicator` trait に `reset()` / `min_periods()` を持たせ Phase 3 バックテストと共通利用
- `SignalEngine::aggregate()` は純粋関数で Buy/Sell の confidence を合算してシグナル合成
- `RiskManager::evaluate()` は非同期なしのピュアロジック（テスト容易）
- `main.rs` を配線専用に整理: `MarketDataBus → SignalEngine → TradingEngine`
- **テスト: 77本 全パス（Phase 1: 27本 + Phase 2: 50本）**

## Phase 3 実装済み (Backtesting) ✅

### 実装内容

```
src/
  backtest/
    mod.rs          # BacktestConfig, BacktestReport 型定義
    downloader.rs   # ExecutionSource trait + Downloader（DB キャッシュ優先）
    simulator.rs    # SimulatedExchange（ExchangeClient 実装）+ Simulator
    report.rs       # format_report() 統計出力ユーティリティ
  storage/
    backtest_runs.rs # BacktestRunRepository（DB 永続化）
    schema.rs       # migration v2: backtest_runs テーブル追加
```

### 設計ポイント

- `SimulatedExchange` は `ExchangeClient` を実装し、スリッページ・手数料設定可能
- `Simulator::run()` は `Vec<Candle>` + `Vec<Box<dyn Indicator>>` + `RiskParams` を受け取り、本番と同じ `SignalEngine::aggregate()` / `RiskManager::evaluate()` ロジックをそのまま使用
- `Downloader` は `ExecutionSource` trait 経由で bitFlyer REST をモック差し替え可能
- バックテスト結果は `backtest_runs` テーブルに自動保存
- 統計: total return, Sharpe ratio（年率化）, max drawdown, win rate
- **テスト: 93本 全パス（Phase 1: 27本 + Phase 2: 50本 + Phase 3: 16本）**

```rust
// src/backtest/simulator.rs
pub struct BacktestConfig {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub resolution_secs: u32,
    pub slippage_pct: f64,      // スリッページ率
    pub fee_pct: f64,           // 手数料率
    pub initial_jpy: Decimal,
}

pub struct BacktestReport {
    pub total_return_pct: f64,
    pub sharpe_ratio: f64,
    pub max_drawdown_pct: f64,
    pub win_rate: f64,
    pub total_trades: u32,
}
```

### Phase 2 → 3 の移行ポイント

- `SignalEngine::start()` の引数を `impl Stream<Item = Candle>` に変えておくと、本番（broadcast）とバックテスト（Vec のイテレータ）を差し替えやすい
- バックテスト結果を DB に保存するテーブル (`backtest_runs`) を schema.rs に追加する

---

## Phase 4 実装済み (Dashboard) ✅

### 実装内容

```
src/
  api/
    mod.rs          # AppState 定義
    server.rs       # axum サーバー + build_router() + ServeDir
    routes/
      ticker.rs     # GET /api/ticker
      candles.rs    # GET /api/candles?resolution=60&count=200
      orders.rs     # GET/POST /api/orders
      balance.rs    # GET /api/balance
      backtest.rs   # GET /api/backtest
    ws_handler.rs   # WebSocket /ws/candles（Candle broadcast → JSON Push）
frontend/
  static/
    index.html      # lightweight-charts SPA（candlestick chart + balance + orders）
```

### 設計ポイント

- `AppState` に `db`, `exchange`, `candle_tx`, `signal_tx` をまとめ axum `State` で共有
- WebSocket は `candle_tx` を subscribe し、新 Candle が来るたび JSON で Push
- フロントエンドは純粋な HTML + lightweight-charts CDN（Rust/WASM 不要でシンプルに）
- `tower_http::ServeDir` で `frontend/static/` を `/` にマウント
- `tower_http::CorsLayer` で CORS を許可（ローカル開発用）
- **テスト: 110本 全パス（Phase 1: 27本 + Phase 2: 50本 + Phase 3: 16本 + Phase 4: 17本）**

### Phase 3 → 4 の移行ポイント

- バックテスト結果を API 経由で返せるよう `backtest_runs` テーブルを Phase 3 で作っておく
- `Candle` と `Signal` に `serde::Serialize` が実装済みなので REST レスポンスにそのまま使える

```rust
// src/api/server.rs
pub struct AppState {
    pub db: Database,
    pub exchange: Arc<dyn ExchangeClient>,
    pub candle_tx: broadcast::Sender<Candle>,
    pub signal_tx: broadcast::Sender<Signal>,
}
```

### Phase 3 → 4 の移行ポイント

- バックテスト結果を API 経由で返せるよう `backtest_runs` テーブルを Phase 3 で作っておく
- `Candle` と `Signal` に `serde::Serialize` が実装済みなので REST レスポンスにそのまま使える

---

## Phase 5 移行方針 (News AI)

### 構成

```
src/
  news/
    mod.rs
    fetcher.rs      # feed-rs で RSS フィード取得
    analyzer.rs     # Ollama HTTP API でセンチメント判定
    scorer.rs       # センチメントスコアを Signal の confidence に混合
```

### 設計方針

- RSS フィードは定期取得（例: 5分ごと）し、新着ヘッドラインのみ Ollama に投げる
- Ollama への問い合わせは `reqwest` で `POST http://localhost:11434/api/generate`
- センチメントスコア (-1.0〜+1.0) を `Signal` の `confidence` に加重平均で合算する

```rust
// src/news/analyzer.rs
pub struct SentimentScore {
    pub headline: String,
    pub score: f64,       // -1.0 (強気売り) 〜 +1.0 (強気買い)
    pub analyzed_at: DateTime<Utc>,
}

// src/news/scorer.rs の合成例
// combined_confidence = ta_confidence * 0.7 + sentiment * 0.3
```

- Ollama が起動していない場合は `score = 0.0` にフォールバックし、TA シグナルのみで動作を続ける
- `NewsAnalyzer` を `ExchangeClient` と同様に trait 化し、`MockNewsAnalyzer` でテストする

### Phase 4 → 5 の移行ポイント

- Dashboard にセンチメントスコアのパネルを追加する（API: `GET /api/news/latest`）
- Ollama モデルは `OLLAMA_MODEL` 環境変数で切り替え可能にする（デフォルト: `llama3`）

---

## Phase 6 移行方針 (Hardening)

### 対応項目

| 項目 | 実装場所 | 内容 |
|------|---------|------|
| WS 再接続 | `src/exchange/bitflyer/ws.rs` | 指数バックオフ（初期 1s、最大 60s）でループ再試行 |
| REST ギャップフィル | `src/market/bus.rs` | WS 切断中の期間を REST でさかのぼり取得 |
| 注文状態リカバリ | `src/trading/engine.rs` | 起動時に DB の ACTIVE 注文を取引所に照合し状態を同期 |
| API レートリミット | `src/exchange/bitflyer/rest.rs` | トークンバケット（bitFlyer: 200 req/min） |
| サーキットブレーカー | `src/risk/manager.rs` | 日次損失が `-5%` を超えたら全注文キャンセル・取引停止 |
| アラート | `src/alert/mod.rs` | 閾値超過時にログ + （オプション）Slack Webhook 通知 |
| テストカバレッジ | 全モジュール | `cargo tarpaulin` で 80% 以上を確認 |

### WS 再接続の実装イメージ

```rust
// src/exchange/bitflyer/ws.rs
pub async fn run_with_reconnect(self, channels: Vec<String>, tx: broadcast::Sender<WsMessage>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        match self.run(channels.clone(), tx.clone()).await {
            Ok(()) => break,  // 正常終了
            Err(e) => {
                warn!("WS disconnected: {}. Reconnecting in {:?}", e, backoff);
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
}
```

### Phase 5 → 6 の移行ポイント

- Phase 6 はリファクタ・テスト追加が中心。新機能追加は最小限にする
- `cargo clippy -- -D warnings` と `cargo fmt --check` を CI で常時通す
- ライブトレード移行前に最低 1 週間のペーパートレード（`MockExchangeClient` + 実価格）でシグナル品質を確認する

---

## Development

- Build: `cargo build`
- Test: `cargo test`
- Lint: `cargo clippy`
- Format: `cargo fmt`
- bitFlyer API keys via environment variables (`BITFLYER_API_KEY`, `BITFLYER_API_SECRET`). Never hardcode secrets.
