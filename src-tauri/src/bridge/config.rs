//! Runtime configuration: env vars, PAX_HOME/PAX_DATA_DIR resolution, defaults.
//!
//! Mirrors `bridge/index.js`'s `runtimeDir()` / `ensureRuntimeSetup()` /
//! `resolveDataDir()` — PAX_HOME (installer-provided, writable) always wins,
//! otherwise the exe folder when packaged, or the crate root in dev.

use std::path::{Path, PathBuf};

pub const DEFAULT_PORT: u16 = 5000;

const DEFAULT_ENV: &str = "# Auto-created on first run — live BroadPOS / TSYS Sierra mode.\nPORT=5000\nPAX_PING_TIMEOUT_MS=10000\nPAX_PAYMENT_TIMEOUT_MS=120000\n";

/// Directory where config + data live in dev (no packaged exe yet).
fn dev_runtime_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Base runtime directory: PAX_HOME wins, else exe folder (release), else crate root (dev).
pub fn runtime_dir() -> PathBuf {
    if let Ok(home) = std::env::var("PAX_HOME") {
        if !home.trim().is_empty() {
            return PathBuf::from(home);
        }
    }
    if !cfg!(debug_assertions) {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(parent) = exe.parent() {
                return parent.to_path_buf();
            }
        }
    }
    dev_runtime_dir()
}

pub struct RuntimeSetup {
    pub dir: PathBuf,
    pub env_path: PathBuf,
    pub data_dir: PathBuf,
    pub created_env: bool,
}

/// Create data/ + .env with production-safe defaults if missing. Degrades to
/// built-in defaults (never crashes) if the folder is read-only.
pub fn ensure_runtime_setup() -> RuntimeSetup {
    let dir = runtime_dir();
    let env_path = dir.join(".env");
    let data_dir = dir.join("data");
    let mut created_env = false;

    if !data_dir.exists() {
        if let Err(err) = std::fs::create_dir_all(&data_dir) {
            tracing::error!("[setup] Could not create data folder {}: {}", data_dir.display(), err);
        } else {
            tracing::info!("[setup] Created data folder: {}", data_dir.display());
        }
    }

    if !env_path.exists() {
        match std::fs::write(&env_path, DEFAULT_ENV) {
            Ok(_) => {
                created_env = true;
                tracing::info!("[setup] Created default config: {}", env_path.display());
            }
            Err(err) => {
                tracing::error!(
                    "[setup] Could not write to {} ({}). Using defaults; set PAX_HOME to a writable folder to persist config.",
                    dir.display(),
                    err
                );
            }
        }
    }

    RuntimeSetup { dir, env_path, data_dir, created_env }
}

/// Where to store the JSON database. PAX_DATA_DIR > PAX_HOME/data > exe-dir/data (release) > crate-root/data (dev).
pub fn resolve_data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("PAX_DATA_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    if let Ok(home) = std::env::var("PAX_HOME") {
        if !home.trim().is_empty() {
            return PathBuf::from(home).join("data");
        }
    }
    if !cfg!(debug_assertions) {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(parent) = exe.parent() {
                return parent.join("data");
            }
        }
    }
    dev_runtime_dir().join("data")
}

/// Load .env from the runtime dir (if present) into the process environment.
pub fn load_env() -> RuntimeSetup {
    let setup = ensure_runtime_setup();
    if let Err(err) = dotenvy::from_path(&setup.env_path) {
        tracing::debug!("[setup] no .env loaded from {}: {}", setup.env_path.display(), err);
    }
    setup
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(default)
}

pub fn port() -> u16 {
    std::env::var("PORT").ok().and_then(|v| v.parse::<u16>().ok()).unwrap_or(DEFAULT_PORT)
}

pub fn ping_timeout_ms() -> u64 {
    env_u64("PAX_PING_TIMEOUT_MS", 10_000)
}

pub fn payment_timeout_ms() -> u64 {
    env_u64("PAX_PAYMENT_TIMEOUT_MS", 120_000)
}

/// Protocol version string sent as field #2 of most commands.
/// EMPIRICAL: BroadPOS TSYS Sierra (A35 / A920 Pro) responds with "1.54" in A01.
pub fn protocol_version() -> String {
    std::env::var("PAX_PROTOCOL_VERSION").unwrap_or_else(|_| "1.54".to_string())
}

/// DoCredit transaction type used for a sale.
///
/// "01" is SALE and "02" is RETURN in every PAX implementation we could check,
/// and a terminal that cannot reach its host will happily approve "02" offline
/// while refusing "01" — which makes a refund look like a working sale. The
/// override exists so the two can be compared on the terminal's own screen
/// without a rebuild: set PAX_SALE_TXN_TYPE and read what the terminal says.
pub fn sale_txn_type() -> String {
    std::env::var("PAX_SALE_TXN_TYPE")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| crate::bridge::protocol::TXN_TYPE_SALE.to_string())
}

/// Optional ENQ handshake for BroadPOS builds that require it.
pub fn send_enq() -> bool {
    std::env::var("PAX_SEND_ENQ")
        .map(|v| v.trim().eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

#[allow(dead_code)]
pub fn is_dir_writable(dir: &Path) -> bool {
    std::fs::metadata(dir).map(|m| !m.permissions().readonly()).unwrap_or(false)
}
