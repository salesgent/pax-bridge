//! High-level PAX operations — ports the `paxService.js` section of `pax.js`.
//!
//! Turns intent (sale/refund/void/ping/batch) into protocol field arrays,
//! sends them via the live terminal transport (TCP or USB), and parses the
//! response frames into structured, cents-based objects.
//!
//! Concurrency: PAX terminals handle exactly ONE transaction at a time. Each
//! terminal (keyed by `tcp:ip:port` or `usb:/dev/...`) gets a serialized queue
//! so commands never interleave on the wire. `is_busy()` lets callers fail
//! fast with 409 TERMINAL_BUSY instead of queueing.

use crate::bridge::config;
use crate::bridge::db::Terminal;
use crate::bridge::protocol::{self, BatchResponse, CreditFieldsInput, CreditResponse, Field, InitializeInfo, OnState, PaxError};
use crate::bridge::serial::{self, SerialPortInfo};
use crate::bridge::tcp;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Instant;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub enum Target {
    Tcp { ip: String, port: u16 },
    Usb { path: String, baud_rate: u32 },
}

impl Target {
    pub fn from_terminal(t: &Terminal) -> Self {
        if t.conn_type == "usb" {
            Target::Usb { path: t.serial_path.clone(), baud_rate: t.baud_rate }
        } else {
            Target::Tcp { ip: t.ip.clone(), port: t.port }
        }
    }

    pub fn key(&self) -> String {
        match self {
            Target::Tcp { ip, port } => format!("tcp:{}:{}", ip, port),
            Target::Usb { path, .. } => format!("usb:{}", path),
        }
    }
}

struct TerminalQueue {
    lock: Arc<tokio::sync::Mutex<()>>,
    pending: AtomicUsize,
}

/// Handles a raw response that arrived after the caller stopped waiting.
type LateRaw = Box<dyn FnOnce(Result<protocol::ParsedResponse, PaxError>) + Send + 'static>;

/// Handles a credit result that arrived after the POS cancelled the wait —
/// i.e. the customer completed the payment on the terminal anyway.
pub type LateCredit = Box<dyn FnOnce(Result<CreditResponse, PaxError>) + Send + 'static>;

static QUEUES: OnceLock<StdMutex<HashMap<String, Arc<TerminalQueue>>>> = OnceLock::new();

/// Cancellation tokens for commands currently in flight, keyed by terminal.
/// Lets `cancel()` abort a card prompt the cashier no longer wants to wait on.
static CANCELS: OnceLock<StdMutex<HashMap<String, CancellationToken>>> = OnceLock::new();

/// Senders that push a frame into the connection a command is currently on,
/// keyed by terminal. This is how A14 reaches the terminal: BroadPOS services
/// one ECR connection at a time, so a cancel sent on a fresh socket is never
/// read.
static CANCEL_FRAMES: OnceLock<StdMutex<HashMap<String, tokio::sync::mpsc::Sender<Vec<u8>>>>> = OnceLock::new();

fn cancels() -> &'static StdMutex<HashMap<String, CancellationToken>> {
    CANCELS.get_or_init(|| StdMutex::new(HashMap::new()))
}

fn cancel_frames() -> &'static StdMutex<HashMap<String, tokio::sync::mpsc::Sender<Vec<u8>>>> {
    CANCEL_FRAMES.get_or_init(|| StdMutex::new(HashMap::new()))
}

/// Stop waiting on the in-flight command for this terminal, if any.
///
/// This is the POS-side half of a cancel: it releases whoever is awaiting the
/// terminal's reply. It does not touch the terminal — pair it with
/// [`cancel_on_terminal`], which pushes the A14 down the live connection to
/// clear the card prompt. The command keeps running on its socket either way,
/// so whatever the
/// terminal finally reports is handed to the `on_late` hook the caller passed
/// to sale/refund/void, which is what lets an unwanted approval be voided
/// instead of silently charged.
/// Returns false if nothing was in flight.
pub fn cancel(terminal: &Terminal) -> bool {
    let key = Target::from_terminal(terminal).key();
    let guard = cancels().lock().unwrap();
    match guard.get(&key) {
        Some(token) => {
            token.cancel();
            true
        }
        None => false,
    }
}

/// Tell the terminal itself to abort what it is prompting for (A14 CANCEL).
///
/// Sent on its own connection, deliberately bypassing the per-terminal queue:
/// the queue lock is held by the very command we are aborting, so waiting for
/// it would deadlock until the sale timed out — the whole thing we are trying
/// to avoid. The terminal answers A15 and drops back to idle, which also makes
/// the pending T00 come back as a non-approval through the normal path.
///
/// Serial terminals cannot take a second command while one is on the wire (the
/// port is held exclusively), so USB stays a device-side cancel.
pub async fn cancel_on_terminal(terminal: &Terminal) -> Result<(), PaxError> {
    let key = Target::from_terminal(terminal).key();
    let sender = { cancel_frames().lock().unwrap().get(&key).cloned() };
    let sender = sender.ok_or_else(|| {
        PaxError::new("NOT_IN_FLIGHT", "No command is on the wire for this terminal to cancel.")
    })?;

    let fields =
        vec![Field::Single(protocol::COMMAND_CANCEL.to_string()), Field::Single(config::protocol_version())];

    sender.try_send(protocol::build_message(&fields)).map_err(|e| {
        PaxError::new("CANCEL_FAILED", "Could not hand the cancel to the terminal connection.")
            .with_cause(e.to_string())
    })
}

/// Removes this terminal's cancel token when the command finishes, so a later
/// cancel can never abort an unrelated command.
struct CancelGuard(String);
impl Drop for CancelGuard {
    fn drop(&mut self) {
        cancels().lock().unwrap().remove(&self.0);
        cancel_frames().lock().unwrap().remove(&self.0);
    }
}

fn queues() -> &'static StdMutex<HashMap<String, Arc<TerminalQueue>>> {
    QUEUES.get_or_init(|| StdMutex::new(HashMap::new()))
}

fn queue_for(key: &str) -> Arc<TerminalQueue> {
    let mut guard = queues().lock().unwrap();
    guard
        .entry(key.to_string())
        .or_insert_with(|| Arc::new(TerminalQueue { lock: Arc::new(tokio::sync::Mutex::new(())), pending: AtomicUsize::new(0) }))
        .clone()
}

/// True if a command is already in flight/queued for this terminal.
pub fn is_busy(terminal: &Terminal) -> bool {
    let key = Target::from_terminal(terminal).key();
    let guard = queues().lock().unwrap();
    guard.get(&key).map(|q| q.pending.load(Ordering::SeqCst) > 0).unwrap_or(false)
}

/// True if any terminal has a command in flight/queued.
///
/// Used to hold back an automatic update+restart while a card prompt is live.
pub fn any_in_flight() -> bool {
    let guard = queues().lock().unwrap();
    guard.values().any(|q| q.pending.load(Ordering::SeqCst) > 0)
}

struct PendingGuard(Arc<TerminalQueue>);
impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.0.pending.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Send a command to a terminal and await its response frame, serialized
/// per-terminal so only one command is ever in flight on the wire.
async fn send_command(
    terminal: &Terminal,
    fields: Vec<Field>,
    timeout_ms: u64,
    on_state: Option<OnState>,
    on_late: Option<LateRaw>,
) -> Result<protocol::ParsedResponse, PaxError> {
    let target = Target::from_terminal(terminal);
    let key = target.key();
    let q = queue_for(&key);

    q.pending.fetch_add(1, Ordering::SeqCst);
    let pending_guard = PendingGuard(q.clone());

    // Serialize: only one in-flight command at a time per terminal.
    let lock = q.lock.clone().lock_owned().await;

    let command = match fields.first() {
        Some(Field::Single(s)) => s.clone(),
        _ => String::new(),
    };
    let expected = protocol::response_for(&command).map(|s| s.to_string());
    let request = protocol::build_message(&fields);

    // Registered only now that we hold the queue lock, so the token always
    // belongs to the command actually on the wire.
    let token = CancellationToken::new();
    cancels().lock().unwrap().insert(key.clone(), token.clone());
    // Depth 1: one cancel is all a single command can use.
    let (cancel_tx, cancel_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
    cancel_frames().lock().unwrap().insert(key.clone(), cancel_tx);
    let cancel_guard = CancelGuard(key);

    // The wire work owns the guards so it can outlive a cancel: closing the
    // socket would not clear the terminal's card prompt, it would only throw
    // away the result of a card the customer may still tap. The terminal stays
    // marked busy for as long as the command is genuinely on the wire.
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        // Dropped in reverse order: the cancel token is deregistered before the
        // queue lock is released, so the next command can register its own.
        let _pending_guard = pending_guard;
        let _lock = lock;
        let _cancel_guard = cancel_guard;

        let result = match target {
            Target::Tcp { ip, port } => {
                tcp::send_command(&ip, port, &request, expected.as_deref(), timeout_ms, on_state, Some(cancel_rx)).await
            }
            Target::Usb { path, baud_rate } => {
                serial::send_command(&path, baud_rate, request, expected, timeout_ms, on_state, Some(cancel_rx)).await
            }
        };
        let _ = tx.send(result);
    });

    tokio::select! {
        received = &mut rx => {
            return received.unwrap_or_else(|_| Err(PaxError::new("SOCKET_ERROR", "Terminal command ended without a result")));
        }
        _ = token.cancelled() => {}
    }

    if let Some(handler) = on_late {
        // Still racing: a result sent just before the cancel landed is waiting
        // in the channel and reaches the handler immediately.
        tokio::spawn(async move {
            if let Ok(result) = rx.await {
                handler(result);
            }
        });
    }

    Err(PaxError::new("CANCELED", "Cancelled from the point of sale."))
}

/// List serial ports available on the host (for the "detect USB device" UI).
pub async fn list_serial_ports() -> Result<Vec<SerialPortInfo>, PaxError> {
    serial::list_ports().await
}

/// Wraps a credit-result handler so it can consume a raw response frame.
fn late_credit(on_late: Option<LateCredit>) -> Option<LateRaw> {
    on_late.map(|handler| -> LateRaw {
        Box::new(move |raw| handler(raw.map(|parsed| protocol::parse_credit_response(&parsed))))
    })
}

/// A00 initialize / ping. Returns terminal info + latencyMs.
pub async fn initialize(terminal: &Terminal, on_state: Option<OnState>) -> Result<InitializeInfo, PaxError> {
    let started = Instant::now();
    let fields = vec![Field::Single(protocol::COMMAND_INITIALIZE.to_string()), Field::Single(config::protocol_version())];
    let parsed = send_command(terminal, fields, config::ping_timeout_ms(), on_state, None).await?;
    let mut info = protocol::parse_initialize(&parsed);
    info.latency_ms = started.elapsed().as_millis() as i64;
    Ok(info)
}

/// T00 SALE. amountCents/tipCents are integer cents.
pub async fn sale(
    terminal: &Terminal,
    amount_cents: i64,
    ecr_ref_num: String,
    tip_cents: i64,
    on_state: Option<OnState>,
    on_late: Option<LateCredit>,
) -> Result<CreditResponse, PaxError> {
    let fields = protocol::build_credit_fields(CreditFieldsInput {
        txn_type: protocol::TXN_TYPE_SALE,
        amount_cents,
        tip_cents,
        ecr_ref_num,
        cashier_id: String::new(),
        orig_ref_num: None,
        orig_trans_num: None,
    });
    let parsed = send_command(terminal, fields, config::payment_timeout_ms(), on_state, late_credit(on_late)).await?;
    Ok(protocol::parse_credit_response(&parsed))
}

/// T00 RETURN / refund.
pub async fn refund(
    terminal: &Terminal,
    amount_cents: i64,
    ecr_ref_num: String,
    on_state: Option<OnState>,
    on_late: Option<LateCredit>,
) -> Result<CreditResponse, PaxError> {
    let fields = protocol::build_credit_fields(CreditFieldsInput {
        txn_type: protocol::TXN_TYPE_RETURN,
        amount_cents,
        tip_cents: 0,
        ecr_ref_num,
        cashier_id: String::new(),
        orig_ref_num: None,
        orig_trans_num: None,
    });
    let parsed = send_command(terminal, fields, config::payment_timeout_ms(), on_state, late_credit(on_late)).await?;
    Ok(protocol::parse_credit_response(&parsed))
}

/// T00 VOID. Voids a previous transaction by its original ref number.
pub async fn void_transaction(
    terminal: &Terminal,
    orig_ref_num: String,
    ecr_ref_num: String,
    amount_cents: i64,
    orig_trans_num: Option<String>,
    on_state: Option<OnState>,
    on_late: Option<LateCredit>,
) -> Result<CreditResponse, PaxError> {
    let fields = protocol::build_credit_fields(CreditFieldsInput {
        txn_type: protocol::TXN_TYPE_VOID,
        amount_cents,
        tip_cents: 0,
        ecr_ref_num,
        cashier_id: String::new(),
        orig_ref_num: Some(orig_ref_num),
        orig_trans_num,
    });
    let parsed = send_command(terminal, fields, config::payment_timeout_ms(), on_state, late_credit(on_late)).await?;
    Ok(protocol::parse_credit_response(&parsed))
}

/// B00 batch close / settle.
pub async fn batch_close(terminal: &Terminal) -> Result<BatchResponse, PaxError> {
    let fields = vec![
        Field::Single(protocol::COMMAND_BATCH_CLOSE.to_string()),
        Field::Single(config::protocol_version()),
        Field::Single(protocol::EDC_TYPE_ALL.to_string()),
    ];
    let parsed = send_command(terminal, fields, config::payment_timeout_ms(), None, None).await?;
    Ok(protocol::parse_batch_response(&parsed))
}

/// Lightweight LAN/USB diagnostics without sending a full payment. Helps
/// distinguish "device offline" vs "BroadPOS ECR not listening".
pub async fn diagnose(terminal: &Terminal) -> Result<Value, PaxError> {
    let protocol_version = config::protocol_version();

    if terminal.conn_type == "usb" {
        let ports = serial::list_ports().await?;
        let path = terminal.serial_path.clone();
        let matching = ports.iter().find(|p| p.path == path).cloned();
        let port_present = matching.is_some();
        let next_steps: Vec<&str> = if port_present {
            vec![
                "Serial device is present. Run Test Connection.",
                "In BroadPOS: External POS / ECR ON, Communication = USB, leave idle.",
            ]
        } else {
            vec![
                "No matching USB serial device. Set Android USB to PAX POSVCOM USB MODE.",
                "In BroadPOS: External POS / ECR ON, Communication = USB.",
                "Replug the USB cable into this Mac, then Detect again.",
            ]
        };
        return Ok(json!({
            "connType": "usb",
            "protocolVersion": protocol_version,
            "serialPath": path,
            "portPresent": port_present,
            "portsFound": ports.len(),
            "matchingPort": matching,
            "ecrLikelyListening": port_present,
            "nextSteps": next_steps,
        }));
    }

    let ip = terminal.ip.clone();
    let port = if terminal.port != 0 { terminal.port } else { 10009 };
    let (open, host_reachable, error) = tcp::probe(&ip, port, 3000).await;
    let next_steps: Vec<&str> = if open {
        vec!["TCP port is open — BroadPOS ECR appears to be listening. Run Test Connection."]
    } else if host_reachable {
        vec![
            "Device is on the network but port is closed — BroadPOS ECR is not listening.",
            "Open BroadPOS TSYS Sierra → Settings (squares) → password = today's date MMDDYYYY (try ±1 day).",
            "System Settings → ECR-Terminal Integration Mode → External POS.",
            "ECR Comm Settings → Protocol Type = TCP/IP, Host Port = 10009.",
            "Leave BroadPOS on the idle / ready screen, then Test Connection again.",
        ]
    } else {
        vec!["Cannot reach this IP. Confirm the terminal Wi-Fi IP and that the Mac is on the same LAN (AP isolation off)."]
    };

    Ok(json!({
        "connType": "tcp",
        "protocolVersion": protocol_version,
        "ip": ip,
        "port": port,
        "hostReachable": host_reachable,
        "tcpOpen": open,
        "tcpError": error,
        "ecrLikelyListening": open,
        "nextSteps": next_steps,
    }))
}
