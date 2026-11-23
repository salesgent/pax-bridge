//! Valor PayTech driver — VP100 (Newland SP880), VL100, VL110, VP300, VP500, VL500, VL550.
//!
//! Supports three connection modes:
//!   1. `tcp`   — Valor Connect Local TCP/IP (default port 5000 for txns, port 5001 for cancel).
//!                Wire framing: `<STX>{JSON}<ETX>` (0x02 ... 0x03).
//!                Parser tolerates both STX/ETX-framed JSON and bare JSON responses.
//!   2. `usb`   — Valor Connect USB serial (same `<STX>{JSON}<ETX>` framing).
//!   3. `cloud` — Valor Connect Cloud / SecureLink wrapper (`vc_publish`, using `epi`, `appId`, `appKey`, `channelId`).
//!
//! All amounts are integer cents sent as stringified cents (e.g. `"100"` = $1.00).

use crate::bridge::config;
use crate::bridge::db::Terminal;
use crate::bridge::protocol::{fire_state, BatchResponse, CreditResponse, InitializeInfo, OnState, PaxError, ETX, STX};
use crate::bridge::tcp;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub const DEFAULT_PORT: u16 = 5000;
pub const DEFAULT_CANCEL_PORT: u16 = 5001;
const PROD_CLOUD_URL: &str = "https://securelink.valorpaytech.com:4430/?status";
const SANDBOX_CLOUD_URL: &str = "https://securelink-staging.valorpaytech.com:4430/?status";

/// Active REQ_TXN_ID per terminal key so out-of-band cancel on port 5001
/// knows which transaction to abort.
static ACTIVE_REQ_IDS: OnceLock<StdMutex<HashMap<String, String>>> = OnceLock::new();

fn active_req_ids() -> &'static StdMutex<HashMap<String, String>> {
    ACTIVE_REQ_IDS.get_or_init(|| StdMutex::new(HashMap::new()))
}

struct ActiveReqGuard(String);
impl Drop for ActiveReqGuard {
    fn drop(&mut self) {
        active_req_ids().lock().unwrap().remove(&self.0);
    }
}

pub fn terminal_key(terminal: &Terminal) -> String {
    match terminal.conn_type.as_str() {
        "usb" => format!("valor:usb:{}", terminal.serial_path),
        "cloud" => format!("valor:cloud:{}", terminal.config_str("epi")),
        _ => format!("valor:tcp:{}:{}", terminal.ip, effective_port(terminal)),
    }
}

pub fn effective_port(terminal: &Terminal) -> u16 {
    if terminal.port != 0 { terminal.port } else { DEFAULT_PORT }
}

pub fn effective_cancel_port(terminal: &Terminal) -> u16 {
    terminal.config_u16("cancelPort").unwrap_or(DEFAULT_CANCEL_PORT)
}

// ---------------------------------------------------------------------------
// Framing & payload builders
// ---------------------------------------------------------------------------

/// Frame a JSON payload with `<STX>{JSON}<ETX>`.
pub fn build_frame(payload: &Value) -> Vec<u8> {
    let body = serde_json::to_string(payload).unwrap_or_else(|_| "{}".to_string());
    let mut out = Vec::with_capacity(body.len() + 2);
    out.push(STX);
    out.extend_from_slice(body.as_bytes());
    out.push(ETX);
    out
}

/// Extract a complete JSON object from either `<STX>{...}<ETX>` or bare `{...}`.
/// Returns `Ok(Some(value))` when a complete frame is parsed, `Ok(None)` if
/// more bytes are still arriving, or `Err` if malformed.
pub fn try_extract_json(buf: &[u8]) -> Result<Option<Value>, PaxError> {
    if buf.is_empty() {
        return Ok(None);
    }

    // Case 1: STX ... ETX frame
    if let Some(stx_pos) = buf.iter().position(|&b| b == STX) {
        if let Some(etx_rel) = buf[stx_pos + 1..].iter().position(|&b| b == ETX) {
            let slice = &buf[stx_pos + 1..stx_pos + 1 + etx_rel];
            let text = String::from_utf8_lossy(slice);
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return Err(PaxError::new("MALFORMED_RESPONSE", "Valor terminal returned an empty STX/ETX frame"));
            }
            let val: Value = serde_json::from_str(trimmed).map_err(|e| {
                PaxError::new("MALFORMED_RESPONSE", format!("Could not parse Valor JSON response: {}", e))
            })?;
            return Ok(Some(val));
        }
        return Ok(None); // still receiving until ETX
    }

    // Case 2: Bare JSON (some Valor firmware versions omit STX/ETX on TCP)
    let text = String::from_utf8_lossy(buf);
    let trimmed = text.trim_matches(|c: char| c.is_whitespace() || c == '\x06' || c == '\x00');
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
            return Ok(Some(val));
        }
    }
    Ok(None)
}

pub fn build_sale_payload(amount_cents: i64, tip_cents: i64, req_txn_id: &str) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("TRAN_MODE".into(), json!("1"));
    obj.insert("TRAN_CODE".into(), json!("1"));
    obj.insert("AMOUNT".into(), json!(amount_cents.to_string()));
    obj.insert("REQ_TXN_ID".into(), json!(req_txn_id));
    if tip_cents > 0 {
        obj.insert("TIP_ENTRY".into(), json!("0"));
        obj.insert("TIP_AMOUNT".into(), json!(tip_cents.to_string()));
    } else {
        obj.insert("TIP_ENTRY".into(), json!("0"));
    }
    Value::Object(obj)
}

pub fn build_refund_payload(amount_cents: i64, req_txn_id: &str) -> Value {
    json!({
        "TRAN_MODE": "1",
        "TRAN_CODE": "5",
        "AMOUNT": amount_cents.to_string(),
        "REQ_TXN_ID": req_txn_id,
    })
}

pub fn build_void_payload(orig_ref: &str, amount_cents: i64, req_txn_id: &str) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("TRAN_MODE".into(), json!("0"));
    obj.insert("TRAN_CODE".into(), json!("2"));
    obj.insert("REQ_TXN_ID".into(), json!(req_txn_id));
    if amount_cents > 0 {
        obj.insert("AMOUNT".into(), json!(amount_cents.to_string()));
    }
    if !orig_ref.is_empty() {
        obj.insert("RRN".into(), json!(orig_ref));
        obj.insert("TXN_ID".into(), json!(orig_ref));
    }
    Value::Object(obj)
}

pub fn build_cancel_payload(req_txn_id: &str) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("TRAN_MODE".into(), json!("99"));
    if !req_txn_id.is_empty() {
        obj.insert("REQ_TXN_ID".into(), json!(req_txn_id));
    }
    Value::Object(obj)
}

pub fn build_batch_close_payload(req_txn_id: &str) -> Value {
    json!({
        "TRAN_MODE": "1",
        "TRAN_CODE": "SETTLEMENT",
        "REQ_TXN_ID": req_txn_id,
    })
}

// ---------------------------------------------------------------------------
// Response parser
// ---------------------------------------------------------------------------

fn first_str(obj: &Value, keys: &[&str]) -> String {
    for &k in keys {
        if let Some(v) = obj.get(k) {
            match v {
                Value::String(s) if !s.trim().is_empty() => return s.trim().to_string(),
                Value::Number(n) => return n.to_string(),
                _ => {}
            }
        }
    }
    String::new()
}

fn parse_amount_to_cents(raw: &str, fallback_cents: i64) -> i64 {
    let s = raw.trim();
    if s.is_empty() {
        return fallback_cents;
    }
    if s.contains('.') {
        if let Ok(f) = s.parse::<f64>() {
            return (f * 100.0).round() as i64;
        }
    }
    let digits: String = s.chars().filter(|c| c.is_ascii_digit() || *c == '-').collect();
    digits.parse::<i64>().unwrap_or(fallback_cents)
}

/// Parse a Valor terminal or Valor Connect Cloud JSON response into our unified CreditResponse.
pub fn parse_credit_response(raw: &Value, requested_cents: i64, ecr_ref_num: &str) -> CreditResponse {
    // Cloud wrapper nests details inside `"response": { ... }`, while local TCP returns flat keys.
    let inner = raw.get("response").filter(|v| v.is_object()).unwrap_or(raw);

    let state = first_str(inner, &["STATE", "state"]);
    let error_code = {
        let c = first_str(inner, &["ERROR_CODE", "error_code", "CODE", "code"]);
        if !c.is_empty() { c } else { first_str(raw, &["ERROR_CODE", "error_code", "error_no"]) }
    };
    let error_msg = {
        let m = first_str(inner, &["ERROR_MSG", "error_msg", "MSG", "msg", "DESC", "desc"]);
        if !m.is_empty() { m } else { first_str(raw, &["ERROR_MSG", "error_msg", "msg", "desc"]) }
    };
    let auth_code = first_str(inner, &["AUTH_CODE", "APPROVAL_CODE", "auth_code", "approval_code"]);
    let rrn = first_str(inner, &["RRN", "rrn", "HOST_REF_NUM"]);
    let txn_id = first_str(inner, &["TXN_ID", "txn_id", "STAN", "stan", "REF_TXN_ID"]);
    let masked_pan = first_str(inner, &["MASKED_PAN", "masked_pan", "ACCOUNT", "PAN"]);
    let card_type = first_str(inner, &["CARD_LABEL", "CARD_TYPE", "ISSUER", "card_label", "card_type"]);
    let entry_mode = first_str(inner, &["TRAN_METHOD", "ENTRY_MODE", "entry_mode"]);
    let timestamp = first_str(inner, &["DATE", "TIMESTAMP", "date", "timestamp"]);
    let amount_str = first_str(inner, &["APPROVED_AMOUNT", "AMOUNT", "ORIGINAL_AMOUNT", "amount"]);
    let tip_str = first_str(inner, &["TIP_AMOUNT", "tip_amount"]);
    let wrapper_ok = first_str(raw, &["error_no"]).eq_ignore_ascii_case("S00");

    let approved_flag = state == "0"
        || error_code == "00"
        || error_code == "000000"
        || (wrapper_ok && (state.is_empty() || state == "0"))
        || error_msg.eq_ignore_ascii_case("APPROVED");

    // Same safety invariant as PAX: an approval with no auth code could be offline/SAF or malformed.
    let has_auth = !auth_code.is_empty();
    let offline_approval = approved_flag && !has_auth && requested_cents > 0 && state == "0";
    let approved = approved_flag && has_auth;

    let last4: String = {
        let digits: Vec<char> = masked_pan.chars().filter(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            String::new()
        } else {
            let start = digits.len().saturating_sub(4);
            digits[start..].iter().collect()
        }
    };

    let ref_num = if !rrn.is_empty() { rrn.clone() } else { txn_id.clone() };
    let result_code = if !error_code.is_empty() {
        error_code.clone()
    } else if approved {
        "00".to_string()
    } else if !state.is_empty() {
        state.clone()
    } else {
        "DECLINED".to_string()
    };
    let result_txt = if !error_msg.is_empty() {
        error_msg.clone()
    } else if approved {
        "APPROVED".to_string()
    } else {
        "DECLINED".to_string()
    };

    CreditResponse {
        result_code,
        result_txt: result_txt.clone(),
        host_response_code: error_code,
        host_response_text: if offline_approval {
            "Offline / unconfirmed approval — no authorization code returned".to_string()
        } else {
            result_txt
        },
        approved,
        offline_approval,
        auth_code,
        host_ref_num: rrn,
        ref_num,
        transaction_num: txn_id,
        ecr_ref_num: {
            let echoed = first_str(inner, &["REQ_TXN_ID", "MER_TXN_ID"]);
            if !echoed.is_empty() { echoed } else { ecr_ref_num.to_string() }
        },
        masked_pan,
        last4,
        card_type,
        entry_mode,
        approved_amount_cents: if approved { parse_amount_to_cents(&amount_str, requested_cents) } else { 0 },
        tip_amount_cents: parse_amount_to_cents(&tip_str, 0),
        timestamp,
        raw: raw.clone(),
    }
}

// ---------------------------------------------------------------------------
// Wire transports (TCP / USB / Cloud)
// ---------------------------------------------------------------------------

async fn send_tcp_json(ip: &str, port: u16, payload: &Value, timeout_ms: u64, on_state: Option<OnState>) -> Result<Value, PaxError> {
    let where_str = format!("{}:{}", ip, port);
    let frame = build_frame(payload);
    fire_state(&on_state, "SENDING");

    let fut = async {
        let mut stream = TcpStream::connect((ip, port)).await.map_err(|e| map_tcp_err(&where_str, &e))?;
        let _ = stream.set_nodelay(true);
        tracing::info!("[valor] -> {} ({} bytes): {}", where_str, frame.len(), payload);

        stream.write_all(&frame).await.map_err(|e| {
            PaxError::new("WRITE_FAILED", format!("Failed to write to Valor terminal {}: {}", where_str, e))
        })?;
        stream.flush().await.ok();
        fire_state(&on_state, "WAITING");

        let mut buf: Vec<u8> = Vec::with_capacity(1024);
        let mut chunk = [0u8; 4096];
        let mut first_byte = false;

        loop {
            let n = stream.read(&mut chunk).await.map_err(|e| map_tcp_err(&where_str, &e))?;
            if n == 0 {
                if let Some(parsed) = try_extract_json(&buf)? {
                    tracing::info!("[valor] <- {} JSON: {}", where_str, parsed);
                    return Ok(parsed);
                }
                return Err(PaxError::new("CONNECTION_CLOSED", "Valor terminal closed the connection before responding"));
            }
            if !first_byte {
                first_byte = true;
                fire_state(&on_state, "RECEIVING");
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(parsed) = try_extract_json(&buf)? {
                tracing::info!("[valor] <- {} JSON: {}", where_str, parsed);
                return Ok(parsed);
            }
        }
    };

    tokio::time::timeout(Duration::from_millis(timeout_ms), fut)
        .await
        .map_err(|_| PaxError::new("TIMEOUT", format!("No response from Valor terminal {} within {}ms", where_str, timeout_ms)))?
}

async fn send_usb_json(path: String, baud_rate: u32, payload: Value, timeout_ms: u64, on_state: Option<OnState>) -> Result<Value, PaxError> {
    let frame = build_frame(&payload);
    fire_state(&on_state, "SENDING");

    tokio::task::spawn_blocking(move || {
        let mut port = serialport::new(&path, baud_rate)
            .timeout(Duration::from_millis(250))
            .open()
            .map_err(|e| PaxError::new("SERIAL_OPEN_FAILED", format!("Cannot open Valor USB port {}: {}", path, e)))?;

        port.write_all(&frame)
            .map_err(|e| PaxError::new("WRITE_FAILED", format!("Failed writing to {}: {}", path, e)))?;
        port.flush().ok();
        fire_state(&on_state, "WAITING");

        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut buf = Vec::with_capacity(1024);
        let mut chunk = [0u8; 1024];
        let mut first_byte = false;

        while Instant::now() < deadline {
            match port.read(&mut chunk) {
                Ok(n) if n > 0 => {
                    if !first_byte {
                        first_byte = true;
                        fire_state(&on_state, "RECEIVING");
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(parsed) = try_extract_json(&buf)? {
                        return Ok(parsed);
                    }
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
                Err(e) => return Err(PaxError::new("SERIAL_ERROR", format!("Valor serial error on {}: {}", path, e))),
            }
        }
        Err(PaxError::new("TIMEOUT", format!("No response from Valor USB {} within {}ms", path, timeout_ms)))
    })
    .await
    .map_err(|e| PaxError::new("SERIAL_ERROR", format!("Valor serial task panicked: {}", e)))?
}

async fn send_cloud_json(terminal: &Terminal, payload: &Value, timeout_ms: u64, on_state: Option<OnState>) -> Result<Value, PaxError> {
    let epi = terminal.config_str("epi");
    let app_id = terminal.config_str("appId");
    let app_key = terminal.config_str("appKey");
    let channel_id = terminal.config_str("channelId");

    if epi.is_empty() || app_id.is_empty() || app_key.is_empty() {
        return Err(PaxError::new(
            "VALOR_CONFIG_MISSING",
            "Valor Connect Cloud requires EPI, App ID, and App Key in terminal settings.",
        ));
    }

    let url = {
        let custom = terminal.config_str("apiUrl");
        if !custom.is_empty() {
            custom
        } else if terminal.config_str("environment").eq_ignore_ascii_case("sandbox") {
            SANDBOX_CLOUD_URL.to_string()
        } else {
            PROD_CLOUD_URL.to_string()
        }
    };

    let envelope = json!({
        "appid": app_id,
        "appkey": app_key,
        "epi": epi,
        "channel_id": channel_id,
        "txn_type": "vc_publish",
        "version": "1",
        "payload": payload,
    });

    fire_state(&on_state, "SENDING");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .build()
        .map_err(|e| PaxError::new("SOCKET_ERROR", format!("HTTP client error: {}", e)))?;

    fire_state(&on_state, "WAITING");
    let resp = client
        .post(&url)
        .json(&envelope)
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                PaxError::new("TIMEOUT", format!("Valor Cloud request timed out after {}ms", timeout_ms))
            } else {
                PaxError::new("HOST_UNREACHABLE", format!("Could not reach Valor Connect Cloud ({}): {}", url, e))
            }
        })?;

    fire_state(&on_state, "RECEIVING");
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let body: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "rawText": text }));
    if !status.is_success() {
        return Err(PaxError::new(
            "SOCKET_ERROR",
            format!("Valor Cloud returned HTTP {}: {}", status.as_u16(), first_str(&body, &["error_msg", "msg", "desc", "rawText"])),
        ));
    }
    Ok(body)
}

async fn send_payload(terminal: &Terminal, payload: Value, timeout_ms: u64, on_state: Option<OnState>) -> Result<Value, PaxError> {
    match terminal.conn_type.as_str() {
        "usb" => send_usb_json(terminal.serial_path.clone(), terminal.baud_rate, payload, timeout_ms, on_state).await,
        "cloud" => send_cloud_json(terminal, &payload, timeout_ms, on_state).await,
        _ => send_tcp_json(&terminal.ip, effective_port(terminal), &payload, timeout_ms, on_state).await,
    }
}

fn map_tcp_err(where_str: &str, err: &std::io::Error) -> PaxError {
    match err.kind() {
        std::io::ErrorKind::ConnectionRefused => PaxError::new(
            "CONNECTION_REFUSED",
            format!("{} refused connection — enable Valor Connect (TCP port 5000) in Valor Portal and run Download Package on the VP100.", where_str),
        ),
        std::io::ErrorKind::TimedOut => PaxError::new("TIMEOUT", format!("Timed out connecting to Valor terminal {}", where_str)),
        _ => PaxError::new("HOST_UNREACHABLE", format!("Cannot reach Valor terminal {}: {}", where_str, err)),
    }
}

// ---------------------------------------------------------------------------
// Public driver operations
// ---------------------------------------------------------------------------

pub async fn initialize(terminal: &Terminal) -> Result<InitializeInfo, PaxError> {
    let started = Instant::now();
    match terminal.conn_type.as_str() {
        "usb" => {
            let ports = crate::bridge::serial::list_ports().await?;
            let found = ports.iter().any(|p| p.path == terminal.serial_path);
            if !found {
                return Err(PaxError::new(
                    "SERIAL_OPEN_FAILED",
                    format!("Valor USB port {} not found. Check the cable and USB mode.", terminal.serial_path),
                ));
            }
            Ok(InitializeInfo {
                result_code: "00".into(),
                result_txt: "USB port ready".into(),
                approved: true,
                serial_number: terminal.config_str("epi"),
                model: terminal.model.clone(),
                app_version: "Valor Connect USB".into(),
                mac_address: String::new(),
                raw: json!({ "serialPath": terminal.serial_path }),
                latency_ms: started.elapsed().as_millis() as i64,
            })
        }
        "cloud" => {
            let epi = terminal.config_str("epi");
            if epi.is_empty() || terminal.config_str("appId").is_empty() || terminal.config_str("appKey").is_empty() {
                return Err(PaxError::new("VALOR_CONFIG_MISSING", "EPI, App ID, and App Key are required for Valor Cloud."));
            }
            Ok(InitializeInfo {
                result_code: "00".into(),
                result_txt: "Valor Connect Cloud configured".into(),
                approved: true,
                serial_number: epi,
                model: terminal.model.clone(),
                app_version: "Valor Connect Cloud".into(),
                mac_address: String::new(),
                raw: json!({ "mode": "cloud" }),
                latency_ms: started.elapsed().as_millis() as i64,
            })
        }
        _ => {
            let port = effective_port(terminal);
            let timeout = config::ping_timeout_ms().min(5000);
            let (open, host_reachable, err) = tcp::probe(&terminal.ip, port, timeout).await;
            if !open {
                return Err(if host_reachable {
                    PaxError::new(
                        "CONNECTION_REFUSED",
                        format!(
                            "Valor VP100 at {}:{} is on the network, but port {} is closed. Enable Valor Connect (TCP) in Valor Portal → run Download Package → Reboot.",
                            terminal.ip, port, port
                        ),
                    )
                } else {
                    PaxError::new(
                        "HOST_UNREACHABLE",
                        format!("Cannot reach Valor terminal at {}:{} ({}). Verify same Wi-Fi/LAN.", terminal.ip, port, err.unwrap_or_default()),
                    )
                });
            }
            Ok(InitializeInfo {
                result_code: "00".into(),
                result_txt: "Valor Connect TCP listening".into(),
                approved: true,
                serial_number: terminal.config_str("epi"),
                model: terminal.model.clone(),
                app_version: "Valor Connect TCP".into(),
                mac_address: String::new(),
                raw: json!({ "ip": terminal.ip, "port": port, "cancelPort": effective_cancel_port(terminal) }),
                latency_ms: started.elapsed().as_millis() as i64,
            })
        }
    }
}

pub async fn diagnose(terminal: &Terminal) -> Result<Value, PaxError> {
    if terminal.conn_type == "usb" {
        let ports = crate::bridge::serial::list_ports().await?;
        let matching = ports.iter().find(|p| p.path == terminal.serial_path).cloned();
        let present = matching.is_some();
        return Ok(json!({
            "vendor": "valor",
            "connType": "usb",
            "serialPath": terminal.serial_path,
            "portPresent": present,
            "portsFound": ports.len(),
            "matchingPort": matching,
            "ecrLikelyListening": present,
            "nextSteps": if present {
                vec!["USB serial device found. Run Test Connection."]
            } else {
                vec![
                    "No matching Valor USB serial device found.",
                    "In Valor Portal: Device Management → Edit Parameters → Valor Connect = USB → Download Package on VP100.",
                ]
            }
        }));
    }

    if terminal.conn_type == "cloud" {
        let epi = terminal.config_str("epi");
        let ok = !epi.is_empty() && !terminal.config_str("appId").is_empty() && !terminal.config_str("appKey").is_empty();
        return Ok(json!({
            "vendor": "valor",
            "connType": "cloud",
            "epi": epi,
            "credentialsPresent": ok,
            "ecrLikelyListening": ok,
            "nextSteps": if ok {
                vec!["Valor Connect Cloud credentials configured. Send a small test sale to verify terminal delivery."]
            } else {
                vec!["Enter EPI (10-digit Terminal ID), App ID, App Key, and Channel ID from the Valor Portal."]
            }
        }));
    }

    let port = effective_port(terminal);
    let cancel_port = effective_cancel_port(terminal);
    let (open, host_reachable, err) = tcp::probe(&terminal.ip, port, 3000).await;
    let (cancel_open, _, _) = tcp::probe(&terminal.ip, cancel_port, 2000).await;

    let next_steps = if open {
        vec![
            "Valor Connect TCP port is open — terminal is waiting for transactions.",
            if cancel_open {
                "Cancel port (5001) is also open — remote POS cancel is ready."
            } else {
                "Note: secondary cancel port did not respond (some Valor builds only open 5001 during an active sale)."
            },
        ]
    } else if host_reachable {
        vec![
            "VP100 is on the network, but Valor Connect TCP port is closed (terminal is likely still in Standalone mode).",
            "In Valor Portal: Device Management → EPI → Edit Parameters → Terminal & Transaction → Valor Connect = Yes, Connection = TCP.",
            "On the VP100: Menu (≡) → Download Package → OK, then reboot the VP100.",
            "After reboot, confirm screen shows 'Server is Waiting for Transaction' and its IP address.",
        ]
    } else {
        vec![
            "Cannot reach this IP. Confirm the VP100 is powered on and connected to the same Wi-Fi/LAN subnet (AP isolation off).",
        ]
    };

    Ok(json!({
        "vendor": "valor",
        "connType": "tcp",
        "ip": terminal.ip,
        "port": port,
        "cancelPort": cancel_port,
        "hostReachable": host_reachable,
        "tcpOpen": open,
        "cancelPortOpen": cancel_open,
        "tcpError": err,
        "ecrLikelyListening": open,
        "nextSteps": next_steps,
    }))
}

pub async fn sale(
    terminal: &Terminal,
    amount_cents: i64,
    ecr_ref_num: String,
    tip_cents: i64,
    on_state: Option<OnState>,
) -> Result<CreditResponse, PaxError> {
    let key = terminal_key(terminal);
    active_req_ids().lock().unwrap().insert(key.clone(), ecr_ref_num.clone());
    let _guard = ActiveReqGuard(key);

    let payload = build_sale_payload(amount_cents, tip_cents, &ecr_ref_num);
    let raw = send_payload(terminal, payload, config::payment_timeout_ms(), on_state).await?;
    Ok(parse_credit_response(&raw, amount_cents, &ecr_ref_num))
}

pub async fn refund(
    terminal: &Terminal,
    amount_cents: i64,
    ecr_ref_num: String,
    on_state: Option<OnState>,
) -> Result<CreditResponse, PaxError> {
    let key = terminal_key(terminal);
    active_req_ids().lock().unwrap().insert(key.clone(), ecr_ref_num.clone());
    let _guard = ActiveReqGuard(key);

    let payload = build_refund_payload(amount_cents, &ecr_ref_num);
    let raw = send_payload(terminal, payload, config::payment_timeout_ms(), on_state).await?;
    Ok(parse_credit_response(&raw, amount_cents, &ecr_ref_num))
}

pub async fn void_transaction(
    terminal: &Terminal,
    orig_ref_num: String,
    ecr_ref_num: String,
    amount_cents: i64,
    on_state: Option<OnState>,
) -> Result<CreditResponse, PaxError> {
    let key = terminal_key(terminal);
    active_req_ids().lock().unwrap().insert(key.clone(), ecr_ref_num.clone());
    let _guard = ActiveReqGuard(key);

    let payload = build_void_payload(&orig_ref_num, amount_cents, &ecr_ref_num);
    let raw = send_payload(terminal, payload, config::payment_timeout_ms(), on_state).await?;
    Ok(parse_credit_response(&raw, amount_cents, &ecr_ref_num))
}

/// Send out-of-band cancel (`TRAN_MODE = "99"`) to port 5001 (Local TCP) or Cloud.
pub async fn cancel_on_terminal(terminal: &Terminal) -> Result<(), PaxError> {
    let key = terminal_key(terminal);
    let req_id = active_req_ids().lock().unwrap().get(&key).cloned().unwrap_or_default();
    let payload = build_cancel_payload(&req_id);

    match terminal.conn_type.as_str() {
        "usb" => Err(PaxError::new(
            "CANCEL_UNSUPPORTED_USB",
            "Valor USB cannot send a second cancel command while busy — press the red X key on the VP100.",
        )),
        "cloud" => {
            let _ = send_cloud_json(terminal, &payload, 10_000, None).await?;
            Ok(())
        }
        _ => {
            let cancel_port = effective_cancel_port(terminal);
            let frame = build_frame(&payload);
            let mut stream = tokio::time::timeout(
                Duration::from_secs(5),
                TcpStream::connect((terminal.ip.as_str(), cancel_port)),
            )
            .await
            .map_err(|_| PaxError::new("CANCEL_FAILED", format!("Timed out connecting to Valor cancel port {}:{}", terminal.ip, cancel_port)))?
            .map_err(|e| PaxError::new("CANCEL_FAILED", format!("Could not reach Valor cancel port {}:{}: {}", terminal.ip, cancel_port, e)))?;

            stream.write_all(&frame).await.map_err(|e| PaxError::new("CANCEL_FAILED", format!("Failed sending cancel to VP100: {}", e)))?;
            stream.flush().await.ok();
            // Give the terminal a short moment to acknowledge if it sends a reply, then close cleanly.
            let mut ack = [0u8; 256];
            let _ = tokio::time::timeout(Duration::from_millis(800), stream.read(&mut ack)).await;
            Ok(())
        }
    }
}

pub async fn batch_close(terminal: &Terminal) -> Result<BatchResponse, PaxError> {
    let req_id = format!("B{}", chrono::Utc::now().timestamp_millis());
    let payload = build_batch_close_payload(&req_id);
    let raw = send_payload(terminal, payload, config::payment_timeout_ms(), None).await?;
    let inner = raw.get("response").filter(|v| v.is_object()).unwrap_or(&raw);
    let state = first_str(inner, &["STATE", "state"]);
    let code = first_str(inner, &["ERROR_CODE", "error_code"]);
    let msg = first_str(inner, &["ERROR_MSG", "MSG", "msg"]);
    let settled = state == "0" || code == "00" || code == "000000" || first_str(&raw, &["error_no"]).eq_ignore_ascii_case("S00");
    Ok(BatchResponse {
        result_code: if !code.is_empty() { code } else if settled { "00".into() } else { state },
        result_txt: if !msg.is_empty() { msg } else if settled { "SETTLED".into() } else { "BATCH FAILED".into() },
        settled,
        batch_number: first_str(inner, &["BATCH_NO", "BATCH_NUMBER", "batch_no"]),
        credit_count: first_str(inner, &["SALE_COUNT", "CREDIT_COUNT", "TOTAL_COUNT"]),
        credit_amount_cents: parse_amount_to_cents(&first_str(inner, &["SALE_AMOUNT", "CREDIT_AMOUNT", "TOTAL_AMOUNT"]), 0),
        raw,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_and_extracts_stx_etx_json() {
        let req = build_sale_payload(1250, 0, "202611230001");
        let frame = build_frame(&req);
        assert_eq!(frame.first(), Some(&STX));
        assert_eq!(frame.last(), Some(&ETX));
        let parsed = try_extract_json(&frame).unwrap().unwrap();
        assert_eq!(parsed["TRAN_MODE"], "1");
        assert_eq!(parsed["TRAN_CODE"], "1");
        assert_eq!(parsed["AMOUNT"], "1250");
        assert_eq!(parsed["REQ_TXN_ID"], "202611230001");
    }

    #[test]
    fn parses_bare_json_without_stx_etx() {
        let raw = br#"{"STATE":"0","AUTH_CODE":"163489","RRN":"0111518118181831","MASKED_PAN":"************1234","CARD_LABEL":"VISA","AMOUNT":"1000"}"#;
        let parsed = try_extract_json(raw).unwrap().unwrap();
        let res = parse_credit_response(&parsed, 1000, "202611230001");
        assert!(res.approved);
        assert_eq!(res.auth_code, "163489");
        assert_eq!(res.last4, "1234");
        assert_eq!(res.card_type, "VISA");
        assert_eq!(res.approved_amount_cents, 1000);
        assert_eq!(res.ref_num, "0111518118181831");
    }

    #[test]
    fn parses_wrapped_cloud_approval_and_decline() {
        let ok = json!({
            "error_no": "S00",
            "error_code": "00",
            "msg": "APPROVED",
            "response": {
                "STATE": "0",
                "TXN_ID": "11254314",
                "AUTH_CODE": "998877",
                "MASKED_PAN": "************4242",
                "CARD_LABEL": "MASTERCARD",
                "AMOUNT": "25.50",
                "RRN": "RRN999"
            }
        });
        let res = parse_credit_response(&ok, 2550, "E1");
        assert!(res.approved);
        assert_eq!(res.approved_amount_cents, 2550);
        assert_eq!(res.last4, "4242");
        assert_eq!(res.auth_code, "998877");

        let declined = json!({
            "STATE": "1",
            "ERROR_CODE": "05",
            "ERROR_MSG": "DO NOT HONOR"
        });
        let dres = parse_credit_response(&declined, 2550, "E2");
        assert!(!dres.approved);
        assert_eq!(dres.result_code, "05");
        assert_eq!(dres.result_txt, "DO NOT HONOR");
    }
}
