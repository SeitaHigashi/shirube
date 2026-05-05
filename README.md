# 導 (shirube)

BTC/JPY automated trading bot powered by bitFlyer API, technical analysis, and local LLM sentiment.

---

## Overview

導 (shirube) is a BTC/JPY automated trading bot built with Rust.
It combines multi-indicator technical analysis with AI-powered news sentiment via a local LLM (Ollama) to generate trading signals, and provides a real-time web dashboard for monitoring.

## Features

- **Real-time market data** via bitFlyer WebSocket
- **Technical analysis** — SMA, EMA, RSI, MACD, Bollinger Bands → composite signal
- **AI news sentiment** — RSS feeds analyzed by local LLM (Ollama), score −1.0 to +1.0
- **Combined signal** — `confidence = TA × 0.7 + |sentiment| × 0.3`
- **Zone-based allocation** — configurable BTC/JPY allocation zones with live updates
- **Risk management** — position limits, daily drawdown circuit breaker, auto daily reset
- **Backtesting engine** — historical simulation with Sharpe ratio, max drawdown, win rate
- **Real-time dashboard** — candlestick chart, balance, orders, news panel (lightweight-charts)
- **Paper trading** — runs without API keys using live public prices

## Architecture

```
Browser Dashboard (HTML + lightweight-charts)
        ↕ WebSocket + REST
API Server (axum)
  ├── Trading Engine (Signal → RiskManager → send_order)
  ├── Signal Engine  (TA indicators → Signal broadcast)
  ├── News Analyzer  (RSS → Ollama sentiment)
  ├── Risk Manager   (position limit / circuit breaker / daily reset)
  └── Backtest Engine
Market Data Bus (tokio broadcast channel)
bitFlyer Exchange Client (REST + WS, rate limiter 200 req/min)
Storage (SQLite WAL)
```

## Tech Stack

| Layer | Library |
|---|---|
| Language | Rust |
| Async runtime | tokio |
| Web server | axum |
| WebSocket client | tokio-tungstenite |
| Frontend | lightweight-charts (CDN) |
| Database | SQLite — rusqlite (WAL mode) |
| RSS parsing | feed-rs |
| LLM sentiment | Ollama HTTP API |
| API auth | hmac + sha2 (HMAC-SHA256) |

## Getting Started

### Prerequisites

- Rust (stable)
- [Ollama](https://ollama.com/) running locally (optional — falls back to sentiment score 0.0)
- bitFlyer API key/secret (optional — paper trading mode works without one)

### Setup

```bash
git clone https://github.com/<your-username>/shirube
cd shirube
cp .env.example .env
# Edit .env with your settings
```

### Run

```bash
cargo run
```

Dashboard is available at `http://localhost:3000`.

Without `BITFLYER_API_KEY`, the bot runs in **paper trading mode** using live public prices.

## Configuration

| Variable | Default | Description |
|---|---|---|
| `BITFLYER_API_KEY` | — | bitFlyer API key (paper trading if absent) |
| `BITFLYER_API_SECRET` | — | bitFlyer API secret |
| `DATABASE_PATH` | `shirube.db` | SQLite file path |
| `API_PORT` | `3000` | Dashboard port |
| `OLLAMA_URL` | `http://localhost:11434` | Ollama server URL |
| `OLLAMA_MODEL` | `llama3` | Ollama model name |
| `NEWS_FEED_URLS` | CoinDesk, CoinTelegraph | Comma-separated RSS URLs |

## Development

```bash
cargo build        # build
cargo test         # run all tests
cargo watch -x run # auto-reload on file change (requires cargo-watch)
```

## License

MIT
