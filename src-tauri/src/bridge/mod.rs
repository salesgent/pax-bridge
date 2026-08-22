//! PAX Bridge — Rust/Tauri port of `bridge/index.js` + `bridge/pax.js`.
//! Local HTTP/WS server for live BroadPOS terminals. No mock/emulator.

pub mod config;
pub mod db;
pub mod http;
pub mod printer;
pub mod protocol;
pub mod serial;
pub mod server;
pub mod system_printer;
pub mod tcp;
pub mod transport;
pub mod ws;

use std::sync::Arc;
use tokio::sync::Mutex;

/// Shared application state handed to every axum handler.
#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Mutex<db::Db>>,
    pub ws: Arc<ws::Broadcaster>,
}

pub use server::start_bridge;
