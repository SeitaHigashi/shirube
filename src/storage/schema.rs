use tokio_rusqlite::Connection;

use crate::error::Result;

pub async fn migrate(conn: &Connection) -> Result<()> {
    conn.call(|c| {
        // Enable WAL mode
        c.execute_batch("PRAGMA journal_mode=WAL;")?;

        // Create schema_version table
        c.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (
                version     INTEGER PRIMARY KEY,
                applied_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
            );",
        )?;

        // Check current version
        let current_version: i64 = c
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_version",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        if current_version < 1 {
            c.execute_batch(
                "CREATE TABLE IF NOT EXISTS candles (
                    id              INTEGER PRIMARY KEY AUTOINCREMENT,
                    product_code    TEXT    NOT NULL,
                    open_time       TEXT    NOT NULL,
                    resolution_secs INTEGER NOT NULL,
                    open            TEXT    NOT NULL,
                    high            TEXT    NOT NULL,
                    low             TEXT    NOT NULL,
                    close           TEXT    NOT NULL,
                    volume          TEXT    NOT NULL,
                    UNIQUE(product_code, open_time, resolution_secs)
                );
                CREATE INDEX IF NOT EXISTS idx_candles_product_time
                    ON candles(product_code, resolution_secs, open_time DESC);

                CREATE TABLE IF NOT EXISTS orders (
                    id              INTEGER PRIMARY KEY AUTOINCREMENT,
                    acceptance_id   TEXT    NOT NULL UNIQUE,
                    product_code    TEXT    NOT NULL,
                    side            TEXT    NOT NULL CHECK(side IN ('BUY','SELL')),
                    order_type      TEXT    NOT NULL CHECK(order_type IN ('LIMIT','MARKET')),
                    price           TEXT,
                    size            TEXT    NOT NULL,
                    status          TEXT    NOT NULL CHECK(status IN ('ACTIVE','COMPLETED','CANCELED','EXPIRED','REJECTED')),
                    created_at      TEXT    NOT NULL,
                    updated_at      TEXT    NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_orders_status ON orders(status);
                CREATE INDEX IF NOT EXISTS idx_orders_created ON orders(created_at DESC);

                CREATE TABLE IF NOT EXISTS trades (
                    id              INTEGER PRIMARY KEY,
                    exec_date       TEXT    NOT NULL,
                    price           TEXT    NOT NULL,
                    size            TEXT    NOT NULL,
                    side            TEXT    NOT NULL CHECK(side IN ('BUY','SELL')),
                    buy_order_id    TEXT    NOT NULL,
                    sell_order_id   TEXT    NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_trades_exec_date ON trades(exec_date DESC);

                INSERT INTO schema_version(version) VALUES(1);",
            )?;
        }

        if current_version < 2 {
            c.execute_batch(
                "CREATE TABLE IF NOT EXISTS backtest_runs (
                    id              INTEGER PRIMARY KEY AUTOINCREMENT,
                    created_at      TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
                    product_code    TEXT    NOT NULL,
                    from_time       TEXT    NOT NULL,
                    to_time         TEXT    NOT NULL,
                    resolution_secs INTEGER NOT NULL,
                    slippage_pct    REAL    NOT NULL,
                    fee_pct         REAL    NOT NULL,
                    initial_jpy     TEXT    NOT NULL,
                    total_return_pct REAL   NOT NULL,
                    sharpe_ratio    REAL    NOT NULL,
                    max_drawdown_pct REAL   NOT NULL,
                    win_rate        REAL    NOT NULL,
                    total_trades    INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_backtest_runs_created
                    ON backtest_runs(created_at DESC);

                INSERT INTO schema_version(version) VALUES(2);",
            )?;
        }

        if current_version < 3 {
            c.execute_batch(
                "CREATE TABLE IF NOT EXISTS news_sentiments (
                    id          INTEGER PRIMARY KEY AUTOINCREMENT,
                    headline    TEXT    NOT NULL,
                    url         TEXT    NOT NULL DEFAULT '',
                    body        TEXT,
                    score       REAL    NOT NULL,
                    analyzed_at TEXT    NOT NULL,
                    UNIQUE(headline, analyzed_at)
                );
                CREATE INDEX IF NOT EXISTS idx_news_sentiments_analyzed_at
                    ON news_sentiments(analyzed_at DESC);

                INSERT INTO schema_version(version) VALUES(3);",
            )?;
        }

        if current_version < 4 {
            c.execute_batch(
                "ALTER TABLE news_sentiments ADD COLUMN published_at TEXT;
                 INSERT INTO schema_version(version) VALUES(4);",
            )?;
        }

        if current_version < 5 {
            c.execute_batch(
                "CREATE TABLE IF NOT EXISTS mock_balances (
                    currency_code   TEXT    PRIMARY KEY,
                    amount          TEXT    NOT NULL
                );

                CREATE TABLE IF NOT EXISTS mock_state (
                    key             TEXT    PRIMARY KEY,
                    value           TEXT    NOT NULL
                );

                CREATE TABLE IF NOT EXISTS mock_filled_trades (
                    id      INTEGER PRIMARY KEY AUTOINCREMENT,
                    side    TEXT    NOT NULL CHECK(side IN ('BUY','SELL')),
                    price   TEXT    NOT NULL,
                    size    TEXT    NOT NULL,
                    fee     TEXT    NOT NULL
                );

                INSERT INTO schema_version(version) VALUES(5);",
            )?;
        }

        if current_version < 6 {
            c.execute_batch(
                "CREATE TABLE IF NOT EXISTS app_config (
                    key     TEXT PRIMARY KEY,
                    value   TEXT NOT NULL
                );

                INSERT INTO schema_version(version) VALUES(6);",
            )?;
        }

        if current_version < 7 {
            c.execute_batch(
                "CREATE TABLE IF NOT EXISTS tickers (
                    id                INTEGER PRIMARY KEY AUTOINCREMENT,
                    product_code      TEXT NOT NULL,
                    timestamp         TEXT NOT NULL,
                    best_bid          TEXT NOT NULL,
                    best_ask          TEXT NOT NULL,
                    best_bid_size     TEXT NOT NULL,
                    best_ask_size     TEXT NOT NULL,
                    ltp_open          TEXT NOT NULL,
                    ltp               TEXT NOT NULL,
                    ltp_high          TEXT NOT NULL,
                    ltp_low           TEXT NOT NULL,
                    volume            TEXT NOT NULL,
                    volume_by_product TEXT NOT NULL,
                    UNIQUE(product_code, timestamp)
                );
                CREATE INDEX IF NOT EXISTS idx_tickers_product_time
                    ON tickers(product_code, timestamp DESC);

                INSERT INTO schema_version(version) VALUES(7);",
            )?;
        }

        if current_version < 8 {
            // Trading-cost metrics added to BacktestReport (total fees paid,
            // cumulative traded volume, blended effective fee rate, the fee
            // tier reached by the end of the run, and fees as a percentage
            // of starting capital). DEFAULT 0.0 means existing rows read
            // back as 0.0 for these columns rather than NULL.
            c.execute_batch(
                "ALTER TABLE backtest_runs ADD COLUMN total_fees_jpy REAL NOT NULL DEFAULT 0.0;
                 ALTER TABLE backtest_runs ADD COLUMN traded_volume_jpy REAL NOT NULL DEFAULT 0.0;
                 ALTER TABLE backtest_runs ADD COLUMN effective_fee_pct REAL NOT NULL DEFAULT 0.0;
                 ALTER TABLE backtest_runs ADD COLUMN final_fee_tier_pct REAL NOT NULL DEFAULT 0.0;
                 ALTER TABLE backtest_runs ADD COLUMN fee_drag_pct REAL NOT NULL DEFAULT 0.0;

                 INSERT INTO schema_version(version) VALUES(8);",
            )?;
        }

        if current_version < 9 {
            // Buy-and-hold / static-mix benchmark metrics, computed over the
            // same evaluated candle slice as the run itself, so every saved
            // report carries a reported diagnostic of whether the strategy
            // actually beat doing nothing (or doing the static equivalent of
            // its own average exposure). This is measurement only — it does
            // not change the promotion rule in `report::compare`. DEFAULT
            // 0.0 means existing rows read back as 0.0 for these columns.
            c.execute_batch(
                "ALTER TABLE backtest_runs ADD COLUMN avg_btc_exposure REAL NOT NULL DEFAULT 0.0;
                 ALTER TABLE backtest_runs ADD COLUMN hold_return_pct REAL NOT NULL DEFAULT 0.0;
                 ALTER TABLE backtest_runs ADD COLUMN hold_sharpe_ratio REAL NOT NULL DEFAULT 0.0;
                 ALTER TABLE backtest_runs ADD COLUMN hold_max_drawdown_pct REAL NOT NULL DEFAULT 0.0;
                 ALTER TABLE backtest_runs ADD COLUMN static_mix_return_pct REAL NOT NULL DEFAULT 0.0;
                 ALTER TABLE backtest_runs ADD COLUMN static_mix_sharpe_ratio REAL NOT NULL DEFAULT 0.0;
                 ALTER TABLE backtest_runs ADD COLUMN static_mix_max_drawdown_pct REAL NOT NULL DEFAULT 0.0;
                 ALTER TABLE backtest_runs ADD COLUMN excess_return_vs_static_mix_pct REAL NOT NULL DEFAULT 0.0;
                 ALTER TABLE backtest_runs ADD COLUMN sharpe_minus_static_mix REAL NOT NULL DEFAULT 0.0;

                 INSERT INTO schema_version(version) VALUES(9);",
            )?;
        }

        Ok(())
    })
    .await?;
    Ok(())
}
