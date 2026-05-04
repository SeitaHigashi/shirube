# 導 (shirube) - bitFlyer BTC/JPY Automated Trading Bot

## Overview

Automated BTC/JPY trading bot using the bitFlyer API. Combines technical analysis with AI-powered news sentiment analysis to generate trading signals.

## Exchange API Reference

bitFlyer の全 REST/Realtime API 仕様（エンドポイント一覧・リクエスト/レスポンス形式・認証方式・実装済み状況）は以下を参照すること:

**`docs/bitflyer-api-spec.md`**

Exchange クライアントの新規実装・拡張・修正を行う際は必ずこのファイルを確認し、仕様と整合性を保つこと。

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

- **Technical Analysis**: SMA, EMA, RSI, MACD, Bollinger Bands → composite signal
- **AI News Analysis**: RSS feeds → Ollama (local LLM) sentiment score (-1.0 to +1.0)
- **Combined Signal**: `confidence = ta * 0.7 + |sentiment| * 0.3`

## Implementation Status — All Phases Complete ✅

| Phase | Content | Tests |
|-------|---------|-------|
| 1 | Foundation (BitFlyer REST/WS, Storage, common types) | 27 |
| 2 | Analysis + Strategy (TA indicators, SignalEngine, RiskManager, TradingEngine) | 77 |
| 3 | Backtesting (Downloader, Simulator, Report, DB persistence) | 93 |
| 4 | Dashboard (axum API, WebSocket Push, frontend) | 110 |
| 5 | News AI (RSS fetch, Ollama sentiment, /api/news/latest, dashboard panel) | 133 |
| 6 | Hardening (WS reconnect, rate limiter, alerts, circuit breaker, daily reset) | **152** |

**Current: 152 tests, all passing**

---

## Phase 1 — Foundation ✅

- `src/types/` — Ticker, Trade, Candle, Order, Balance and other common types
- `src/exchange/bitflyer/` — REST client (public API requires no key), WebSocket client, HMAC-SHA256 auth
- `src/exchange/mock.rs` — MockExchangeClient (for tests and paper trading)
- `src/exchange/mod.rs` — ExchangeClient trait, PublicBitFlyerClient (real prices without API key)
- `src/storage/` — SQLite WAL mode, CandleRepository, OrderRepository, TradeRepository
- `src/main.rs` — startup entry point (auto-switches based on API key presence)

---

## Phase 2 — Analysis + Strategy ✅

```
src/
  market/
    bus.rs              # MarketDataBus — WS + REST → Candle broadcast + DB save
    candle_aggregator.rs # WS executions / REST ticker → Candle aggregation (bucket mgmt)
  signal/
    mod.rs              # Signal type, Indicator trait, MockIndicator
    indicators/
      sma.rs / ema.rs / rsi.rs / macd.rs / bollinger.rs
    engine.rs           # SignalEngine — multi-indicator aggregation → Signal broadcast
  risk/
    mod.rs              # RiskParams, RiskDecision (Allow/Reject/CircuitBreaker)
    manager.rs          # RiskManager — position limit / daily drawdown / circuit breaker
  trading/
    engine.rs           # TradingEngine — Signal → RiskManager → send_order
```

Design notes:
- `SignalEngine::aggregate()` is a pure function for easy testing
- `RiskManager::evaluate()` is pure logic with no async

---

## Phase 3 — Backtesting ✅

```
src/
  backtest/
    mod.rs          # BacktestConfig, BacktestReport type definitions
    downloader.rs   # ExecutionSource trait + Downloader (DB cache first)
    simulator.rs    # SimulatedExchange (ExchangeClient impl) + Simulator
    report.rs       # format_report() statistics output utility
  storage/
    backtest_runs.rs # BacktestRunRepository (DB persistence)
```

Statistics: total return, annualized Sharpe ratio, max drawdown, win rate

---

## Phase 4 — Dashboard ✅

```
src/
  api/
    mod.rs          # AppState (db, exchange, candle_tx, signal_tx, news_cache)
    server.rs       # axum server + build_router() + ServeDir
    routes/
      ticker.rs     # GET /api/ticker
      candles.rs    # GET /api/candles?resolution=60&count=200
      orders.rs     # GET/POST /api/orders
      balance.rs    # GET /api/balance
      backtest.rs   # GET /api/backtest
      news.rs       # GET /api/news/latest
    ws_handler.rs   # WebSocket /ws/candles (Candle broadcast → JSON push)
frontend/
  static/
    index.html      # lightweight-charts SPA (candlestick + balance + orders + news)
```

---

## Phase 5 — News AI ✅

```
src/
  news/
    mod.rs
    fetcher.rs      # FeedSource trait + NewsFetcher (feed-rs, dedup)
    analyzer.rs     # NewsAnalyzer (Ollama HTTP API, fallback score=0.0)
    scorer.rs       # NewsScorer (combined_confidence, sentiment_to_signal)
```

- Sentiment scored via `POST {OLLAMA_URL}/api/generate`
- Falls back to `score = 0.0` when Ollama is unavailable
- Fetched and analyzed in background every hour
- `GET /api/news/latest` → returns `Vec<SentimentScore>`
- Duplicate articles skipped via DB URL lookup; cache refreshed from DB each cycle

---

## Phase 6 — Hardening ✅

```
src/
  exchange/
    rate_limiter.rs   # Token bucket (200 req/min)
    bitflyer/
      ws.rs           # run_with_reconnect() (exponential backoff, 1s initial / 60s max)
      rest.rs         # rate_limiter.acquire() integrated into all HTTP methods
  alert/
    mod.rs            # AlertManager (log + Slack Webhook notification)
  risk/
    mod.rs            # RiskDecision::CircuitBreaker { drawdown_pct } added
  trading/
    engine.rs         # CircuitBreaker → alert.circuit_breaker_triggered()
                      # last_reset_date detects UTC date change → auto reset_daily()
```

Key design decisions:
- WS reconnect: reconnects on both server Close and errors; exits only when `tx.receiver_count() == 0`
- Rate limiter: `RateLimiter::new(200)` built into BitFlyerRestClient
- Alerts: Slack notifications enabled via `SLACK_WEBHOOK_URL` env var
- Daily reset: TradingEngine detects UTC date change and calls reset automatically

---

## Environment Variables

| Variable | Default | Purpose |
|----------|---------|---------|
| `BITFLYER_API_KEY` | — | BitFlyer API key (uses mock if absent) |
| `BITFLYER_API_SECRET` | — | BitFlyer API secret |
| `DATABASE_PATH` | `shirube.db` | SQLite file path |
| `API_PORT` | `3000` | API server port |
| `OLLAMA_URL` | `http://localhost:11434` | Ollama LLM server |
| `OLLAMA_MODEL` | `llama3` | Model to use |
| `NEWS_FEED_URLS` | CoinDesk, CoinTelegraph | RSS feed URLs (comma-separated) |
| `SLACK_WEBHOOK_URL` | — | Slack alert webhook (optional) |

---

## Development

- Build: `cargo build`
- Test: `cargo test`
- API keys via environment variables. Never hardcode secrets.

## Git Operations

**MANDATORY: Always commit and push after every implementation task, without waiting for user instruction.**

After completing any implementation task:

1. Run `cargo test` and confirm all tests pass
2. Stage only changed files (avoid `git add -A`)
3. Commit using conventional commits format with an **English** message:
   ```
   <type>: <short description>

   - bullet points of changes
   ```
   Types: `feat` / `fix` / `refactor` / `docs` / `test` / `chore` / `perf`
4. Verify with `git log --oneline -3`
5. Run `git push` to push the commit to remote

**Rules:**
- Never commit secrets or `.env` files
- Never use `--force` or `--no-verify`

## Comment Policy

ALWAYS add comments when writing or modifying code:
- Public structs/traits/functions: `///` doc comment explaining purpose and usage
- Non-trivial logic blocks (>5 lines): inline `//` comment explaining "what & why"
- Mathematical formulas: comment the formula name, inputs, and expected range
- Design decisions: `// NOTE: ...` explaining why this approach was chosen
- Async coordination patterns: explain task lifecycle and channel semantics
- All comments in **English**
