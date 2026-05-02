//! candles テーブルの 1 分足データを tickers テーブルへ移行するワンショットツール。
//!
//! 使い方:
//!   cargo run --bin migrate_candles_to_tickers -- [--db shirube.db] [--product BTC_JPY]

use std::env;
use rusqlite::{Connection, params};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().collect();
    let db_path = flag_value(&args, "--db").unwrap_or_else(|| "shirube.db".to_string());
    let product = flag_value(&args, "--product").unwrap_or_else(|| "BTC_JPY".to_string());

    println!("DB: {db_path}");
    println!("product_code: {product}");

    let conn = Connection::open(&db_path)?;

    // WAL モードを有効化（本体と同じ設定）
    conn.execute_batch("PRAGMA journal_mode=WAL;")?;

    // 対象件数を確認
    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM candles WHERE product_code = ?1 AND resolution_secs = 60",
        params![product],
        |r| r.get(0),
    )?;
    println!("candles (1min) to migrate: {total}");

    // 既存 tickers 件数
    let before: i64 = conn.query_row(
        "SELECT COUNT(*) FROM tickers WHERE product_code = ?1",
        params![product],
        |r| r.get(0),
    )?;
    println!("tickers before migration: {before}");

    // 一括マイグレーション
    // best_bid / best_ask は close で近似、サイズは 0、volume_by_product は volume で代替
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO tickers
             (product_code, timestamp,
              best_bid, best_ask, best_bid_size, best_ask_size,
              ltp_open, ltp, ltp_high, ltp_low,
              volume, volume_by_product)
         SELECT
             product_code,
             open_time,       -- timestamp
             close,           -- best_bid (近似)
             close,           -- best_ask (近似)
             '0',             -- best_bid_size
             '0',             -- best_ask_size
             open,            -- ltp_open
             close,           -- ltp (終値)
             high,            -- ltp_high
             low,             -- ltp_low
             volume,          -- volume
             volume           -- volume_by_product (近似)
         FROM candles
         WHERE product_code = ?1
           AND resolution_secs = 60",
        params![product],
    )?;

    let after: i64 = conn.query_row(
        "SELECT COUNT(*) FROM tickers WHERE product_code = ?1",
        params![product],
        |r| r.get(0),
    )?;

    println!("Inserted: {inserted} rows");
    println!("tickers after migration: {after}");
    println!("Done.");
    Ok(())
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2)
        .find(|w| w[0] == flag)
        .map(|w| w[1].clone())
}
