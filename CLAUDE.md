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
```

## Trading Strategy

- **Technical Analysis**: SMA, EMA, RSI, MACD, Bollinger Bands → composite signal
- **AI News Analysis**: RSS feeds → Ollama (local LLM) sentiment score (-1.0 to +1.0)
- **Combined Signal**: `confidence = ta * 0.7 + |sentiment| * 0.3`
- **Zone-based Allocation**: `TradingConfig` + `ZoneConfig` で BTC 配分率をゾーン補間

---

## Module Structure

```
src/
  config.rs           # TradingConfig + ZoneConfig (zone-based BTC allocation, DB-persisted)
  error.rs            # unified error type
  main.rs             # startup entry point (auto-switches based on API key presence)

  types/
    market.rs         # Ticker, Trade, Candle
    order.rs          # Order, Balance
    mod.rs

  exchange/
    bitflyer/
      rest.rs         # REST client + rate_limiter.acquire() on all HTTP methods
      ws.rs           # WebSocket client + run_with_reconnect() (exponential backoff)
      auth.rs         # HMAC-SHA256 signature
      models.rs       # bitFlyer-specific response structs
      mod.rs
    mock.rs           # MockExchangeClient (for tests and paper trading)
    rate_limiter.rs   # Token bucket (200 req/min)
    mod.rs            # ExchangeClient trait, PublicBitFlyerClient

  market/
    bus.rs            # MarketDataBus — WS + REST → Candle broadcast + DB save
    candle_aggregator.rs  # WS executions / REST ticker → Candle aggregation (bucket mgmt)
    mod.rs

  signal/
    mod.rs            # Signal type, Indicator trait, MockIndicator, SignalDetail, AllocationSignal
    engine.rs         # SignalEngine — multi-indicator aggregation → Signal broadcast
    indicators/
      sma.rs / ema.rs / rsi.rs / macd.rs / bollinger.rs
      mod.rs

  risk/
    mod.rs            # RiskParams, RiskDecision (Allow/Reject/CircuitBreaker)
    manager.rs        # RiskManager — position limit / daily drawdown / circuit breaker

  trading/
    engine.rs         # TradingEngine — Signal → RiskManager → send_order
                      # last_reset_date detects UTC date change → auto reset_daily()
    mod.rs

  backtest/
    mod.rs            # BacktestConfig, BacktestReport type definitions
    downloader.rs     # ExecutionSource trait + Downloader (DB cache first)
    simulator.rs      # SimulatedExchange (ExchangeClient impl) + Simulator
    report.rs         # format_report() statistics output utility

  news/
    mod.rs
    fetcher.rs        # FeedSource trait + NewsFetcher (feed-rs, dedup)
    analyzer.rs       # NewsAnalyzer (Ollama HTTP API, fallback score=0.0)
    scorer.rs         # NewsScorer (combined_confidence, sentiment_to_signal)

  storage/
    db.rs             # Database wrapper (WAL mode)
    schema.rs         # CREATE TABLE definitions
    candles.rs        # CandleRepository
    tickers.rs        # TickerRepository
    orders.rs         # OrderRepository
    trades.rs         # TradeRepository
    news_sentiments.rs # NewsSentimentRepository
    config.rs         # ConfigRepository (TradingConfig persistence)
    mock_state.rs     # MockStateRepository (paper trading balance/order state)
    mod.rs

  api/
    mod.rs            # AppState (db, exchange, candle_tx, signal_tx, news_cache, trading_config, latest_signal)
    server.rs         # axum server + build_router() + ServeDir
    ws_handler.rs     # WebSocket /ws/candles (Candle broadcast → JSON push)
    routes/
      ticker.rs       # GET /api/ticker
      candles.rs      # GET /api/candles
      orders.rs       # GET/POST /api/orders
      balance.rs      # GET /api/balance
      backtest.rs     # GET /api/backtest
      news.rs         # GET /api/news/latest
      config.rs       # GET/PUT /api/config (TradingConfig)
      signal.rs       # GET /api/signal (latest SignalDetail)
      mod.rs

  bin/
    migrate_candles_to_tickers.rs  # one-off migration utility
```

```
frontend/
  static/
    index.html        # lightweight-charts SPA (candlestick + balance + orders + news)
```

---

## Key Design Decisions

- `SignalEngine::aggregate()` is a pure function for easy testing
- `RiskManager::evaluate()` is pure logic with no async
- WS reconnect: reconnects on both server Close and errors; exits only when `tx.receiver_count() == 0`
- Rate limiter: `RateLimiter::new(200)` built into BitFlyerRestClient
- Daily reset: TradingEngine detects UTC date change and calls reset automatically
- `TradingConfig` / `ZoneConfig` persisted to DB via `ConfigRepository`; loaded at startup and held in `RwLock` for live updates
- Sentiment: `POST {OLLAMA_URL}/api/generate`; falls back to `score = 0.0` when Ollama unavailable
- News dedup: skips duplicate articles via DB URL lookup; cache refreshed from DB each cycle

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
