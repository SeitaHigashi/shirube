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
- **Frontend**: Svelte 4 + Vite 5 + TypeScript（bun でビルド）
- **Charts**: lightweight-charts v4（npm パッケージ）
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

`src/` mirrors the architecture above, one directory per box:
`config` · `error` · `types` · `exchange` (`bitflyer/{rest,ws,auth,models}`, `mock`, `rate_limiter`)
· `market` · `signal` (`engine`, `indicators/`) · `risk` · `trading` · `backtest` · `news`
· `storage` (one repository per table) · `api` (`server`, `ws_handler`, `routes/`).

`frontend/` is a Svelte app: `src/lib/{api,ws,stores,types}.ts` plus `src/components/*.svelte`;
`bun run build` emits `frontend/static/`, which axum's `ServeDir` serves and which **is committed**.

Read the directory itself for file-level detail — it is authoritative, this list is not.

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
- Test: `cargo test` (unit tests + `tests/` integration tests)
- API keys via environment variables. Never hardcode secrets.

## Testing Policy

**IMPORTANT: write the test first.** Write a failing test, run `cargo test` and
confirm it fails for the reason you expect, then implement until it passes.
**Never edit a test to make it pass** — fix the implementation. If a test turns
out to encode the wrong expectation, say so explicitly before changing it.

### Tests are REQUIRED for

- Pure logic: `signal/` (indicators, `engine`), `risk/manager`, `news/scorer`,
  `config`, `backtest/` — normal case, boundary values, and error case
- Any new `storage/` repository method or `api/routes/` endpoint
  (use `Database::open_in_memory()`)
- **Every bug fix** — a regression test that fails before the fix and passes after

Tests are optional for thin I/O wrappers (`exchange/bitflyer/{rest,ws}`,
`market/bus`, `updater`); prefer covering those through `MockExchangeClient`
rather than hand-mocking each call.

### Where tests go

- Unit tests: `#[cfg(test)] mod tests` in the same file (can reach private items)
- Integration tests: `tests/*.rs`, public API only, shared helpers in `tests/common/`.
  Add one when a change crosses a module boundary — signal→trading→risk→exchange,
  api→storage, or the backtest pipeline.

### Quality bar (a bad test is worse than no test)

- Test **behavior**, not implementation. Ask: *would this fail if the logic broke?*
  If not, delete it.
- No trivial tests (getters, struct construction, `Default` values)
- Never mock the thing under test; use real in-memory DB and `MockExchangeClient`
- Assert on values, never just "it didn't panic"
- Deterministic: inject timestamps, never `sleep` on a fixed duration to await work
- Name the scenario, not the function: `a_bearish_reversal_sells_the_position_back_out`

## Frontend Development

フロントエンドは Svelte 4 + Vite 5 + TypeScript で実装。パッケージマネージャは **bun**。
`nix develop` 環境（`flake.nix` に `pkgs.bun` 定義済み）または bun が PATH にある環境で実行する。

### セットアップ（初回のみ）
```bash
cd frontend && bun install
```

### ビルド
```bash
cd frontend && bun run build
# → frontend/static/ に index.html + assets/ が生成される
# axum の ServeDir がこのディレクトリを直接配信するため、ビルド成果物もコミット対象
```

### フロントエンドを変更したときのコミット手順
`frontend/src/` を変更した場合は **必ず** ビルドを実行してから成果物を一緒にコミットする。
ビルドせずにコミットすると本番サーバーが旧バージョンの UI を配信し続ける。

```bash
cd frontend && bun run build
cd ..
git add frontend/src/ frontend/static/   # 変更ファイルのみ
cargo test                               # バックエンドのテストが通ることを確認
git commit -m "feat: ..."
git push
```

## Git Operations

**MANDATORY: Always commit and push after every implementation task, without waiting for user instruction.**

After completing any implementation task:

1. If frontend source (`frontend/src/`) was changed, run `cd frontend && bun run build` first
2. Confirm the change carries the tests the Testing Policy requires
3. Run `cargo test` and confirm all tests pass
4. Stage only changed files (avoid `git add -A`)
5. Commit using conventional commits format with an **English** message:
   ```
   <type>: <short description>

   - bullet points of changes
   ```
   Types: `feat` / `fix` / `refactor` / `docs` / `test` / `chore` / `perf`
6. Verify with `git log --oneline -3`
7. Run `git push` to push the commit to remote

**Rules:**
- Never commit secrets or `.env` files
- Never use `--force` or `--no-verify`
- Daily development work goes on the `dev` branch; push to `main` only to release

## Branch Strategy & Release

- **Development branch**: `dev` — all feature/fix work goes here
- **Production branch**: `main` — merge from `dev` triggers automatic release
- **Release process**: Push/merge to `main` → CI computes version `0.1.<commit-count>` →
  builds Linux x86_64 binary → creates GitHub Release and git tag automatically
- **No manual tagging or version bumping required**

### Daily Development Flow

1. Work on `dev` branch (or feature branches off `dev`)
2. When ready to release: open PR `dev → main` and merge
3. CI automatically releases `v0.1.<N>` within a few minutes
4. Running `shirube` instances pick up the update within 60 minutes (auto-updater)

## Comment Policy

ALWAYS add comments when writing or modifying code:
- Public structs/traits/functions: `///` doc comment explaining purpose and usage
- Non-trivial logic blocks (>5 lines): inline `//` comment explaining "what & why"
- Mathematical formulas: comment the formula name, inputs, and expected range
- Design decisions: `// NOTE: ...` explaining why this approach was chosen
- Async coordination patterns: explain task lifecycle and channel semantics
- All comments in **English**
