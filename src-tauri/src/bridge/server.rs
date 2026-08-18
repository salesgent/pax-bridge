//! Bridge server bootstrap — ports the bottom section of `bridge/index.js`:
//! load env/config, load the DB, wire CORS + routes, bind and listen.

use crate::bridge::db::Db;
use crate::bridge::ws::Broadcaster;
use crate::bridge::{config, http, AppState};
use std::future::Future;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Start the PAX bridge HTTP/WS server. Binds `0.0.0.0:<PORT>` (default 5000)
/// and stores its JSON database under `PAX_HOME`/`PAX_DATA_DIR` (see `config`).
/// Runs until `shutdown` resolves (graceful shutdown).
pub async fn start_bridge(shutdown: impl Future<Output = ()> + Send + 'static) -> anyhow::Result<()> {
    let setup = config::load_env();
    tracing::info!("[setup] runtime dir: {}", setup.dir.display());

    let data_dir = config::resolve_data_dir();
    let db = Db::load(&data_dir);

    let state = AppState { db: Arc::new(Mutex::new(db)), ws: Arc::new(Broadcaster::new()) };

    let app = http::router(state);

    let port = config::port();
    let addr = format!("0.0.0.0:{}", port);
    // A bare "Address already in use (os error 48)" tells a cashier nothing.
    // Say which port and what to do about it.
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
            return Err(anyhow::anyhow!(
                "Port {port} is already in use by another program — most often a second copy of this app that is still running. Quit it (or pick a different port in Settings), then press Start."
            ));
        }
        Err(err) => return Err(err.into()),
    };

    tracing::info!("Server listening on http://localhost:{}", port);
    tracing::info!("WebSocket lifecycle events on ws://localhost:{}/ws", port);
    tracing::info!("PAX mode: live terminal");

    axum::serve(listener, app.into_make_service())
        .with_graceful_shutdown(shutdown)
        .await?;

    Ok(())
}
