//! Dejavoo SPIn (Secure Payment Interface) v2 driver — Z8, Z9, Z11, QD2, QD4, P1, P3, P8.
//!
//! Supports both:
//!   - `cloud`: SPIn v2 Cloud REST (`https://api.spinpos.net/v2` or sandbox `https://test.spinpos.net/spin/v2`)
//!   - `tcp`:   SPIn Local LAN REST (`http://<ip>:<port>/v2`, default port 8080)

use crate::bridge::config;
use crate::bridge::db::Terminal;
use crate::bridge::protocol::{fire_state, BatchResponse, CreditResponse, InitializeInfo, OnState, PaxError};
use crate::bridge::tcp;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

pub const DEFAULT_LOCAL_PORT: u16 = 8080;
const PROD_BASE: &str = "https://api.spinpos.net/v2";
const SANDBOX_BASE: &str = "https://test.spinpos.net/spin/v2";

pub fn terminal_key(terminal: &Terminal) -> String {
    if terminal.conn_type == "tcp" {
        format!("dejavoo:tcp:{}:{}", terminal.ip, effective_port(terminal))
    } else {
        format!("dejavoo:cloud:{}", terminal.config_str("tpn"))
    }
}

pub fn effective_port(terminal: &Terminal) -> u16 {
    if terminal.port != 0 { terminal.port } else { DEFAULT_LOCAL_PORT }
}

fn base_url(terminal: &Terminal) -> String {
    let custom = terminal.config_str("apiUrl");
    if !custom.is_empty() {
        return custom.trim_end_matches('/').to_string();
    }
    if terminal.conn_type == "tcp" {
        format!("http://{}:{}/v2", terminal.ip, effective_port(terminal))
    } else if terminal.config_str("environment").eq_ignore_ascii_case("sandbox") {
        SANDBOX_BASE.to_string()
    } else {
        PROD_BASE.to_string()
    }
}

fn require_creds(terminal: &Terminal) -> Result<(String, String), PaxError> {
    let tpn = terminal.config_str("tpn");
    let auth_key = terminal.config_str("authKey");
    if tpn.is_empty() || auth_key.is_empty() {
        return Err(PaxError::new("DEJAVOO_CONFIG_MISSING", "Dejavoo SPIn requires both TPN and Auth Key."));
    }
    Ok((tpn, auth_key))
}

fn cents_to_dollars(cents: i64) -> f64 {
    (cents as f64) / 100.0
}

pub fn parse_spin_response(raw: &Value, requested_cents: i64, ecr_ref_num: &str) -> CreditResponse {
    let general = raw.get("GeneralResponse").unwrap_or(raw);
    let result_code = general.get("ResultCode").and_then(Value::as_str).unwrap_or("").to_string();
    let status_code = general.get("StatusCode").and_then(Value::as_str).unwrap_or("").to_string();
    let message = general
        .get("Message")
        .or_else(|| general.get("DetailedMessage"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let auth_code = raw.get("AuthCode").and_then(Value::as_str).unwrap_or("").to_string();
    let ref_id = raw.get("ReferenceId").and_then(Value::as_str).unwrap_or(ecr_ref_num).to_string();
    let rrn = raw.get("RRN").or_else(|| raw.get("HostRefNum")).and_then(Value::as_str).unwrap_or("").to_string();
    let card_data = raw.get("CardData").unwrap_or(&Value::Null);
    let last4 = card_data.get("Last4").and_then(Value::as_str).unwrap_or("").to_string();
    let card_type = card_data.get("CardType").and_then(Value::as_str).unwrap_or("").to_string();
    let entry_mode = card_data.get("EntryType").and_then(Value::as_str).unwrap_or("").to_string();

    let amounts = raw.get("Amounts").unwrap_or(&Value::Null);
    let approved_cents = amounts
        .get("TotalAmount")
        .or_else(|| amounts.get("Amount"))
        .and_then(Value::as_f64)
        .map(|d| (d * 100.0).round() as i64)
        .unwrap_or(requested_cents);
    let tip_cents = amounts
        .get("TipAmount")
        .and_then(Value::as_f64)
        .map(|d| (d * 100.0).round() as i64)
        .unwrap_or(0);

    let approved = (result_code == "0" || status_code == "0000") && !auth_code.is_empty();

    CreditResponse {
        result_code: if !status_code.is_empty() { status_code } else { result_code },
        result_txt: if !message.is_empty() { message.clone() } else if approved { "APPROVED".into() } else { "DECLINED".into() },
        host_response_code: String::new(),
        host_response_text: message,
        approved,
        offline_approval: false,
        auth_code,
        host_ref_num: rrn.clone(),
        ref_num: if !rrn.is_empty() { rrn } else { ref_id.clone() },
        transaction_num: ref_id.clone(),
        ecr_ref_num: ref_id,
        masked_pan: if !last4.is_empty() { format!("****{}", last4) } else { String::new() },
        last4,
        card_type,
        entry_mode,
        approved_amount_cents: if approved { approved_cents } else { 0 },
        tip_amount_cents: tip_cents,
        timestamp: String::new(),
        raw: raw.clone(),
    }
}

async fn post_spin(terminal: &Terminal, path: &str, body: Value, timeout_ms: u64, on_state: Option<OnState>) -> Result<Value, PaxError> {
    fire_state(&on_state, "SENDING");
    let url = format!("{}{}", base_url(terminal), path);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .build()
        .map_err(|e| PaxError::new("SOCKET_ERROR", format!("HTTP client error: {}", e)))?;

    fire_state(&on_state, "WAITING");
    let resp = client.post(&url).json(&body).send().await.map_err(|e| {
        if e.is_timeout() {
            PaxError::new("TIMEOUT", format!("Dejavoo SPIn request timed out after {}ms", timeout_ms))
        } else {
            PaxError::new("HOST_UNREACHABLE", format!("Could not reach Dejavoo SPIn ({}): {}", url, e))
        }
    })?;

    fire_state(&on_state, "RECEIVING");
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let val: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "rawText": text }));
    if !status.is_success() {
        return Err(PaxError::new("SOCKET_ERROR", format!("Dejavoo SPIn returned HTTP {}", status.as_u16())));
    }
    Ok(val)
}

pub async fn initialize(terminal: &Terminal) -> Result<InitializeInfo, PaxError> {
    let started = Instant::now();
    let (tpn, auth_key) = require_creds(terminal)?;
    if terminal.conn_type == "tcp" {
        let port = effective_port(terminal);
        let (open, host_up, err) = tcp::probe(&terminal.ip, port, 3000).await;
        if !open {
            return Err(if host_up {
                PaxError::new("CONNECTION_REFUSED", format!("Dejavoo at {}:{} is reachable but SPIn port {} is closed.", terminal.ip, port, port))
            } else {
                PaxError::new("HOST_UNREACHABLE", format!("Cannot reach Dejavoo at {}:{} ({}).", terminal.ip, port, err.unwrap_or_default()))
            });
        }
    }
    let _ = auth_key;
    Ok(InitializeInfo {
        result_code: "0000".into(),
        result_txt: "Dejavoo SPIn ready".into(),
        approved: true,
        serial_number: tpn,
        model: terminal.model.clone(),
        app_version: "SPIn v2".into(),
        mac_address: String::new(),
        raw: json!({ "baseUrl": base_url(terminal) }),
        latency_ms: started.elapsed().as_millis() as i64,
    })
}

pub async fn diagnose(terminal: &Terminal) -> Result<Value, PaxError> {
    let has_creds = !terminal.config_str("tpn").is_empty() && !terminal.config_str("authKey").is_empty();
    if terminal.conn_type == "tcp" {
        let port = effective_port(terminal);
        let (open, host_up, err) = tcp::probe(&terminal.ip, port, 3000).await;
        return Ok(json!({
            "vendor": "dejavoo",
            "connType": "tcp",
            "ip": terminal.ip,
            "port": port,
            "hostReachable": host_up,
            "tcpOpen": open,
            "credentialsPresent": has_creds,
            "tcpError": err,
            "ecrLikelyListening": open && has_creds,
            "nextSteps": if open && has_creds {
                vec!["Dejavoo SPIn local port is open and TPN/AuthKey are set. Run Test Connection."]
            } else {
                vec!["Ensure SPIn Local Integration is enabled on the Dejavoo terminal and TPN + AuthKey are entered."]
            }
        }));
    }
    Ok(json!({
        "vendor": "dejavoo",
        "connType": "cloud",
        "tpn": terminal.config_str("tpn"),
        "credentialsPresent": has_creds,
        "ecrLikelyListening": has_creds,
        "nextSteps": if has_creds {
            vec!["Dejavoo SPIn Cloud credentials configured. Run a small test sale to verify terminal delivery."]
        } else {
            vec!["Enter the terminal's TPN and AuthKey from the Dejavoo STEAM / iPOSpays portal."]
        }
    }))
}

pub async fn sale(
    terminal: &Terminal,
    amount_cents: i64,
    ecr_ref_num: String,
    tip_cents: i64,
    on_state: Option<OnState>,
) -> Result<CreditResponse, PaxError> {
    let (tpn, auth_key) = require_creds(terminal)?;
    let body = json!({
        "Amount": cents_to_dollars(amount_cents),
        "TipAmount": cents_to_dollars(tip_cents),
        "PaymentType": "Credit",
        "ReferenceId": ecr_ref_num,
        "Tpn": tpn,
        "Authkey": auth_key,
        "PrintReceipt": "Both",
        "GetReceipt": "No",
    });
    let raw = post_spin(terminal, "/Payment/Sale", body, config::payment_timeout_ms(), on_state).await?;
    Ok(parse_spin_response(&raw, amount_cents, &ecr_ref_num))
}

pub async fn refund(
    terminal: &Terminal,
    amount_cents: i64,
    ecr_ref_num: String,
    on_state: Option<OnState>,
) -> Result<CreditResponse, PaxError> {
    let (tpn, auth_key) = require_creds(terminal)?;
    let body = json!({
        "Amount": cents_to_dollars(amount_cents),
        "PaymentType": "Credit",
        "ReferenceId": ecr_ref_num,
        "Tpn": tpn,
        "Authkey": auth_key,
        "PrintReceipt": "Both",
        "GetReceipt": "No",
    });
    let raw = post_spin(terminal, "/Payment/Return", body, config::payment_timeout_ms(), on_state).await?;
    Ok(parse_spin_response(&raw, amount_cents, &ecr_ref_num))
}

pub async fn void_transaction(
    terminal: &Terminal,
    orig_ref_num: String,
    ecr_ref_num: String,
    amount_cents: i64,
    on_state: Option<OnState>,
) -> Result<CreditResponse, PaxError> {
    let (tpn, auth_key) = require_creds(terminal)?;
    let body = json!({
        "Amount": cents_to_dollars(amount_cents),
        "PaymentType": "Credit",
        "ReferenceId": if !orig_ref_num.is_empty() { orig_ref_num } else { ecr_ref_num.clone() },
        "Tpn": tpn,
        "Authkey": auth_key,
    });
    let raw = post_spin(terminal, "/Payment/Void", body, config::payment_timeout_ms(), on_state).await?;
    Ok(parse_spin_response(&raw, amount_cents, &ecr_ref_num))
}

pub async fn batch_close(terminal: &Terminal) -> Result<BatchResponse, PaxError> {
    let (tpn, auth_key) = require_creds(terminal)?;
    let body = json!({ "Tpn": tpn, "Authkey": auth_key });
    let raw = post_spin(terminal, "/Payment/Settle", body, config::payment_timeout_ms(), None).await?;
    let general = raw.get("GeneralResponse").unwrap_or(&raw);
    let code = general.get("StatusCode").and_then(Value::as_str).unwrap_or("").to_string();
    let settled = general.get("ResultCode").and_then(Value::as_str) == Some("0") || code == "0000";
    Ok(BatchResponse {
        result_code: code,
        result_txt: general.get("Message").and_then(Value::as_str).unwrap_or("").to_string(),
        settled,
        batch_number: raw.get("BatchNum").and_then(Value::as_str).unwrap_or("").to_string(),
        credit_count: String::new(),
        credit_amount_cents: 0,
        raw,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_spin_sale_response() {
        let raw = json!({
            "GeneralResponse": { "ResultCode": "0", "StatusCode": "0000", "Message": "Approved" },
            "AuthCode": "654321",
            "ReferenceId": "REF01",
            "RRN": "99887766",
            "CardData": { "Last4": "1111", "CardType": "VISA", "EntryType": "Chip" },
            "Amounts": { "TotalAmount": 19.95, "TipAmount": 2.00 }
        });
        let res = parse_spin_response(&raw, 1995, "REF01");
        assert!(res.approved);
        assert_eq!(res.auth_code, "654321");
        assert_eq!(res.approved_amount_cents, 1995);
        assert_eq!(res.tip_amount_cents, 200);
        assert_eq!(res.last4, "1111");
    }
}
