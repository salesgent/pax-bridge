//! Axum HTTP routes — ports `bridge/index.js`'s Express routers byte-for-byte:
//! same paths, JSON shapes, status codes (402 decline, 504 timeout, 409 busy).

use crate::bridge::db::{self, Terminal};
use crate::bridge::protocol::{CreditResponse, OnState, PaxError};
use crate::bridge::transport;
use crate::bridge::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Error mapping (mirrors the `MAP` table in index.js / `toHttpError`)
// ---------------------------------------------------------------------------
fn error_map(code: &str) -> (StatusCode, &'static str) {
    match code {
        "TERMINAL_BUSY" => (
            StatusCode::CONFLICT,
            "The terminal is already processing another transaction. Wait for it to finish before sending a new one.",
        ),
        "CONNECTION_REFUSED" => (
            StatusCode::BAD_GATEWAY,
            "Device reachable but BroadPOS is not listening on this port. In BroadPOS TSYS Sierra: set ECR-Terminal Integration Mode = External POS, Communication = TCP, Host Port = 10009 (or USB if using a cable), then leave BroadPOS on the idle screen.",
        ),
        "HOST_UNREACHABLE" => (
            StatusCode::BAD_GATEWAY,
            "Terminal unreachable — verify it is powered on and on the same LAN/subnet as this server.",
        ),
        "CONNECTION_CLOSED" => (
            StatusCode::BAD_GATEWAY,
            "The terminal closed the connection before responding. Check terminal status and try again.",
        ),
        "TIMEOUT" => (
            StatusCode::GATEWAY_TIMEOUT,
            "No response within the timeout. IMPORTANT: the card may already have been charged — verify on the terminal before retrying. Do NOT blindly retry.",
        ),
        "CANCELED" => (
            StatusCode::CONFLICT,
            "The terminal keeps its card prompt until it times out or its red Cancel key is pressed. If a card is charged anyway, this bridge voids it automatically.",
        ),
        "LRC_MISMATCH" => (
            StatusCode::BAD_GATEWAY,
            "Corrupted response (LRC check failed). This can indicate a protocol version mismatch or a noisy connection.",
        ),
        "MALFORMED_RESPONSE" => (
            StatusCode::BAD_GATEWAY,
            "Could not parse the terminal response. Verify the protocol version and constants against the PAX spec PDF.",
        ),
        "UNEXPECTED_RESPONSE" => (
            StatusCode::BAD_GATEWAY,
            "The terminal returned an unexpected command code. Verify the constants file against the PAX spec.",
        ),
        "WRITE_FAILED" => (StatusCode::BAD_GATEWAY, "Failed to send the command to the terminal."),
        "SOCKET_ERROR" => (StatusCode::BAD_GATEWAY, "Network error communicating with the terminal."),
        "SERIAL_OPEN_FAILED" => (
            StatusCode::BAD_GATEWAY,
            "Could not open the USB/serial port. Plug in the cable, set Android USB computer connection to PAX POSVCOM USB MODE, and in BroadPOS set Communication = USB with External POS / ECR on. On macOS the device appears as /dev/tty.usbmodem…",
        ),
        "SERIAL_ERROR" => (StatusCode::BAD_GATEWAY, "Serial communication error with the terminal."),
        "SERIAL_UNAVAILABLE" => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Serial support (the \"serialport\" native module) is not installed. Run npm install in the server directory.",
        ),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "Unexpected server error."),
    }
}

fn pax_error_body(err: &PaxError) -> (StatusCode, Value) {
    let (status, hint) = error_map(err.code);
    (status, json!({ "error": { "code": err.code, "message": err.message, "hint": hint } }))
}

fn pax_error_response(err: &PaxError) -> (StatusCode, Json<Value>) {
    let (status, body) = pax_error_body(err);
    (status, Json(body))
}

fn merge_json(base: &mut Value, extra: Value) {
    if let Value::Object(extra_map) = extra {
        if let Value::Object(base_map) = base {
            for (k, v) in extra_map {
                base_map.insert(k, v);
            }
        }
    }
}

fn not_found(message: &str) -> (StatusCode, Json<Value>) {
    (StatusCode::NOT_FOUND, Json(json!({ "error": { "code": "NOT_FOUND", "message": message } })))
}

fn validation_error(message: String) -> (StatusCode, Json<Value>) {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": { "code": "VALIDATION", "message": message } })))
}

fn fmt_money(cents: i64) -> String {
    format!("${:.2}", (cents as f64) / 100.0)
}

fn non_empty(s: &str) -> &str {
    if s.is_empty() {
        "-"
    } else {
        s
    }
}

fn json_number(v: Option<&Value>) -> Option<f64> {
    match v {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Terminal validation (mirrors `validateTerminal` in index.js)
// ---------------------------------------------------------------------------
const MODELS: [&str; 2] = ["A920 Pro", "A35"];

fn validate_terminal(body: &Value, partial: bool) -> Vec<String> {
    let mut errors = Vec::new();

    let name_present = body.get("name").is_some();
    if !partial || name_present {
        let empty = body.get("name").and_then(Value::as_str).map(|s| s.trim().is_empty()).unwrap_or(true);
        if empty {
            errors.push("name is required".to_string());
        }
    }

    let conn_type_field = body.get("connType").and_then(Value::as_str);
    if let Some(ct) = conn_type_field {
        if ct != "tcp" && ct != "usb" {
            errors.push("connType must be 'tcp' or 'usb'".to_string());
        }
    }

    let effective_type: Option<&str> = if let Some(ct) = conn_type_field {
        Some(ct)
    } else if !partial {
        Some("tcp")
    } else {
        None
    };

    if effective_type == Some("usb") {
        if !partial || body.get("serialPath").is_some() || conn_type_field.is_some() {
            let empty = body.get("serialPath").and_then(Value::as_str).map(|s| s.trim().is_empty()).unwrap_or(true);
            if empty {
                errors.push("serialPath is required for USB terminals".to_string());
            }
        }
        if let Some(v) = body.get("baudRate") {
            let n = json_number(Some(v));
            if n.map(|x| x <= 0.0).unwrap_or(true) {
                errors.push("baudRate must be a positive number".to_string());
            }
        }
    } else if effective_type == Some("tcp") {
        if !partial || body.get("ip").is_some() || conn_type_field.is_some() {
            let empty = body.get("ip").and_then(Value::as_str).map(|s| s.trim().is_empty()).unwrap_or(true);
            if empty {
                errors.push("ip is required for LAN (TCP) terminals".to_string());
            }
        }
        if let Some(v) = body.get("port") {
            let n = json_number(Some(v));
            if n.map(|x| x <= 0.0).unwrap_or(true) {
                errors.push("port must be a positive number".to_string());
            }
        }
    }

    if let Some(m) = body.get("model").and_then(Value::as_str) {
        if !MODELS.contains(&m) {
            errors.push(format!("model must be one of: {}", MODELS.join(", ")));
        }
    }

    errors
}

// ---------------------------------------------------------------------------
// Terminals routes
// ---------------------------------------------------------------------------

async fn get_serial_ports() -> (StatusCode, Json<Value>) {
    match transport::list_serial_ports().await {
        Ok(ports) => (StatusCode::OK, Json(json!({ "ports": ports }))),
        Err(err) => pax_error_response(&err),
    }
}

async fn list_terminals(State(state): State<AppState>) -> Json<Value> {
    let db = state.db.lock().await;
    Json(json!({ "terminals": db.list_terminals() }))
}

async fn create_terminal(State(state): State<AppState>, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let errors = validate_terminal(&body, false);
    if !errors.is_empty() {
        return validation_error(errors.join("; "));
    }
    let mut db = state.db.lock().await;
    let terminal = db.create_terminal(&body);
    (StatusCode::CREATED, Json(json!({ "terminal": terminal })))
}

async fn update_terminal_handler(State(state): State<AppState>, Path(id): Path<String>, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let errors = validate_terminal(&body, true);
    if !errors.is_empty() {
        return validation_error(errors.join("; "));
    }
    let mut db = state.db.lock().await;
    match db.update_terminal(&id, &body) {
        Some(t) => (StatusCode::OK, Json(json!({ "terminal": t }))),
        None => not_found("Terminal not found"),
    }
}

async fn delete_terminal_handler(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let mut db = state.db.lock().await;
    if db.delete_terminal(&id) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        not_found("Terminal not found").into_response()
    }
}

async fn ping_terminal(State(state): State<AppState>, Path(id): Path<String>) -> (StatusCode, Json<Value>) {
    let terminal = { state.db.lock().await.get_terminal(&id) };
    let terminal = match terminal {
        Some(t) => t,
        None => return not_found("Terminal not found"),
    };
    // Without this a ping would queue behind the live command and report a
    // timeout, which reads as "terminal unreachable" instead of "still busy".
    if transport::is_busy(&terminal) {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "online": true, "error": {
                "code": "TERMINAL_BUSY",
                "message": "Terminal is busy",
                "hint": "Finish the transaction on the terminal, or clear its card prompt with the red Cancel key, then try again."
            }})),
        );
    }

    match transport::initialize(&terminal, None).await {
        Ok(info) => (StatusCode::OK, Json(json!({ "online": true, "info": info }))),
        Err(err) => {
            let (status, mut body) = pax_error_body(&err);
            if let Value::Object(map) = &mut body {
                map.insert("online".to_string(), json!(false));
            }
            (status, Json(body))
        }
    }
}

/// Send the A14 that clears the terminal's own card prompt.
///
/// Returns the error when the terminal refused or was unreachable — the POS is
/// already released either way, so a failure here changes only what the cashier
/// is told to do at the device, never whether the cancel is reported.
async fn cancel_on_device(terminal: &Terminal) -> Option<PaxError> {
    match transport::cancel_on_terminal(terminal).await {
        Ok(()) => None,
        Err(err) => {
            tracing::warn!(
                "[payment] A14 cancel failed on terminal \"{}\": {} {}",
                terminal.name,
                err.code,
                err.message
            );
            Some(err)
        }
    }
}

fn device_json(err: Option<PaxError>) -> Value {
    match err {
        None => json!({ "sent": true }),
        Some(e) => json!({ "sent": false, "code": e.code, "message": e.message }),
    }
}

/// Abort whatever is in flight on this terminal, without needing a txnId.
///
/// The POS learns a payment's txnId from the WebSocket, which can be blocked
/// (e.g. Chrome Local Network Access) even when HTTP works. Without this route
/// a cashier's Cancel would then be local-only: the sale would stay live on the
/// wire and the terminal would keep prompting for a card.
async fn cancel_terminal(State(state): State<AppState>, Path(id): Path<String>) -> (StatusCode, Json<Value>) {
    let terminal = { state.db.lock().await.get_terminal(&id) };
    let terminal = match terminal {
        Some(t) => t,
        None => return not_found("Terminal not found"),
    };

    if transport::cancel(&terminal) {
        let device = cancel_on_device(&terminal).await;
        tracing::warn!(
            "[payment] cancel requested for terminal \"{}\" (no txnId) — device cancel {}",
            terminal.name,
            if device.is_none() { "sent" } else { "failed" }
        );
        (StatusCode::OK, Json(json!({ "canceled": true, "terminalId": id, "deviceCancel": device_json(device) })))
    } else {
        (
            StatusCode::CONFLICT,
            Json(json!({ "error": {
                "code": "NOT_IN_FLIGHT",
                "message": "No command is currently in flight for this terminal.",
                "hint": "The payment may have just completed — check its status before retrying."
            }})),
        )
    }
}

async fn diagnose_terminal(State(state): State<AppState>, Path(id): Path<String>) -> (StatusCode, Json<Value>) {
    let terminal = { state.db.lock().await.get_terminal(&id) };
    let terminal = match terminal {
        Some(t) => t,
        None => return not_found("Terminal not found"),
    };
    match transport::diagnose(&terminal).await {
        Ok(report) => (StatusCode::OK, Json(json!({ "report": report }))),
        Err(err) => pax_error_response(&err),
    }
}

async fn batch_close_terminal(State(state): State<AppState>, Path(id): Path<String>) -> (StatusCode, Json<Value>) {
    let terminal = { state.db.lock().await.get_terminal(&id) };
    let terminal = match terminal {
        Some(t) => t,
        None => return not_found("Terminal not found"),
    };

    if transport::is_busy(&terminal) {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": { "code": "TERMINAL_BUSY", "message": "Terminal is busy", "hint": "Wait for the current transaction to finish." } })),
        );
    }

    let unsettled: Vec<Map<String, Value>> = {
        let db = state.db.lock().await;
        db.list_unsettled()
            .into_iter()
            .filter(|t| t.get("terminalId").and_then(Value::as_str) == Some(terminal.id.as_str()))
            .collect()
    };

    match transport::batch_close(&terminal).await {
        Ok(result) => {
            let settled_count = if result.settled {
                let ids: Vec<String> = unsettled.iter().filter_map(|t| t.get("id").and_then(Value::as_str).map(|s| s.to_string())).collect();
                let batch_info = json!({ "batchNumber": result.batch_number, "settledAt": chrono::Utc::now().to_rfc3339() });
                {
                    let mut db = state.db.lock().await;
                    db.mark_settled(&ids, batch_info);
                }
                unsettled.len()
            } else {
                0
            };
            (StatusCode::OK, Json(json!({ "result": result, "settledCount": settled_count })))
        }
        Err(err) => pax_error_response(&err),
    }
}

// ---------------------------------------------------------------------------
// Payments routes
// ---------------------------------------------------------------------------

type InvokeFuture = Pin<Box<dyn Future<Output = Result<CreditResponse, PaxError>> + Send>>;
type Invoke = Box<dyn FnOnce(String, Option<OnState>, Option<transport::LateCredit>) -> InvokeFuture + Send>;

/// Record a result the terminal produced after the POS gave up waiting.
///
/// Cancelling stops the POS from waiting; it cannot stop a customer who taps
/// their card anyway. A sale that lands here is money taken for an order the
/// POS already abandoned, so it is voided immediately rather than left for
/// someone to notice on a statement.
async fn record_late_result(
    state: AppState,
    terminal: Terminal,
    txn_id: String,
    ecr_ref_num: String,
    tx_type: &'static str,
    result: Result<CreditResponse, PaxError>,
) {
    let result = match result {
        Ok(result) => result,
        Err(err) => {
            // The terminal never completed it either — the cancel stands.
            let mut patch = Map::new();
            patch.insert("status".into(), json!("CANCELED"));
            patch.insert("unknown".into(), json!(false));
            patch.insert("error".into(), json!({ "code": err.code, "message": err.message }));
            state.db.lock().await.update_transaction(&txn_id, patch);

            tracing::info!(
                "[payment] {} CANCELED settled — txnId={} ecrRefNum={} terminal=\"{}\" — terminal ended with {}: {} — no card was charged.",
                tx_type,
                txn_id,
                ecr_ref_num,
                terminal.name,
                err.code,
                err.message,
            );
            state.ws.emit(json!({ "type": "CANCELED", "txnId": txn_id, "ecrRefNum": ecr_ref_num }));
            return;
        }
    };

    let result_value = serde_json::to_value(&result).unwrap_or(Value::Null);
    let mut patch = Map::new();
    patch.insert("response".into(), result_value.clone());
    patch.insert("resultCode".into(), json!(result.result_code.clone()));
    patch.insert("resultTxt".into(), json!(result.result_txt.clone()));
    patch.insert("authCode".into(), json!(result.auth_code.clone()));
    patch.insert("refNum".into(), json!(result.ref_num.clone()));
    patch.insert("transactionNum".into(), json!(result.transaction_num.clone()));
    patch.insert("last4".into(), json!(result.last4.clone()));
    patch.insert("cardType".into(), json!(result.card_type.clone()));
    patch.insert("approvedAmountCents".into(), json!(result.approved_amount_cents));
    patch.insert("canceledByPos".into(), json!(true));

    if !result.approved {
        // Declined, or aborted on the device — nothing was charged.
        patch.insert("status".into(), json!("CANCELED"));
        patch.insert("unknown".into(), json!(false));
        state.db.lock().await.update_transaction(&txn_id, patch);

        tracing::info!(
            "[payment] {} CANCELED settled — txnId={} ecrRefNum={} terminal=\"{}\" resultCode={} resultTxt=\"{}\" — no card was charged.",
            tx_type,
            txn_id,
            ecr_ref_num,
            terminal.name,
            non_empty(&result.result_code),
            result.result_txt,
        );
        state.ws.emit(json!({ "type": "CANCELED", "txnId": txn_id, "ecrRefNum": ecr_ref_num, "result": result_value }));
        return;
    }

    // Approved after the POS walked away. Flag it as unknown until the void
    // settles it, so a crash here still leaves a record that needs attention.
    patch.insert("status".into(), json!("APPROVED"));
    patch.insert("unknown".into(), json!(true));
    state.db.lock().await.update_transaction(&txn_id, patch);
    state.ws.emit(json!({ "type": "APPROVED", "txnId": txn_id.clone(), "ecrRefNum": ecr_ref_num.clone(), "result": result_value }));

    if tx_type != "SALE" {
        // Auto-reversing a refund or a void is not something to decide here.
        tracing::error!(
            "[payment] {} APPROVED after cancel — txnId={} ecrRefNum={} terminal=\"{}\" amount={} — completed on the terminal after the POS cancelled; reverse it manually.",
            tx_type,
            txn_id,
            ecr_ref_num,
            terminal.name,
            fmt_money(result.approved_amount_cents),
        );
        return;
    }

    tracing::error!(
        "[payment] SALE APPROVED after cancel — txnId={} ecrRefNum={} terminal=\"{}\" amount={} authCode={} last4={} — voiding it now.",
        txn_id,
        ecr_ref_num,
        terminal.name,
        fmt_money(result.approved_amount_cents),
        non_empty(&result.auth_code),
        non_empty(&result.last4),
    );

    let void_ecr_ref_num = { state.db.lock().await.next_ecr_ref_num() };
    let orig_trans_num = if result.transaction_num.is_empty() { None } else { Some(result.transaction_num.clone()) };
    let voided = transport::void_transaction(
        &terminal,
        result.ref_num.clone(),
        void_ecr_ref_num.clone(),
        result.approved_amount_cents,
        orig_trans_num,
        None,
        None,
    )
    .await;

    let mut patch = Map::new();
    match voided {
        Ok(void_result) if void_result.approved => {
            patch.insert("status".into(), json!("VOIDED"));
            patch.insert("unknown".into(), json!(false));
            patch.insert("voidedByEcrRefNum".into(), json!(void_ecr_ref_num));
            state.db.lock().await.update_transaction(&txn_id, patch);

            tracing::warn!(
                "[payment] SALE VOIDED after cancel — txnId={} ecrRefNum={} terminal=\"{}\" amount={} — the cancelled charge was reversed.",
                txn_id,
                ecr_ref_num,
                terminal.name,
                fmt_money(result.approved_amount_cents),
            );
            state.ws.emit(json!({ "type": "VOIDED", "txnId": txn_id, "ecrRefNum": ecr_ref_num }));
        }
        Ok(void_result) => {
            patch.insert("voidError".into(), json!({ "code": void_result.result_code, "message": void_result.result_txt }));
            state.db.lock().await.update_transaction(&txn_id, patch);

            tracing::error!(
                "[payment] SALE void DECLINED after cancel — txnId={} ecrRefNum={} terminal=\"{}\" amount={} — THE CARD IS STILL CHARGED, void or refund it manually.",
                txn_id,
                ecr_ref_num,
                terminal.name,
                fmt_money(result.approved_amount_cents),
            );
            state.ws.emit(json!({ "type": "VOID_FAILED", "txnId": txn_id, "ecrRefNum": ecr_ref_num }));
        }
        Err(err) => {
            patch.insert("voidError".into(), json!({ "code": err.code, "message": err.message }));
            state.db.lock().await.update_transaction(&txn_id, patch);

            tracing::error!(
                "[payment] SALE void FAILED after cancel — txnId={} ecrRefNum={} terminal=\"{}\" amount={} code={} message=\"{}\" — THE CARD IS STILL CHARGED, void or refund it manually.",
                txn_id,
                ecr_ref_num,
                terminal.name,
                fmt_money(result.approved_amount_cents),
                err.code,
                err.message,
            );
            state.ws.emit(json!({ "type": "VOID_FAILED", "txnId": txn_id, "ecrRefNum": ecr_ref_num }));
        }
    }
}

async fn require_terminal(state: &AppState, body: &Value) -> Result<Terminal, (StatusCode, Json<Value>)> {
    let terminal_id = body.get("terminalId").and_then(Value::as_str);
    let db = state.db.lock().await;
    match terminal_id.and_then(|id| db.get_terminal(id)) {
        Some(t) => Ok(t),
        None => Err(not_found("Terminal not found. Check terminalId.")),
    }
}

fn valid_amount(v: Option<&Value>) -> Option<i64> {
    let n = v?.as_f64()?;
    if n.fract() == 0.0 && n > 0.0 {
        Some(n as i64)
    } else {
        None
    }
}

fn integer_or(v: Option<&Value>, default: i64) -> i64 {
    match v.and_then(Value::as_f64) {
        Some(n) if n.fract() == 0.0 => n as i64,
        _ => default,
    }
}

/// Shared runner for a payment-class command (sale/refund/void). Handles
/// logging, WS lifecycle, final-status mapping, and the timeout rule.
/// Returns (http status, JSON body, the resulting/updated transaction record).
async fn run_payment(
    state: &AppState,
    terminal: &Terminal,
    tx_type: &'static str,
    amount_cents: i64,
    tip_cents: i64,
    order_ref: Option<String>,
    extra: Map<String, Value>,
    invoke: Invoke,
) -> (StatusCode, Value, Map<String, Value>) {
    if transport::is_busy(terminal) {
        return (
            StatusCode::CONFLICT,
            json!({ "error": { "code": "TERMINAL_BUSY", "message": "Terminal is busy", "hint": "Finish the transaction on the terminal, or clear its card prompt with the red Cancel key, then try again." } }),
            Map::new(),
        );
    }

    let ecr_ref_num = {
        let mut db = state.db.lock().await;
        db.next_ecr_ref_num()
    };

    let mut tx_fields = Map::new();
    tx_fields.insert("terminalId".into(), json!(terminal.id));
    tx_fields.insert("terminalName".into(), json!(terminal.name));
    tx_fields.insert("type".into(), json!(tx_type));
    tx_fields.insert("amountCents".into(), json!(amount_cents));
    tx_fields.insert("tipCents".into(), json!(tip_cents));
    tx_fields.insert("orderRef".into(), order_ref.clone().map(Value::String).unwrap_or(Value::Null));
    tx_fields.insert("ecrRefNum".into(), json!(ecr_ref_num));
    tx_fields.insert("status".into(), json!("PENDING"));
    for (k, v) in extra {
        tx_fields.insert(k, v);
    }

    let txn = {
        let mut db = state.db.lock().await;
        db.create_transaction(tx_fields)
    };
    let txn_id = txn.get("id").and_then(Value::as_str).unwrap_or_default().to_string();

    tracing::info!(
        "[payment] {} started — txnId={} ecrRefNum={} terminal=\"{}\" amount={} tip={} orderRef={}",
        tx_type,
        txn_id,
        ecr_ref_num,
        terminal.name,
        fmt_money(amount_cents),
        fmt_money(tip_cents),
        order_ref.as_deref().unwrap_or("-"),
    );

    state.ws.emit(json!({ "type": "SENDING", "txnId": txn_id.clone(), "ecrRefNum": ecr_ref_num.clone() }));

    let ws_for_state = state.ws.clone();
    let state_txn_id = txn_id.clone();
    let state_ecr = ecr_ref_num.clone();
    let on_state: OnState = Arc::new(move |s: &str| {
        let evt = match s {
            "SENDING" => "SENDING",
            "WAITING" => "WAITING_FOR_CARD",
            "RECEIVING" => "PROCESSING",
            _ => return,
        };
        ws_for_state.emit(json!({ "type": evt, "txnId": state_txn_id.clone(), "ecrRefNum": state_ecr.clone() }));
    });

    let late_state = state.clone();
    let late_terminal = terminal.clone();
    let late_txn_id = txn_id.clone();
    let late_ecr_ref_num = ecr_ref_num.clone();
    let on_late: transport::LateCredit = Box::new(move |result| {
        tokio::spawn(record_late_result(late_state, late_terminal, late_txn_id, late_ecr_ref_num, tx_type, result));
    });

    match invoke(ecr_ref_num.clone(), Some(on_state), Some(on_late)).await {
        Ok(result) => {
            let status_str = if result.approved { "APPROVED" } else { "DECLINED" };
            let result_value = serde_json::to_value(&result).unwrap_or(Value::Null);

            let mut patch = Map::new();
            patch.insert("status".into(), json!(status_str));
            // Carried on the transaction too, so a payment list can show which
            // approvals never reached the processor.
            patch.insert("offlineApproval".into(), json!(result.offline_approval));
            patch.insert("response".into(), result_value.clone());
            patch.insert("resultCode".into(), json!(result.result_code.clone()));
            patch.insert("resultTxt".into(), json!(result.result_txt.clone()));
            patch.insert("authCode".into(), json!(result.auth_code.clone()));
            patch.insert("refNum".into(), json!(result.ref_num.clone()));
            patch.insert("transactionNum".into(), json!(result.transaction_num.clone()));
            patch.insert("last4".into(), json!(result.last4.clone()));
            patch.insert("cardType".into(), json!(result.card_type.clone()));
            patch.insert("approvedAmountCents".into(), json!(result.approved_amount_cents));

            let updated = {
                let mut db = state.db.lock().await;
                db.update_transaction(&txn_id, patch)
            }
            .unwrap_or_else(|| txn.clone());

            tracing::info!(
                "[payment] {} {} — txnId={} ecrRefNum={} terminal=\"{}\" amount={} authCode={} card={} last4={} resultCode={} resultTxt=\"{}\"",
                tx_type,
                status_str,
                txn_id,
                ecr_ref_num,
                terminal.name,
                fmt_money(result.approved_amount_cents),
                non_empty(&result.auth_code),
                non_empty(&result.card_type),
                non_empty(&result.last4),
                non_empty(&result.result_code),
                result.result_txt,
            );

            if result.offline_approval {
                tracing::warn!(
                    "[payment] {} APPROVED OFFLINE — txnId={} terminal=\"{}\" amount={} — the terminal approved this itself without reaching the processor; it is not authorized and may never settle.",
                    tx_type,
                    txn_id,
                    terminal.name,
                    fmt_money(result.approved_amount_cents),
                );
            }

            state.ws.emit(json!({ "type": status_str, "txnId": txn_id.clone(), "ecrRefNum": ecr_ref_num.clone(), "result": result_value.clone() }));

            let http_status = if status_str == "APPROVED" { StatusCode::OK } else { StatusCode::PAYMENT_REQUIRED };
            let body = json!({ "transaction": Value::Object(updated.clone()), "result": result_value });
            (http_status, body, updated)
        }
        Err(err) => {
            if err.code == "CANCELED" {
                // The POS stopped waiting, but the command is still on the wire
                // because the terminal keeps prompting regardless. Whatever it
                // finally reports goes to `record_late_result`, which settles
                // this record and voids a charge nobody asked for — until then
                // the outcome is genuinely unknown.
                let mut patch = Map::new();
                patch.insert("status".into(), json!("CANCELED"));
                patch.insert("unknown".into(), json!(true));
                patch.insert("error".into(), json!({ "code": "CANCELED", "message": err.message }));
                let updated = {
                    let mut db = state.db.lock().await;
                    db.update_transaction(&txn_id, patch)
                }
                .unwrap_or_else(|| txn.clone());

                tracing::warn!(
                    "[payment] {} CANCELED — txnId={} ecrRefNum={} terminal=\"{}\" amount={} — POS stopped waiting; still watching the terminal, a charge that lands now is voided automatically.",
                    tx_type,
                    txn_id,
                    ecr_ref_num,
                    terminal.name,
                    fmt_money(amount_cents),
                );
                state.ws.emit(json!({ "type": "CANCELED", "txnId": txn_id.clone(), "ecrRefNum": ecr_ref_num.clone() }));

                let (_, err_body) = pax_error_body(&err);
                let mut body = json!({ "transaction": Value::Object(updated.clone()) });
                merge_json(&mut body, err_body);
                (StatusCode::CONFLICT, body, updated)
            } else if err.code == "TIMEOUT" {
                let mut patch = Map::new();
                patch.insert("status".into(), json!("TIMEOUT"));
                patch.insert("unknown".into(), json!(true));
                patch.insert("error".into(), json!({ "code": "TIMEOUT", "message": err.message }));
                let updated = {
                    let mut db = state.db.lock().await;
                    db.update_transaction(&txn_id, patch)
                }
                .unwrap_or_else(|| txn.clone());

                tracing::error!(
                    "[payment] {} TIMEOUT — txnId={} ecrRefNum={} terminal=\"{}\" amount={} — CARD MAY HAVE BEEN CHARGED, verify on the terminal before retrying.",
                    tx_type,
                    txn_id,
                    ecr_ref_num,
                    terminal.name,
                    fmt_money(amount_cents),
                );
                state.ws.emit(json!({ "type": "TIMEOUT", "txnId": txn_id.clone() }));

                let (_, err_body) = pax_error_body(&err);
                let mut body = json!({ "transaction": Value::Object(updated.clone()) });
                merge_json(&mut body, err_body);
                (StatusCode::GATEWAY_TIMEOUT, body, updated)
            } else {
                let mut patch = Map::new();
                patch.insert("status".into(), json!("ERROR"));
                patch.insert("error".into(), json!({ "code": err.code, "message": err.message }));
                let updated = {
                    let mut db = state.db.lock().await;
                    db.update_transaction(&txn_id, patch)
                }
                .unwrap_or_else(|| txn.clone());

                tracing::error!(
                    "[payment] {} ERROR — txnId={} ecrRefNum={} terminal=\"{}\" amount={} code={} message=\"{}\"",
                    tx_type,
                    txn_id,
                    ecr_ref_num,
                    terminal.name,
                    fmt_money(amount_cents),
                    err.code,
                    err.message,
                );
                state.ws.emit(json!({ "type": "ERROR", "txnId": txn_id.clone(), "error": { "code": err.code, "message": err.message } }));

                let (status, err_body) = pax_error_body(&err);
                let mut body = json!({ "transaction": Value::Object(updated.clone()) });
                merge_json(&mut body, err_body);
                (status, body, updated)
            }
        }
    }
}

async fn sale_payment(State(state): State<AppState>, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let terminal = match require_terminal(&state, &body).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    let amount_cents = match valid_amount(body.get("amountCents")) {
        Some(a) => a,
        None => return validation_error("amountCents must be a positive integer (cents)".to_string()),
    };
    let tip_cents = integer_or(body.get("tipCents"), 0);
    let order_ref = body.get("orderRef").and_then(Value::as_str).map(|s| s.to_string());

    let terminal_for_invoke = terminal.clone();
    let invoke: Invoke = Box::new(move |ecr_ref_num, on_state, on_late| {
        Box::pin(async move { transport::sale(&terminal_for_invoke, amount_cents, ecr_ref_num, tip_cents, on_state, on_late).await })
    });

    let (status, resp_body, _updated) = run_payment(&state, &terminal, "SALE", amount_cents, tip_cents, order_ref, Map::new(), invoke).await;
    (status, Json(resp_body))
}

async fn refund_payment(State(state): State<AppState>, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let terminal = match require_terminal(&state, &body).await {
        Ok(t) => t,
        Err(e) => return e,
    };
    let amount_cents = match valid_amount(body.get("amountCents")) {
        Some(a) => a,
        None => return validation_error("amountCents must be a positive integer (cents)".to_string()),
    };
    let order_ref = body.get("orderRef").and_then(Value::as_str).map(|s| s.to_string());

    let terminal_for_invoke = terminal.clone();
    let invoke: Invoke = Box::new(move |ecr_ref_num, on_state, on_late| {
        Box::pin(async move { transport::refund(&terminal_for_invoke, amount_cents, ecr_ref_num, on_state, on_late).await })
    });

    let (status, resp_body, _updated) = run_payment(&state, &terminal, "RETURN", amount_cents, 0, order_ref, Map::new(), invoke).await;
    (status, Json(resp_body))
}

async fn void_payment(State(state): State<AppState>, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let terminal = match require_terminal(&state, &body).await {
        Ok(t) => t,
        Err(e) => return e,
    };

    let orig_txn_id = body.get("origTxnId").and_then(Value::as_str).map(|s| s.to_string());
    let orig = match &orig_txn_id {
        Some(id) => state.db.lock().await.get_transaction(id),
        None => None,
    };
    let orig = match orig {
        Some(o) => o,
        None => return not_found("Original transaction not found (origTxnId)."),
    };

    let orig_status = orig.get("status").and_then(Value::as_str).unwrap_or("");
    if orig_status != "APPROVED" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": { "code": "INVALID_STATE", "message": "Only APPROVED transactions can be voided." } })),
        );
    }
    if orig.get("settled").and_then(Value::as_bool).unwrap_or(false) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": { "code": "INVALID_STATE", "message": "Transaction already settled — issue a refund instead of a void." } })),
        );
    }

    let orig_amount_cents = orig.get("amountCents").and_then(Value::as_i64).unwrap_or(0);
    let orig_order_ref = orig.get("orderRef").and_then(Value::as_str).map(|s| s.to_string());
    let orig_ref_num = orig.get("refNum").and_then(Value::as_str).unwrap_or("").to_string();
    let orig_transaction_num = orig.get("transactionNum").and_then(Value::as_str).map(|s| s.to_string());
    let orig_id = orig.get("id").and_then(Value::as_str).unwrap_or("").to_string();

    let mut extra = Map::new();
    extra.insert("voidOfTxnId".into(), json!(orig_id));
    extra.insert("voidOfRefNum".into(), json!(orig_ref_num));

    let terminal_for_invoke = terminal.clone();
    let orig_ref_for_invoke = orig_ref_num.clone();
    let orig_trans_for_invoke = orig_transaction_num.clone();
    let invoke: Invoke = Box::new(move |ecr_ref_num, on_state, on_late| {
        Box::pin(async move {
            transport::void_transaction(
                &terminal_for_invoke,
                orig_ref_for_invoke,
                ecr_ref_num,
                orig_amount_cents,
                orig_trans_for_invoke,
                on_state,
                on_late,
            )
            .await
        })
    });

    let (status, resp_body, updated) = run_payment(&state, &terminal, "VOID", orig_amount_cents, 0, orig_order_ref, extra, invoke).await;

    if updated.get("status").and_then(Value::as_str) == Some("APPROVED") {
        let void_txn_id = updated.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        let mut patch = Map::new();
        patch.insert("status".into(), json!("VOIDED"));
        patch.insert("voidedByTxnId".into(), json!(void_txn_id));
        let mut db = state.db.lock().await;
        db.update_transaction(&orig_id, patch);
    }

    (status, Json(resp_body))
}

#[derive(Debug, Deserialize, Default)]
struct ListPaymentsQuery {
    status: Option<String>,
    #[serde(rename = "type")]
    tx_type: Option<String>,
    from: Option<String>,
    to: Option<String>,
    #[serde(rename = "terminalId")]
    terminal_id: Option<String>,
    page: Option<i64>,
    #[serde(rename = "pageSize")]
    page_size: Option<i64>,
}

async fn list_payments(State(state): State<AppState>, Query(q): Query<ListPaymentsQuery>) -> Json<Value> {
    let db = state.db.lock().await;
    let result = db.query_transactions(&db::TxQuery {
        status: q.status,
        tx_type: q.tx_type,
        from: q.from,
        to: q.to,
        terminal_id: q.terminal_id,
        page: q.page,
        page_size: q.page_size,
    });
    Json(json!({ "total": result.total, "page": result.page, "pageSize": result.page_size, "pageCount": result.page_count, "rows": result.rows }))
}

async fn get_payment(State(state): State<AppState>, Path(id): Path<String>) -> (StatusCode, Json<Value>) {
    let db = state.db.lock().await;
    match db.get_transaction(&id) {
        Some(t) => (StatusCode::OK, Json(json!({ "transaction": t }))),
        None => not_found("Transaction not found"),
    }
}

/// Abort a transaction that is still waiting on the terminal, so the cashier
/// isn't stuck until the payment timeout. The in-flight request's own handler
/// (`run_payment`) is what writes the CANCELED status and emits the WS event
/// once its connection drops — this endpoint only fires the trigger.
async fn cancel_payment(State(state): State<AppState>, Path(id): Path<String>) -> (StatusCode, Json<Value>) {
    let txn = { state.db.lock().await.get_transaction(&id) };
    let txn = match txn {
        Some(t) => t,
        None => return not_found("Transaction not found"),
    };

    let status = txn.get("status").and_then(Value::as_str).unwrap_or("");
    if status != "PENDING" {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": {
                "code": "NOT_CANCELABLE",
                "message": format!("Transaction already finished with status {status}."),
                "hint": "Only a payment still waiting on the terminal can be cancelled."
            }})),
        );
    }

    let terminal_id = txn.get("terminalId").and_then(Value::as_str).unwrap_or_default().to_string();
    let terminal = { state.db.lock().await.get_terminal(&terminal_id) };
    let terminal = match terminal {
        Some(t) => t,
        None => return not_found("Terminal for this transaction no longer exists"),
    };

    if transport::cancel(&terminal) {
        let device = cancel_on_device(&terminal).await;
        tracing::warn!(
            "[payment] cancel requested — txnId={} terminal=\"{}\" — device cancel {}",
            id,
            terminal.name,
            if device.is_none() { "sent" } else { "failed" }
        );
        (StatusCode::OK, Json(json!({ "canceled": true, "txnId": id, "deviceCancel": device_json(device) })))
    } else {
        // Nothing on the wire: it already completed or was never sent. The
        // caller should re-read the transaction to see where it actually landed.
        (
            StatusCode::CONFLICT,
            Json(json!({ "error": {
                "code": "NOT_IN_FLIGHT",
                "message": "No command is currently in flight for this terminal.",
                "hint": "The payment may have just completed — check its status before retrying."
            }})),
        )
    }
}

// ---------------------------------------------------------------------------
// Misc routes
// ---------------------------------------------------------------------------

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok", "message": "Server is running", "bridgeVersion": "rust-tauri" }))
}

async fn api_root() -> Json<Value> {
    Json(json!({ "message": "Welcome to the API" }))
}

// ---------------------------------------------------------------------------
// Router assembly
// ---------------------------------------------------------------------------

pub fn router(state: AppState) -> Router {
    let terminals_router = Router::new()
        .route("/serial-ports", get(get_serial_ports))
        .route("/", get(list_terminals).post(create_terminal))
        .route("/{id}", put(update_terminal_handler).delete(delete_terminal_handler))
        .route("/{id}/ping", post(ping_terminal))
        .route("/{id}/cancel", post(cancel_terminal))
        .route("/{id}/diagnose", post(diagnose_terminal))
        .route("/{id}/batch-close", post(batch_close_terminal));

    let payments_router = Router::new()
        .route("/sale", post(sale_payment))
        .route("/refund", post(refund_payment))
        .route("/void", post(void_payment))
        .route("/", get(list_payments))
        .route("/{id}", get(get_payment))
        .route("/{id}/cancel", post(cancel_payment));

    Router::new()
        .route("/api/health", get(health))
        .route("/api", get(api_root))
        .nest("/api/terminals", terminals_router)
        .nest("/api/payments", payments_router)
        .route("/ws", get(crate::bridge::ws::ws_handler))
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(state)
}
