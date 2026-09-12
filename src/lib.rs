//! shirube - bitFlyer BTC/JPY automated trading bot.
//!
//! This library target exposes the crate's modules so that both the `shirube`
//! binary and the integration tests under `tests/` can use the same public API.
//! Unit tests live next to the code they cover (`#[cfg(test)] mod tests`);
//! integration tests exercise these modules through this public surface only.

pub mod api;
pub mod backtest;
pub mod cli;
pub mod config;
pub mod error;
pub mod exchange;
pub mod http;
pub mod market;
pub mod news;
pub mod risk;
pub mod signal;
pub mod storage;
pub mod sync_ext;
pub mod trading;
pub mod types;
pub mod updater;
