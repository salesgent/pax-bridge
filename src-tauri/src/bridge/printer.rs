//! Receipt printing — the transports a browser cannot reach on its own.
//!
//! The ERP builds the receipt and hands us the finished ESC/POS bytes; this
//! module only puts them on a wire. Keeping the split there means a change to a
//! receipt template never requires shipping a new bridge to every till.
//!
//! Four ways to reach a printer, which between them cover what shops actually
//! have:
//!
//!   Network   raw TCP on 9100 — WiFi and Ethernet are the same thing here
//!   Serial    a COM port or tty, which is also how Bluetooth Classic pairs
//!   System    a printer installed in the OS, addressed by name
//!
//! `System` is the important one. Bytes go to the spooler as RAW, so they pass
//! through the printer's own vendor driver untouched — the cash drawer kick
//! included. That is what lets a till print from this bridge *and* from every
//! other application on the machine. The alternative, claiming the USB device
//! directly, requires replacing the printer's driver with WinUSB and breaks
//! every other program that prints to it, so it is deliberately not offered.

use crate::bridge::protocol::PaxError;
use crate::bridge::system_printer;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::Mutex;

/// The port virtually every network receipt printer listens on (JetDirect).
pub const DEFAULT_NETWORK_PORT: u16 = 9100;

/// Epson's factory serial rate. Overridable per printer.
const DEFAULT_BAUD: u32 = 38_400;

const DEFAULT_TIMEOUT_MS: u64 = 15_000;

// ---------------------------------------------------------------------------
// Target
// ---------------------------------------------------------------------------

/// Where to send bytes. Deserialised straight from the request body, with
/// `transport` naming the variant and its fields sitting alongside.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "transport", rename_all = "camelCase")]
pub enum PrintTarget {
    /// WiFi or LAN — to the printer these are the same thing.
    #[serde(rename = "network")]
    Network {
        host: String,
        #[serde(default)]
        port: Option<u16>,
    },
    /// A wired serial printer, or a Bluetooth Classic one, which pairs as a
    /// virtual COM port on Windows and a `/dev/tty.*` device on macOS.
    #[serde(rename = "serial")]
    Serial {
        path: String,
        #[serde(default)]
        baud_rate: Option<u32>,
    },
    /// A printer installed in the OS. Printing this way leaves the vendor
    /// driver in place, so other software keeps working.
    #[serde(rename = "system")]
    System { name: String },
}

impl PrintTarget {
    /// Identifies the physical printer, so two receipts never interleave on it.
    pub fn key(&self) -> String {
        match self {
            PrintTarget::Network { host, port } => {
                format!("network:{}:{}", host, port.unwrap_or(DEFAULT_NETWORK_PORT))
            }
            PrintTarget::Serial { path, .. } => format!("serial:{}", path),
            PrintTarget::System { name } => format!("system:{}", name),
        }
    }
}

impl std::fmt::Display for PrintTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.key())
    }
}

// ---------------------------------------------------------------------------
// ESC/POS odds and ends
//
// The ERP sends a complete receipt, so these exist for the bridge's own test
// page and for callers that would rather ask for a cut or a drawer kick than
// build the bytes themselves.
// ---------------------------------------------------------------------------

/// ESC @ — reset the printer to a known state.
pub fn init_bytes() -> Vec<u8> {
    vec![0x1b, 0x40]
}

/// GS V — full cut, matching what the ERP appends.
pub fn cut_bytes() -> Vec<u8> {
    vec![0x1d, 0x56, 0x41, 0x10]
}

/// ESC p — kick the drawer wired to the printer.
///
/// `pin` 0 is the usual one (PIN 2); 1 is PIN 5. The pulse divisor matches the
/// ERP's, so a drawer tuned against the browser path behaves the same here.
pub fn drawer_bytes(pin: u8, pulse_ms: u32) -> Vec<u8> {
    let m = if pin == 1 { 1 } else { 0 };
    let t = (pulse_ms / 4).clamp(1, 255) as u8;
    vec![0x1b, 0x70, m, t, t]
}

// ---------------------------------------------------------------------------
// One job at a time, per printer
// ---------------------------------------------------------------------------

type LockMap = Mutex<HashMap<String, Arc<Mutex<()>>>>;

fn locks() -> &'static LockMap {
    static LOCKS: OnceLock<LockMap> = OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A printer has one paper path: two receipts sent at once would interleave
/// mid-line. Callers queue behind this rather than failing, because a receipt
/// that waits 200ms is fine and one that prints in pieces is not.
async fn lock_for(key: &str) -> Arc<Mutex<()>> {
    let mut map = locks().lock().await;
    map.entry(key.to_string()).or_insert_with(|| Arc::new(Mutex::new(()))).clone()
}

// ---------------------------------------------------------------------------
// The job
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PrintJob {
    pub data: Vec<u8>,
    pub copies: u16,
    pub cut: bool,
    pub open_drawer: bool,
    pub drawer_pin: u8,
    pub drawer_pulse: u32,
    pub timeout_ms: u64,
}

impl Default for PrintJob {
    fn default() -> Self {
        Self {
            data: Vec::new(),
            copies: 1,
            cut: false,
            open_drawer: false,
            drawer_pin: 0,
            drawer_pulse: 200,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }
}

impl PrintJob {
    /// The exact bytes to put on the wire, copies and trailers included.
    fn payload(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.data.len() * self.copies.max(1) as usize + 8);
        for index in 0..self.copies.max(1) {
            out.extend_from_slice(&self.data);
            if self.cut {
                out.extend_from_slice(&cut_bytes());
            }
            // The drawer opens once, with the last copy — a customer copy and a
            // merchant copy are one sale, not two.
            if self.open_drawer && index + 1 == self.copies.max(1) {
                out.extend_from_slice(&drawer_bytes(self.drawer_pin, self.drawer_pulse));
            }
        }
        out
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrintOutcome {
    pub target: String,
    pub bytes: usize,
}

/// Send a job to a printer, waiting for any job already on it to finish.
pub async fn print(target: &PrintTarget, job: &PrintJob) -> Result<PrintOutcome, PaxError> {
    let payload = job.payload();
    if payload.is_empty() {
        return Err(PaxError::new("PRINT_EMPTY", "Nothing to print — the job had no data."));
    }

    let key = target.key();
    let lock = lock_for(&key).await;
    let _held = lock.lock().await;

    let fut = send(target, payload.clone());
    match tokio::time::timeout(Duration::from_millis(job.timeout_ms), fut).await {
        Ok(result) => result?,
        Err(_) => {
            return Err(PaxError::new(
                "PRINTER_TIMEOUT",
                format!("The printer did not accept the receipt within {}ms.", job.timeout_ms),
            ))
        }
    }

    Ok(PrintOutcome { target: key, bytes: payload.len() })
}

async fn send(target: &PrintTarget, payload: Vec<u8>) -> Result<(), PaxError> {
    match target {
        PrintTarget::Network { host, port } => {
            send_network(host, port.unwrap_or(DEFAULT_NETWORK_PORT), payload).await
        }
        PrintTarget::Serial { path, baud_rate } => {
            send_serial(path.clone(), baud_rate.unwrap_or(DEFAULT_BAUD), payload).await
        }
        PrintTarget::System { name } => system_printer::print_raw(name.clone(), payload).await,
    }
}

// ---------------------------------------------------------------------------
// Network — WiFi and LAN
// ---------------------------------------------------------------------------

async fn send_network(host: &str, port: u16, payload: Vec<u8>) -> Result<(), PaxError> {
    let mut stream = TcpStream::connect((host, port)).await.map_err(|e| network_error(host, port, &e))?;
    // Receipt printers do not answer on 9100; the write is the whole exchange.
    stream.write_all(&payload).await.map_err(|e| {
        PaxError::new("PRINTER_WRITE_FAILED", format!("Sent to {}:{} but the printer dropped it: {}", host, port, e))
    })?;
    stream.flush().await.ok();
    Ok(())
}

fn network_error(host: &str, port: u16, err: &std::io::Error) -> PaxError {
    let (code, message) = match err.kind() {
        std::io::ErrorKind::ConnectionRefused => (
            "PRINTER_REFUSED",
            format!("{}:{} refused the connection. The address is right but nothing is listening — check the printer is on and that port {} is its raw printing port.", host, port, port),
        ),
        std::io::ErrorKind::TimedOut => (
            "PRINTER_UNREACHABLE",
            format!("{}:{} did not answer. The printer may be off, asleep, or on a different network.", host, port),
        ),
        std::io::ErrorKind::HostUnreachable | std::io::ErrorKind::NetworkUnreachable => (
            "PRINTER_UNREACHABLE",
            format!("{} is unreachable from this computer — check they are on the same network.", host),
        ),
        _ => ("PRINTER_UNREACHABLE", format!("Could not reach {}:{} — {}", host, port, err)),
    };
    PaxError::new(code, message).with_cause(err.to_string())
}

// ---------------------------------------------------------------------------
// Serial — wired, and Bluetooth Classic
// ---------------------------------------------------------------------------

async fn send_serial(path: String, baud: u32, payload: Vec<u8>) -> Result<(), PaxError> {
    // The serialport crate is blocking, so it runs off the async runtime.
    tokio::task::spawn_blocking(move || {
        let mut port = serialport::new(&path, baud)
            .timeout(Duration::from_millis(5_000))
            .open()
            .map_err(|e| {
                PaxError::new(
                    "PRINTER_UNREACHABLE",
                    format!("Could not open {} — check the printer is connected and no other program is holding the port: {}", path, e),
                )
            })?;

        port.write_all(&payload)
            .map_err(|e| PaxError::new("PRINTER_WRITE_FAILED", format!("Failed writing to {}: {}", path, e)))?;
        port.flush()
            .map_err(|e| PaxError::new("PRINTER_WRITE_FAILED", format!("Failed flushing {}: {}", path, e)))?;
        Ok::<(), PaxError>(())
    })
    .await
    .map_err(|e| PaxError::new("PRINTER_WRITE_FAILED", format!("Serial task failed: {}", e)))?
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredPrinters {
    /// Printers installed in the OS, by name.
    pub system: Vec<system_printer::SystemPrinter>,
    /// Serial ports, which is where wired and Bluetooth Classic printers show.
    pub serial: Vec<crate::bridge::serial::SerialPortInfo>,
    pub default_network_port: u16,
}

/// Everything this computer can print to, for the ERP's printer picker.
/// A failure to enumerate one kind does not hide the other.
pub async fn discover() -> DiscoveredPrinters {
    let system = system_printer::list().await.unwrap_or_default();
    let serial = crate::bridge::serial::list_ports().await.unwrap_or_default();
    DiscoveredPrinters { system, serial, default_network_port: DEFAULT_NETWORK_PORT }
}

// ---------------------------------------------------------------------------
// Test page
// ---------------------------------------------------------------------------

/// A receipt the bridge can print by itself, so a till can be proved working
/// before the ERP is involved.
pub fn test_receipt() -> Vec<u8> {
    let mut out = init_bytes();
    out.extend_from_slice(&[0x1b, 0x61, 0x01]); // centre
    out.extend_from_slice(&[0x1d, 0x21, 0x11]); // double size
    out.extend_from_slice(b"SALESGENT\n");
    out.extend_from_slice(&[0x1d, 0x21, 0x00]); // normal size
    out.extend_from_slice(b"Bridge test receipt\n");
    out.extend_from_slice(&[0x1b, 0x61, 0x00]); // left
    out.extend_from_slice(b"\n");
    out.extend_from_slice(b"If you can read this, the printer\n");
    out.extend_from_slice(b"is reachable and the bytes arrived\n");
    out.extend_from_slice(b"in one piece.\n\n");
    out
}
