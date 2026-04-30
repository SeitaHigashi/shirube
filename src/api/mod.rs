pub mod routes;
pub mod server;
pub mod ws_handler;

use std::sync::Arc;

use tokio::sync::broadcast;

use crate::exchange::ExchangeClient;
use crate::signal::Signal;
use crate::storage::db::Database;
use crate::types::market::Candle;

#[derive(Clone)]
pub struct AppState {
    pub db: Database,
    pub exchange: Arc<dyn ExchangeClient>,
    pub candle_tx: broadcast::Sender<Candle>,
    pub signal_tx: broadcast::Sender<Signal>,
}
