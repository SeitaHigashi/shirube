# trader2 - bitFlyer BTC/JPY Automated Trading Bot

## Overview

Automated BTC/JPY trading bot using the bitFlyer API. Combines technical analysis with AI-powered news sentiment analysis to generate trading signals.

## Tech Stack

- **Language**: Rust
- **Async Runtime**: tokio
- **Web Server**: axum
- **Frontend**: lightweight-charts (HTML + JS CDN)
- **DB**: SQLite (rusqlite, WAL mode)
- **WebSocket**: tokio-tungstenite
- **News Feed**: feed-rs (RSS)
- **Sentiment Analysis**: Ollama HTTP API (local LLM)
- **Auth**: hmac + sha2 (bitFlyer API signature)

## Architecture

```
Browser Dashboard (HTML + lightweight-charts)
        ↕ WebSocket + REST
API Server (axum)
  ├── Trading Engine (Signal → RiskManager → send_order)
  ├── Signal Engine (TA indicators → Signal broadcast)
  ├── News Analyzer (RSS → Ollama sentiment)
  ├── Risk Manager (position limit / circuit breaker / daily reset)
  └── Backtest Engine
Market Data Bus (tokio broadcast channel)
bitFlyer Exchange Client (REST + WS, rate limiter)
Storage (SQLite)
Alert Manager (log + Slack Webhook)
```

## Trading Strategy

- **Technical Analysis**: SMA, EMA, RSI, MACD, Bollinger Bands → 合成シグナル
- **AI News Analysis**: RSSニュース → Ollama (local LLM) センチメント判定 (-1.0〜+1.0)
- **Combined Signal**: `confidence = ta * 0.7 + |sentiment| * 0.3`

## 実装状況 — 全Phase完了 ✅

| Phase | 内容 | テスト累計 |
|-------|------|-----------|
| 1 | Foundation（BitFlyer REST/WS、Storage、共通型） | 27本 |
| 2 | Analysis + Strategy（TA指標、SignalEngine、RiskManager、TradingEngine） | 77本 |
| 3 | Backtesting（Downloader、Simulator、Report、DB保存） | 93本 |
| 4 | Dashboard（axum API、WebSocket Push、フロントエンド） | 110本 |
| 5 | News AI（RSS取得、Ollama感情分析、/api/news/latest、ダッシュボードパネル） | 133本 |
| 6 | Hardening（WS再接続、レートリミット、アラート、サーキットブレーカー強化、日次リセット） | **143本** |

**現在: 143本 全パス**

---

## Phase 1 実装済み (Foundation) ✅

- `src/types/` — Ticker, Trade, Candle, Order, Balance など共通型
- `src/exchange/bitflyer/` — REST クライアント（公開 API はキー不要）、WebSocket クライアント、HMAC-SHA256 認証
- `src/exchange/mock.rs` — MockExchangeClient（テスト・ペーパートレード用）
- `src/exchange/mod.rs` — ExchangeClient trait、PublicBitFlyerClient（キーなしでも実価格取得）
- `src/storage/` — SQLite WAL モード、CandleRepository、OrderRepository、TradeRepository
- `src/main.rs` — 起動エントリポイント（API キー有無で自動切り替え）

---

## Phase 2 実装済み (Analysis + Strategy) ✅

```
src/
  market/
    bus.rs              # MarketDataBus — WS + REST → Candle broadcast + DB 保存
    candle_aggregator.rs # WS executions / REST ticker → Candle 集計（bucket 管理）
  signal/
    mod.rs              # Signal 型、Indicator trait、MockIndicator
    indicators/
      sma.rs / ema.rs / rsi.rs / macd.rs / bollinger.rs
    engine.rs           # SignalEngine — 複数インジケータ合成 → Signal broadcast
  risk/
    mod.rs              # RiskParams、RiskDecision（Allow/Reject/CircuitBreaker）
    manager.rs          # RiskManager — ポジション上限・日次 drawdown・circuit breaker
  trading/
    engine.rs           # TradingEngine — Signal → RiskManager → send_order
```

設計ポイント:
- `SignalEngine::aggregate()` は純粋関数でテスト容易
- `RiskManager::evaluate()` は非同期なしのピュアロジック

---

## Phase 3 実装済み (Backtesting) ✅

```
src/
  backtest/
    mod.rs          # BacktestConfig, BacktestReport 型定義
    downloader.rs   # ExecutionSource trait + Downloader（DB キャッシュ優先）
    simulator.rs    # SimulatedExchange（ExchangeClient 実装）+ Simulator
    report.rs       # format_report() 統計出力ユーティリティ
  storage/
    backtest_runs.rs # BacktestRunRepository（DB 永続化）
```

統計出力: total return, Sharpe ratio（年率化）, max drawdown, win rate

---

## Phase 4 実装済み (Dashboard) ✅

```
src/
  api/
    mod.rs          # AppState（db, exchange, candle_tx, signal_tx, news_cache）
    server.rs       # axum サーバー + build_router() + ServeDir
    routes/
      ticker.rs     # GET /api/ticker
      candles.rs    # GET /api/candles?resolution=60&count=200
      orders.rs     # GET/POST /api/orders
      balance.rs    # GET /api/balance
      backtest.rs   # GET /api/backtest
      news.rs       # GET /api/news/latest
    ws_handler.rs   # WebSocket /ws/candles（Candle broadcast → JSON Push）
frontend/
  static/
    index.html      # lightweight-charts SPA（candlestick + balance + orders + news）
```

---

## Phase 5 実装済み (News AI) ✅

```
src/
  news/
    mod.rs
    fetcher.rs      # FeedSource trait + NewsFetcher（feed-rs、重複排除）
    analyzer.rs     # NewsAnalyzer（Ollama HTTP API、fallback score=0.0）
    scorer.rs       # NewsScorer（combined_confidence、sentiment_to_signal）
```

- `POST {OLLAMA_URL}/api/generate` でセンチメント判定
- Ollama 不在時は `score = 0.0` フォールバック
- 5分ごとにバックグラウンドで取得・分析
- `GET /api/news/latest` → `Vec<SentimentScore>` を返す

---

## Phase 6 実装済み (Hardening) ✅

```
src/
  exchange/
    rate_limiter.rs   # トークンバケット（200 req/min）
    bitflyer/
      ws.rs           # run_with_reconnect()（指数バックオフ、初期1s・最大60s）
      rest.rs         # 全HTTPメソッドに rate_limiter.acquire() を統合
  alert/
    mod.rs            # AlertManager（ログ + Slack Webhook通知）
  risk/
    mod.rs            # RiskDecision::CircuitBreaker { drawdown_pct } 追加
  trading/
    engine.rs         # CircuitBreaker → alert.circuit_breaker_triggered()
                      # last_reset_date で UTC 日付変更時に reset_daily() 自動呼び出し
```

主要設計:
- WS再接続: サーバーClose・エラー両方で再接続、`tx.receiver_count() == 0` のみ終了
- レートリミット: `RateLimiter::new(200)` を BitFlyerRestClient が内蔵
- アラート: `SLACK_WEBHOOK_URL` 環境変数で Slack 通知を有効化
- 日次リセット: TradingEngine 内で UTC 日付変更を検知し自動実行

---

## 環境変数

| 変数 | デフォルト | 用途 |
|------|-----------|------|
| `BITFLYER_API_KEY` | — | BitFlyer APIキー（なければモック） |
| `BITFLYER_API_SECRET` | — | BitFlyer APIシークレット |
| `DATABASE_PATH` | `trader2.db` | SQLiteパス |
| `API_PORT` | `3000` | APIサーバーポート |
| `OLLAMA_URL` | `http://localhost:11434` | Ollama LLMサーバー |
| `OLLAMA_MODEL` | `llama3` | 使用モデル |
| `NEWS_FEED_URLS` | CoinDesk,CoinTelegraph | RSSフィードURL（カンマ区切り）|
| `SLACK_WEBHOOK_URL` | — | Slackアラート通知（任意） |

---

## Development

- Build: `cargo build`
- Test: `cargo test`
- bitFlyer API keys via environment variables. Never hardcode secrets.

## Git Operations

実装完了時は必ず以下の手順で git commit すること：

1. `cargo test` を実行してテストが全パスすることを確認する
2. 変更したファイルのみ `git add` でステージング（`git add -A` は使わない）
3. conventional commits 形式でコミット：
   ```
   <type>: <日本語の説明>

   - 変更点の箇条書き
   ```
   type: `feat` / `fix` / `refactor` / `docs` / `test` / `chore` / `perf`
4. コミット後に `git log --oneline -3` で確認する

**注意事項：**
- secrets・`.env` ファイルは絶対にコミットしない
- ユーザーから明示的に指示された場合のみ `git push` する
- force push / `--no-verify` は使用しない
