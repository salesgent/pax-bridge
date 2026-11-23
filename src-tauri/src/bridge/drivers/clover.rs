//! Clover Flex / Mini / Compact / Station driver.
//!
//! Supports BOTH official Clover semi-integration protocols:
//!   1. `snpd` (or `tcp`) — **Secure Network Pay Display** (`wss://<ip>:12345/remote_pay`)
//!      Exact wire format verified against Clover's official `remote-pay-cloud@4.1.2` +
//!      `remote-pay-cloud-api@4.0.5` SDK:
//!        - `packageName`: `"com.clover.remote_protocol_broadcast.app"`
//!        - `payload`: JSON-stringified inner message
//!        - Pairing: `PAIRING_REQUEST` -> `PAIRING_CODE` (displayed in UI, typed on Flex) ->
//!          `PAIRING_RESPONSE` (persists `authenticationToken`) -> `DISCOVERY_REQUEST` -> `DISCOVERY_RESPONSE`
//!        - Sale: `TX_START` (version 2, `PayIntent` with `transactionType: "PAYMENT"`)
//!        - Refund: `TX_START` (version 2, `PayIntent` with `transactionType: "CREDIT"`, negative amount)
//!        - Auto-handles `PING` -> `PONG`, `VERIFY_SIGNATURE` -> `SIGNATURE_VERIFIED`,
//!          `CONFIRM_PAYMENT_MESSAGE` -> `PAYMENT_CONFIRMED` so the Flex never stalls
//!        - Cancel: `BREAK` sent over the active WebSocket
//!        - Local LAN WSS accepts Clover's self-signed/Device Server CA without manual OS cert install
//!   2. `rest` (or `cloud`) — **Clover REST Pay Display** (`https://<ip>:12346/connect/v1` or `https://api.clover.com/connect/v1`)
//!      Stateless HTTPS using OAuth Bearer token + `X-Clover-Device-Id` + `X-POS-Id` + `Idempotency-Key`.

use crate::bridge::config;
use crate::bridge::db::Terminal;
use crate::bridge::protocol::{fire_state, BatchResponse, CreditResponse, InitializeInfo, OnState, PaxError};
use crate::bridge::tcp;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::Connector;

pub const DEFAULT_SNPD_PORT: u16 = 12345;
pub const DEFAULT_REST_PORT: u16 = 12346;
pub const PACKAGE_NAME: &str = "com.clover.remote_protocol_broadcast.app";
pub const REMOTE_APP_ID: &str = "com.salesgent.pos:1.0.0";
pub const REMOTE_SDK: &str = "com.salesgent.bridge:1.0.0";
pub const PAY_ACTION: &str = "com.clover.remote.protocol.action.START_REMOTE_PROTOCOL_PAY";
const PROD_CLOUD_BASE: &str = "https://api.clover.com";
const SANDBOX_CLOUD_BASE: &str = "https://sandbox.dev.clover.com";

static MSG_ID: AtomicU64 = AtomicU64::new(1);

fn next_msg_id() -> String {
    MSG_ID.fetch_add(1, Ordering::SeqCst).to_string()
}

/// Active SNPD cancel channel per terminal key so `POST /api/payments/:id/cancel`
/// pushes a Clover `BREAK` message directly down the live WebSocket.
static SNPD_CANCELS: OnceLock<StdMutex<HashMap<String, mpsc::Sender<()>>>> = OnceLock::new();

fn snpd_cancels() -> &'static StdMutex<HashMap<String, mpsc::Sender<()>>> {
    SNPD_CANCELS.get_or_init(|| StdMutex::new(HashMap::new()))
}

struct SnpdCancelGuard(String);
impl Drop for SnpdCancelGuard {
    fn drop(&mut self) {
        snpd_cancels().lock().unwrap().remove(&self.0);
    }
}

pub fn terminal_key(terminal: &Terminal) -> String {
    if is_rest_mode(terminal) {
        let dev = terminal.config_str("deviceId");
        if terminal.conn_type == "cloud" {
            return format!("clover:cloud:{}", dev);
        }
        return format!("clover:rest:{}:{}", terminal.ip, effective_port(terminal));
    }
    format!("clover:snpd:{}:{}", terminal.ip, effective_port(terminal))
}

pub fn is_rest_mode(terminal: &Terminal) -> bool {
    matches!(terminal.conn_type.as_str(), "rest" | "cloud")
}

pub fn effective_port(terminal: &Terminal) -> u16 {
    if terminal.port != 0 {
        terminal.port
    } else if is_rest_mode(terminal) {
        if terminal.conn_type == "cloud" { 443 } else { DEFAULT_REST_PORT }
    } else {
        DEFAULT_SNPD_PORT
    }
}

// ---------------------------------------------------------------------------
// Local LAN TLS verifier for Clover's Device Server CA
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct LanNoVerify;

impl rustls::client::danger::ServerCertVerifier for LanNoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn lan_wss_connector() -> Connector {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("TLS protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(LanNoVerify))
        .with_no_client_auth();
    Connector::Rustls(Arc::new(cfg))
}

fn snpd_url(terminal: &Terminal) -> String {
    let scheme = if terminal.config_bool("insecureWs") { "ws" } else { "wss" };
    format!("{}://{}:{}/remote_pay", scheme, terminal.ip, effective_port(terminal))
}

// ---------------------------------------------------------------------------
// Clover RemoteMessage builders (exact official SDK envelope)
// ---------------------------------------------------------------------------

pub fn build_remote_command(method: &str, inner_payload: Value) -> Value {
    let id = next_msg_id();
    let mut inner = inner_payload;
    if let Some(map) = inner.as_object_mut() {
        map.entry("method".to_string()).or_insert_with(|| json!(method));
        map.entry("version".to_string()).or_insert_with(|| json!(0));
        map.insert("messageId".to_string(), json!(id));
    }
    json!({
        "id": id,
        "type": "COMMAND",
        "packageName": PACKAGE_NAME,
        "method": method,
        "version": 1,
        "remoteSourceSDK": REMOTE_SDK,
        "remoteApplicationID": REMOTE_APP_ID,
        "payload": serde_json::to_string(&inner).unwrap_or_else(|_| "{}".to_string()),
    })
}

pub fn build_pong() -> Value {
    json!({
        "type": "PONG",
        "packageName": PACKAGE_NAME,
        "remoteSourceSDK": REMOTE_SDK,
        "remoteApplicationID": REMOTE_APP_ID,
    })
}

pub fn build_pairing_request(terminal: &Terminal) -> Value {
    let pos_name = {
        let p = terminal.config_str("posName");
        if !p.is_empty() { p } else { "Salesgent POS".to_string() }
    };
    let serial = {
        let s = terminal.config_str("posSerial");
        if !s.is_empty() { s } else { format!("SG-{}", terminal.id) }
    };
    let auth_token = terminal.config_str("authToken");
    let mut inner = serde_json::Map::new();
    inner.insert("method".into(), json!("PAIRING_REQUEST"));
    inner.insert("version".into(), json!(0));
    inner.insert("name".into(), json!(pos_name));
    inner.insert("serialNumber".into(), json!(serial));
    inner.insert("applicationName".into(), json!(pos_name));
    if !auth_token.is_empty() {
        inner.insert("authenticationToken".into(), json!(auth_token));
    }
    build_remote_command("PAIRING_REQUEST", Value::Object(inner))
}

pub fn build_discovery_request() -> Value {
    build_remote_command(
        "DISCOVERY_REQUEST",
        json!({
            "method": "DISCOVERY_REQUEST",
            "version": 0,
            "supportsOrderModification": false,
        }),
    )
}

pub fn build_sale_tx_start(amount_cents: i64, tip_cents: i64, external_id: &str) -> Value {
    let mut intent = serde_json::Map::new();
    intent.insert("action".into(), json!(PAY_ACTION));
    intent.insert("amount".into(), json!(amount_cents));
    intent.insert("transactionType".into(), json!("PAYMENT"));
    intent.insert("cardEntryMethods".into(), json!(15)); // MAG_STRIPE | ICC_CONTACT | NFC_CONTACTLESS | MANUAL
    intent.insert("externalPaymentId".into(), json!(external_id));
    intent.insert("allowPartialAuth".into(), json!(true));
    if tip_cents > 0 {
        intent.insert("tipAmount".into(), json!(tip_cents));
    }
    build_remote_command(
        "TX_START",
        json!({
            "method": "TX_START",
            "version": 2,
            "suppressOnScreenTips": tip_cents > 0,
            "payIntent": Value::Object(intent),
        }),
    )
}

pub fn build_credit_tx_start(amount_cents: i64, external_id: &str) -> Value {
    // Official Clover SDK sends negative cents for manual refund (TransactionType.CREDIT).
    let neg_amount = -(amount_cents.abs());
    build_remote_command(
        "TX_START",
        json!({
            "method": "TX_START",
            "version": 2,
            "payIntent": {
                "action": PAY_ACTION,
                "amount": neg_amount,
                "transactionType": "CREDIT",
                "cardEntryMethods": 15,
                "externalPaymentId": external_id,
            }
        }),
    )
}

pub fn build_void_payment(payment_id: &str, order_id: &str) -> Value {
    let mut payment = serde_json::Map::new();
    payment.insert("id".into(), json!(payment_id));
    payment.insert("offline".into(), json!(false));
    if !order_id.is_empty() {
        payment.insert("order".into(), json!({ "id": order_id }));
    }
    build_remote_command(
        "VOID_PAYMENT",
        json!({
            "method": "VOID_PAYMENT",
            "version": 3,
            "payment": Value::Object(payment),
            "voidReason": "USER_CANCEL",
            "disableCloverPrinting": false,
            "disableReceiptSelection": false,
        }),
    )
}

pub fn build_break() -> Value {
    build_remote_command("BREAK", json!({ "method": "BREAK", "version": 0 }))
}

pub fn build_closeout() -> Value {
    build_remote_command(
        "CLOSEOUT_REQUEST",
        json!({
            "method": "CLOSEOUT_REQUEST",
            "version": 0,
            "allowOpenTabs": false,
        }),
    )
}

fn parse_inner_payload(envelope: &Value) -> Value {
    match envelope.get("payload") {
        Some(Value::String(s)) => serde_json::from_str::<Value>(s).unwrap_or_else(|_| json!({})),
        Some(v @ Value::Object(_)) => v.clone(),
        _ => json!({}),
    }
}

/// Extract a unified CreditResponse from a Clover Payment or Credit JSON object.
pub fn parse_clover_payment(payment: &Value, ecr_ref_num: &str, approved: bool, result_txt: &str) -> CreditResponse {
    let card_tx = payment.get("cardTransaction").unwrap_or(&Value::Null);
    let id = payment.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    let order_id = payment
        .get("order")
        .and_then(|o| o.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let auth_code = card_tx.get("authCode").and_then(Value::as_str).unwrap_or("").to_string();
    let last4 = card_tx.get("last4").and_then(Value::as_str).unwrap_or("").to_string();
    let card_type = card_tx.get("cardType").and_then(Value::as_str).unwrap_or("").to_string();
    let entry_mode = card_tx.get("entryType").and_then(Value::as_str).unwrap_or("").to_string();
    let host_ref = card_tx.get("referenceId").and_then(Value::as_str).unwrap_or("").to_string();
    let amount = payment.get("amount").and_then(Value::as_i64).unwrap_or(0).abs();
    let tip = payment.get("tipAmount").and_then(Value::as_i64).unwrap_or(0);
    let result_str = payment.get("result").and_then(Value::as_str).unwrap_or(result_txt).to_string();

    // Store "paymentId:orderId" in ref_num if orderId is present so VOID_PAYMENT always has both!
    let ref_num = if !order_id.is_empty() && !id.is_empty() {
        format!("{}:{}", id, order_id)
    } else {
        id.clone()
    };

    let is_ok = approved && (result_str.is_empty() || result_str.eq_ignore_ascii_case("SUCCESS"));

    CreditResponse {
        result_code: if is_ok { "000000".into() } else { result_str.clone() },
        result_txt: if !result_str.is_empty() { result_str.clone() } else if is_ok { "APPROVED".into() } else { "DECLINED".into() },
        host_response_code: card_tx.get("transactionNo").and_then(Value::as_str).unwrap_or("").to_string(),
        host_response_text: result_str,
        approved: is_ok,
        offline_approval: payment.get("offline").and_then(Value::as_bool).unwrap_or(false),
        auth_code,
        host_ref_num: host_ref,
        ref_num,
        transaction_num: id,
        ecr_ref_num: payment
            .get("externalPaymentId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(ecr_ref_num)
            .to_string(),
        masked_pan: if !last4.is_empty() { format!("****{}", last4) } else { String::new() },
        last4,
        card_type,
        entry_mode,
        approved_amount_cents: if is_ok { amount } else { 0 },
        tip_amount_cents: tip,
        timestamp: payment.get("createdTime").and_then(Value::as_i64).map(|t| t.to_string()).unwrap_or_default(),
        raw: payment.clone(),
    }
}

// ---------------------------------------------------------------------------
// SNPD WebSocket session helper
// ---------------------------------------------------------------------------

type WsStream = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn snpd_connect_and_pair(
    terminal: &Terminal,
    timeout_ms: u64,
    on_pairing_code: Option<Arc<dyn Fn(String) + Send + Sync>>,
) -> Result<(WsStream, Option<String>, Value), PaxError> {
    let url = snpd_url(terminal);
    let connector = if url.starts_with("wss://") { Some(lan_wss_connector()) } else { None };

    let (mut ws, _) = tokio::time::timeout(
        Duration::from_millis(timeout_ms.min(10_000)),
        tokio_tungstenite::connect_async_tls_with_config(url.as_str(), None, false, connector),
    )
    .await
    .map_err(|_| PaxError::new("TIMEOUT", format!("Timed out connecting to Clover Flex at {}", url)))?
    .map_err(|e| {
        let msg = e.to_string();
        if msg.contains("refused") {
            PaxError::new(
                "CONNECTION_REFUSED",
                format!("Clover Flex refused connection at {} — open Secure Network Pay Display on the Flex and leave it running.", url),
            )
        } else {
            PaxError::new("HOST_UNREACHABLE", format!("Could not connect to Clover Flex at {}: {}", url, msg))
        }
    })?;

    // Step 1: PAIRING_REQUEST
    let pair_msg = build_pairing_request(terminal);
    ws.send(Message::Text(pair_msg.to_string().into()))
        .await
        .map_err(|e| PaxError::new("WRITE_FAILED", format!("Failed sending Clover PAIRING_REQUEST: {}", e)))?;

    let mut new_auth_token: Option<String> = None;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);

    // Wait for PAIRING_RESPONSE (emitting PAIRING_CODE if unpaired), then send DISCOVERY_REQUEST
    let mut paired = false;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let next = tokio::time::timeout(remaining, ws.next())
            .await
            .map_err(|_| PaxError::new("TIMEOUT", "Timed out waiting for Clover pairing/discovery. Enter the pairing code on the Clover Flex screen."))?;

        let frame = match next {
            Some(Ok(Message::Text(txt))) => txt.to_string(),
            Some(Ok(Message::Ping(p))) => {
                let _ = ws.send(Message::Pong(p)).await;
                continue;
            }
            Some(Ok(Message::Close(_))) | None => {
                return Err(PaxError::new("CONNECTION_CLOSED", "Clover Flex closed the WebSocket connection during handshake"));
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(PaxError::new("SOCKET_ERROR", format!("Clover WebSocket error: {}", e))),
        };

        let env: Value = match serde_json::from_str(&frame) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if env.get("type").and_then(Value::as_str) == Some("PING") {
            let _ = ws.send(Message::Text(build_pong().to_string().into())).await;
            continue;
        }

        let method = env.get("method").and_then(Value::as_str).unwrap_or("");
        let payload = parse_inner_payload(&env);

        match method {
            "PAIRING_CODE" => {
                let code = payload.get("pairingCode").and_then(Value::as_str).unwrap_or("").to_string();
                tracing::info!("[clover] PAIRING_CODE received for terminal \"{}\": {}", terminal.name, code);
                if let Some(cb) = &on_pairing_code {
                    cb(code.clone());
                } else {
                    // Caller wasn't running interactive pairing — return clear error with the code!
                    return Err(PaxError::new(
                        "CLOVER_PAIRING_REQUIRED",
                        format!("Clover Flex needs pairing. Click Pair and enter code {} on the Clover screen.", code),
                    ));
                }
            }
            "PAIRING_RESPONSE" => {
                let state = payload.get("pairingState").and_then(Value::as_str).unwrap_or("");
                if state == "PAIRED" || state == "INITIAL" {
                    if let Some(tok) = payload.get("authenticationToken").and_then(Value::as_str) {
                        if !tok.is_empty() {
                            new_auth_token = Some(tok.to_string());
                        }
                    }
                    paired = true;
                    let disc = build_discovery_request();
                    ws.send(Message::Text(disc.to_string().into()))
                        .await
                        .map_err(|e| PaxError::new("WRITE_FAILED", format!("Failed sending DISCOVERY_REQUEST: {}", e)))?;
                } else if state == "FAILED" {
                    return Err(PaxError::new(
                        "CLOVER_PAIRING_FAILED",
                        "Clover pairing failed or was rejected on the terminal. Click Pair to try again.",
                    ));
                }
            }
            "DISCOVERY_RESPONSE" if paired => {
                return Ok((ws, new_auth_token, payload));
            }
            _ => {}
        }
    }

    Err(PaxError::new("TIMEOUT", "Clover Flex did not finish pairing/discovery in time"))
}

// ---------------------------------------------------------------------------
// REST Pay Display helpers (Local :12346 or Clover Cloud)
// ---------------------------------------------------------------------------

fn rest_client(terminal: &Terminal, timeout_ms: u64) -> Result<reqwest::Client, PaxError> {
    let is_local = terminal.conn_type != "cloud";
    reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .danger_accept_invalid_certs(is_local)
        .build()
        .map_err(|e| PaxError::new("SOCKET_ERROR", format!("Clover HTTP client error: {}", e)))
}

fn rest_base_url(terminal: &Terminal) -> String {
    if terminal.conn_type == "cloud" {
        if terminal.config_str("environment").eq_ignore_ascii_case("sandbox") {
            SANDBOX_CLOUD_BASE.to_string()
        } else {
            PROD_CLOUD_BASE.to_string()
        }
    } else {
        let scheme = if terminal.config_bool("insecureHttp") { "http" } else { "https" };
        format!("{}://{}:{}", scheme, terminal.ip, effective_port(terminal))
    }
}

fn rest_headers(terminal: &Terminal, idempotency_key: Option<&str>) -> Result<reqwest::header::HeaderMap, PaxError> {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
    let token = terminal.config_str("oauthToken");
    let device_id = terminal.config_str("deviceId");
    let pos_id = {
        let p = terminal.config_str("posId");
        if !p.is_empty() { p } else { "SalesgentPOS".to_string() }
    };
    if token.is_empty() {
        return Err(PaxError::new("CLOVER_AUTH_MISSING", "Clover REST Pay Display requires an OAuth Bearer token."));
    }
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", token)).map_err(|_| PaxError::new("CLOVER_AUTH_MISSING", "Invalid OAuth token"))?,
    );
    if !device_id.is_empty() {
        if let Ok(v) = HeaderValue::from_str(&device_id) {
            headers.insert(HeaderName::from_static("x-clover-device-id"), v);
        }
    }
    if let Ok(v) = HeaderValue::from_str(&pos_id) {
        headers.insert(HeaderName::from_static("x-pos-id"), v);
    }
    if let Some(k) = idempotency_key {
        if let Ok(v) = HeaderValue::from_str(k) {
            headers.insert(HeaderName::from_static("idempotency-key"), v);
        }
    }
    Ok(headers)
}

// ---------------------------------------------------------------------------
// Public driver operations
// ---------------------------------------------------------------------------

/// Interactive SNPD pairing: connects to Clover Flex, invokes `on_pairing_code`
/// with the 4-digit code to display in the UI, and returns the persistent `authToken`
/// once the cashier enters the code on the Clover screen.
pub async fn pair(
    terminal: &Terminal,
    on_pairing_code: Arc<dyn Fn(String) + Send + Sync>,
) -> Result<(String, Value), PaxError> {
    if is_rest_mode(terminal) {
        return Err(PaxError::new("INVALID_STATE", "Clover REST Pay Display uses an OAuth token rather than interactive pairing."));
    }
    let (mut ws, token, discovery) = snpd_connect_and_pair(terminal, 90_000, Some(on_pairing_code)).await?;
    let _ = ws.close(None).await;
    let tok = token.unwrap_or_else(|| terminal.config_str("authToken"));
    Ok((tok, discovery))
}

pub async fn initialize(terminal: &Terminal) -> Result<InitializeInfo, PaxError> {
    let started = Instant::now();
    if is_rest_mode(terminal) {
        let client = rest_client(terminal, config::ping_timeout_ms())?;
        let headers = rest_headers(terminal, None)?;
        let url = format!("{}/connect/v1/device/ping", rest_base_url(terminal));
        let resp = client.post(&url).headers(headers).send().await.map_err(|e| {
            PaxError::new("HOST_UNREACHABLE", format!("Could not reach Clover REST Pay Display ({}): {}", url, e))
        })?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or_else(|_| json!({}));
        if !status.is_success() {
            return Err(PaxError::new("CONNECTION_REFUSED", format!("Clover REST ping failed (HTTP {})", status.as_u16())));
        }
        return Ok(InitializeInfo {
            result_code: "000000".into(),
            result_txt: "Clover REST Pay Display online".into(),
            approved: true,
            serial_number: terminal.config_str("deviceId"),
            model: terminal.model.clone(),
            app_version: "REST Pay Display".into(),
            mac_address: String::new(),
            raw: body,
            latency_ms: started.elapsed().as_millis() as i64,
        });
    }

    let (mut ws, _token, discovery) = snpd_connect_and_pair(terminal, config::ping_timeout_ms(), None).await?;
    let _ = ws.close(None).await;
    let device_info = discovery.get("deviceInfo").unwrap_or(&Value::Null);
    Ok(InitializeInfo {
        result_code: "000000".into(),
        result_txt: "Clover SNPD online".into(),
        approved: true,
        serial_number: device_info.get("serial").and_then(Value::as_str).unwrap_or("").to_string(),
        model: device_info
            .get("model")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(&terminal.model)
            .to_string(),
        app_version: discovery.get("merchantId").and_then(Value::as_str).unwrap_or("SNPD").to_string(),
        mac_address: String::new(),
        raw: discovery,
        latency_ms: started.elapsed().as_millis() as i64,
    })
}

pub async fn diagnose(terminal: &Terminal) -> Result<Value, PaxError> {
    if terminal.conn_type == "cloud" {
        let has_token = !terminal.config_str("oauthToken").is_empty();
        let has_dev = !terminal.config_str("deviceId").is_empty();
        return Ok(json!({
            "vendor": "clover",
            "connType": "cloud",
            "hasOAuthToken": has_token,
            "hasDeviceId": has_dev,
            "ecrLikelyListening": has_token && has_dev,
            "nextSteps": if has_token && has_dev {
                vec!["Clover Cloud REST Pay Display configured. Run Test Connection."]
            } else {
                vec!["Enter your Clover OAuth Bearer Token and Device Serial ID (X-Clover-Device-Id)."]
            }
        }));
    }

    let port = effective_port(terminal);
    let (open, host_reachable, err) = tcp::probe(&terminal.ip, port, 3000).await;
    let paired = !terminal.config_str("authToken").is_empty();
    let next_steps = if open {
        if is_rest_mode(terminal) {
            vec!["Clover REST Pay Display port (12346) is open. Ensure OAuth Token + Device ID are entered, then click Test Connection."]
        } else if paired {
            vec!["Clover SNPD port (12345) is open and device has a saved pairing token. Click Test Connection."]
        } else {
            vec!["Clover SNPD port (12345) is open! Click Pair to display the 4-digit pairing code and type it on the Clover Flex screen."]
        }
    } else if host_reachable {
        if is_rest_mode(terminal) {
            vec![
                "Clover Flex is on Wi-Fi, but port 12346 is closed.",
                "On Clover Flex: open App Market → install & launch 'REST Pay Display', then leave it running.",
            ]
        } else {
            vec![
                "Clover Flex is on Wi-Fi, but port 12345 is closed.",
                "On Clover Flex: open App Market → install & launch 'Secure Network Pay Display' (SNPD), tap Start, and leave it open.",
            ]
        }
    } else {
        vec!["Cannot reach Clover Flex IP. Verify the Flex and this computer are on the same Wi-Fi/LAN subnet (AP isolation off)."]
    };

    Ok(json!({
        "vendor": "clover",
        "connType": terminal.conn_type,
        "ip": terminal.ip,
        "port": port,
        "hostReachable": host_reachable,
        "tcpOpen": open,
        "paired": paired,
        "tcpError": err,
        "ecrLikelyListening": open,
        "nextSteps": next_steps,
    }))
}

async fn run_snpd_tx(
    terminal: &Terminal,
    command: Value,
    ecr_ref_num: &str,
    on_state: Option<OnState>,
) -> Result<CreditResponse, PaxError> {
    fire_state(&on_state, "SENDING");
    let timeout_ms = config::payment_timeout_ms();
    let (mut ws, _token, _discovery) = snpd_connect_and_pair(terminal, 15_000, None).await?;

    let key = terminal_key(terminal);
    let (cancel_tx, mut cancel_rx) = mpsc::channel::<()>(1);
    snpd_cancels().lock().unwrap().insert(key.clone(), cancel_tx);
    let _guard = SnpdCancelGuard(key);

    ws.send(Message::Text(command.to_string().into()))
        .await
        .map_err(|e| PaxError::new("WRITE_FAILED", format!("Failed sending command to Clover Flex: {}", e)))?;
    fire_state(&on_state, "WAITING");

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut break_sent = false;

    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        tokio::select! {
            _ = cancel_rx.recv(), if !break_sent => {
                break_sent = true;
                tracing::info!("[clover] sending BREAK to cancel in-flight payment on \"{}\"", terminal.name);
                let _ = ws.send(Message::Text(build_break().to_string().into())).await;
            }
            next = tokio::time::timeout(remaining, ws.next()) => {
                let item = next.map_err(|_| PaxError::new("TIMEOUT", format!("Clover Flex timed out after {}ms", timeout_ms)))?;
                let frame = match item {
                    Some(Ok(Message::Text(txt))) => txt.to_string(),
                    Some(Ok(Message::Ping(p))) => { let _ = ws.send(Message::Pong(p)).await; continue; }
                    Some(Ok(Message::Close(_))) | None => {
                        return Err(PaxError::new("CONNECTION_CLOSED", "Clover Flex closed the WebSocket before completing the transaction"));
                    }
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => return Err(PaxError::new("SOCKET_ERROR", format!("Clover WebSocket error: {}", e))),
                };
                let env: Value = match serde_json::from_str(&frame) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if env.get("type").and_then(Value::as_str) == Some("PING") {
                    let _ = ws.send(Message::Text(build_pong().to_string().into())).await;
                    continue;
                }
                let method = env.get("method").and_then(Value::as_str).unwrap_or("");
                let payload = parse_inner_payload(&env);

                match method {
                    "UI_STATE" => {
                        let ui = payload.get("uiState").and_then(Value::as_str).unwrap_or("");
                        if ui.starts_with("PROCESSING") {
                            fire_state(&on_state, "RECEIVING");
                        } else if matches!(ui, "START" | "CONTACTLESS_TAP_REQUIRED" | "PIN_PAD") {
                            fire_state(&on_state, "WAITING");
                        }
                    }
                    "VERIFY_SIGNATURE" => {
                        if let Some(payment) = payload.get("payment") {
                            let reply = build_remote_command(
                                "SIGNATURE_VERIFIED",
                                json!({ "method": "SIGNATURE_VERIFIED", "version": 0, "payment": payment, "verified": true }),
                            );
                            let _ = ws.send(Message::Text(reply.to_string().into())).await;
                        }
                    }
                    "CONFIRM_PAYMENT_MESSAGE" => {
                        if let Some(payment) = payload.get("payment") {
                            let reply = build_remote_command(
                                "PAYMENT_CONFIRMED",
                                json!({ "method": "PAYMENT_CONFIRMED", "version": 0, "payment": payment }),
                            );
                            let _ = ws.send(Message::Text(reply.to_string().into())).await;
                        }
                    }
                    "TX_START_RESPONSE" => {
                        let success = payload.get("success").and_then(Value::as_bool).unwrap_or(true);
                        if !success {
                            let reason = payload.get("reason").and_then(Value::as_str).unwrap_or("Clover rejected transaction start");
                            let _ = ws.close(None).await;
                            return Ok(parse_clover_payment(&payload, ecr_ref_num, false, reason));
                        }
                    }
                    "FINISH_OK" => {
                        let _ = ws.close(None).await;
                        let obj = payload
                            .get("payment")
                            .or_else(|| payload.get("credit"))
                            .or_else(|| payload.get("refund"))
                            .unwrap_or(&payload);
                        return Ok(parse_clover_payment(obj, ecr_ref_num, true, "APPROVED"));
                    }
                    "FINISH_CANCEL" => {
                        let _ = ws.close(None).await;
                        let reason = payload.get("reason").and_then(Value::as_str).unwrap_or("CANCELED");
                        return Ok(parse_clover_payment(&payload, ecr_ref_num, false, reason));
                    }
                    "VOID_PAYMENT_RESPONSE" => {
                        let _ = ws.close(None).await;
                        let status = payload.get("status").and_then(Value::as_str).unwrap_or("");
                        let ok = status.eq_ignore_ascii_case("SUCCESS");
                        let pay = payload.get("payment").unwrap_or(&payload);
                        return Ok(parse_clover_payment(pay, ecr_ref_num, ok, status));
                    }
                    _ => {}
                }
            }
        }
    }

    Err(PaxError::new("TIMEOUT", format!("Clover Flex timed out after {}ms", timeout_ms)))
}

pub async fn sale(
    terminal: &Terminal,
    amount_cents: i64,
    ecr_ref_num: String,
    tip_cents: i64,
    on_state: Option<OnState>,
) -> Result<CreditResponse, PaxError> {
    if is_rest_mode(terminal) {
        fire_state(&on_state, "SENDING");
        let client = rest_client(terminal, config::payment_timeout_ms())?;
        let headers = rest_headers(terminal, Some(&ecr_ref_num))?;
        let url = format!("{}/connect/v1/payments", rest_base_url(terminal));
        let mut body = json!({
            "amount": amount_cents,
            "externalId": ecr_ref_num,
            "final": true,
        });
        if tip_cents > 0 {
            body["tipAmount"] = json!(tip_cents);
        }
        fire_state(&on_state, "WAITING");
        let resp = client.post(&url).headers(headers).json(&body).send().await.map_err(|e| {
            if e.is_timeout() {
                PaxError::new("TIMEOUT", "Clover REST payment timed out")
            } else {
                PaxError::new("HOST_UNREACHABLE", format!("Could not reach Clover REST Pay Display: {}", e))
            }
        })?;
        fire_state(&on_state, "RECEIVING");
        let ok = resp.status().is_success();
        let data: Value = resp.json().await.unwrap_or_else(|_| json!({}));
        let payment = data.get("payment").unwrap_or(&data);
        let msg = data.get("message").and_then(Value::as_str).unwrap_or(if ok { "APPROVED" } else { "DECLINED" });
        return Ok(parse_clover_payment(payment, &ecr_ref_num, ok, msg));
    }

    let cmd = build_sale_tx_start(amount_cents, tip_cents, &ecr_ref_num);
    run_snpd_tx(terminal, cmd, &ecr_ref_num, on_state).await
}

pub async fn refund(
    terminal: &Terminal,
    amount_cents: i64,
    ecr_ref_num: String,
    on_state: Option<OnState>,
) -> Result<CreditResponse, PaxError> {
    if is_rest_mode(terminal) {
        fire_state(&on_state, "SENDING");
        let client = rest_client(terminal, config::payment_timeout_ms())?;
        let headers = rest_headers(terminal, Some(&ecr_ref_num))?;
        let url = format!("{}/connect/v1/credits", rest_base_url(terminal));
        fire_state(&on_state, "WAITING");
        let resp = client
            .post(&url)
            .headers(headers)
            .json(&json!({ "amount": amount_cents, "externalId": ecr_ref_num }))
            .send()
            .await
            .map_err(|e| PaxError::new("HOST_UNREACHABLE", format!("Clover REST refund failed: {}", e)))?;
        fire_state(&on_state, "RECEIVING");
        let ok = resp.status().is_success();
        let data: Value = resp.json().await.unwrap_or_else(|_| json!({}));
        let credit = data.get("credit").unwrap_or(&data);
        return Ok(parse_clover_payment(credit, &ecr_ref_num, ok, if ok { "APPROVED" } else { "DECLINED" }));
    }

    let cmd = build_credit_tx_start(amount_cents, &ecr_ref_num);
    run_snpd_tx(terminal, cmd, &ecr_ref_num, on_state).await
}

pub async fn void_transaction(
    terminal: &Terminal,
    orig_ref_num: String,
    ecr_ref_num: String,
    _amount_cents: i64,
    on_state: Option<OnState>,
) -> Result<CreditResponse, PaxError> {
    let (payment_id, order_id) = match orig_ref_num.split_once(':') {
        Some((p, o)) => (p.to_string(), o.to_string()),
        None => (orig_ref_num.clone(), String::new()),
    };

    if is_rest_mode(terminal) {
        fire_state(&on_state, "SENDING");
        let client = rest_client(terminal, config::payment_timeout_ms())?;
        let headers = rest_headers(terminal, Some(&ecr_ref_num))?;
        let url = format!("{}/connect/v1/payments/{}/void", rest_base_url(terminal), payment_id);
        let resp = client
            .post(&url)
            .headers(headers)
            .json(&json!({ "voidReason": "USER_CANCEL" }))
            .send()
            .await
            .map_err(|e| PaxError::new("HOST_UNREACHABLE", format!("Clover REST void failed: {}", e)))?;
        let ok = resp.status().is_success();
        let data: Value = resp.json().await.unwrap_or_else(|_| json!({}));
        let payment = data.get("payment").unwrap_or(&data);
        return Ok(parse_clover_payment(payment, &ecr_ref_num, ok, if ok { "VOIDED" } else { "FAILED" }));
    }

    let cmd = build_void_payment(&payment_id, &order_id);
    run_snpd_tx(terminal, cmd, &ecr_ref_num, on_state).await
}

pub async fn cancel_on_terminal(terminal: &Terminal) -> Result<(), PaxError> {
    if is_rest_mode(terminal) {
        let client = rest_client(terminal, 10_000)?;
        let headers = rest_headers(terminal, None)?;
        let url = format!("{}/connect/v1/device/reset", rest_base_url(terminal));
        client
            .put(&url)
            .headers(headers)
            .send()
            .await
            .map_err(|e| PaxError::new("CANCEL_FAILED", format!("Clover REST reset failed: {}", e)))?;
        return Ok(());
    }

    let key = terminal_key(terminal);
    let tx = { snpd_cancels().lock().unwrap().get(&key).cloned() };
    let tx = tx.ok_or_else(|| PaxError::new("NOT_IN_FLIGHT", "No active Clover SNPD payment to cancel"))?;
    tx.try_send(())
        .map_err(|_| PaxError::new("CANCEL_FAILED", "Could not deliver BREAK command to Clover WebSocket"))
}

pub async fn batch_close(terminal: &Terminal) -> Result<BatchResponse, PaxError> {
    if is_rest_mode(terminal) {
        return Err(PaxError::new("UNSUPPORTED", "Clover REST Pay Display settles automatically via Clover Closeout."));
    }
    let (mut ws, _token, _disc) = snpd_connect_and_pair(terminal, 15_000, None).await?;
    ws.send(Message::Text(build_closeout().to_string().into()))
        .await
        .map_err(|e| PaxError::new("WRITE_FAILED", format!("Failed sending Clover closeout: {}", e)))?;

    let deadline = Instant::now() + Duration::from_millis(config::payment_timeout_ms());
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let next = tokio::time::timeout(remaining, ws.next())
            .await
            .map_err(|_| PaxError::new("TIMEOUT", "Clover closeout timed out"))?;
        if let Some(Ok(Message::Text(txt))) = next {
            if let Ok(env) = serde_json::from_str::<Value>(&txt) {
                if env.get("method").and_then(Value::as_str) == Some("CLOSEOUT_RESPONSE") {
                    let payload = parse_inner_payload(&env);
                    let status = payload.get("status").and_then(Value::as_str).unwrap_or("");
                    let settled = status.eq_ignore_ascii_case("SUCCESS");
                    let _ = ws.close(None).await;
                    return Ok(BatchResponse {
                        result_code: if settled { "000000".into() } else { status.into() },
                        result_txt: payload.get("reason").and_then(Value::as_str).unwrap_or(status).to_string(),
                        settled,
                        batch_number: payload
                            .get("batch")
                            .and_then(|b| b.get("id"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        credit_count: String::new(),
                        credit_amount_cents: 0,
                        raw: payload,
                    });
                }
            }
        }
    }
    Err(PaxError::new("TIMEOUT", "Clover closeout timed out"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_official_remote_message_envelope_with_stringified_payload() {
        let msg = build_sale_tx_start(1599, 200, "202611230001");
        assert_eq!(msg["type"], "COMMAND");
        assert_eq!(msg["method"], "TX_START");
        assert_eq!(msg["packageName"], PACKAGE_NAME);
        // Official Clover SDK requires `payload` to be a JSON STRING, not an object.
        let payload_str = msg["payload"].as_str().expect("payload must be a JSON string");
        let inner: Value = serde_json::from_str(payload_str).unwrap();
        assert_eq!(inner["version"], 2);
        assert_eq!(inner["payIntent"]["amount"], 1599);
        assert_eq!(inner["payIntent"]["tipAmount"], 200);
        assert_eq!(inner["payIntent"]["transactionType"], "PAYMENT");
        assert_eq!(inner["payIntent"]["action"], PAY_ACTION);
    }

    #[test]
    fn credit_refund_uses_negative_cents() {
        let msg = build_credit_tx_start(500, "REF1");
        let inner: Value = serde_json::from_str(msg["payload"].as_str().unwrap()).unwrap();
        assert_eq!(inner["payIntent"]["amount"], -500);
        assert_eq!(inner["payIntent"]["transactionType"], "CREDIT");
    }

    #[test]
    fn parses_clover_payment_and_preserves_order_id_for_void() {
        let pay = json!({
            "id": "PAY123",
            "order": { "id": "ORD456" },
            "amount": 1599,
            "tipAmount": 200,
            "result": "SUCCESS",
            "externalPaymentId": "E1",
            "cardTransaction": {
                "authCode": "A1B2C3",
                "last4": "4242",
                "cardType": "VISA",
                "entryType": "EMV_CONTACT",
                "referenceId": "HREF99"
            }
        });
        let res = parse_clover_payment(&pay, "E1", true, "APPROVED");
        assert!(res.approved);
        assert_eq!(res.auth_code, "A1B2C3");
        assert_eq!(res.last4, "4242");
        assert_eq!(res.ref_num, "PAY123:ORD456");
        assert_eq!(res.approved_amount_cents, 1599);
    }
}
