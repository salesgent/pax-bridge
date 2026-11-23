//! NEXO Retailer (ISO 20022) driver — Ingenico, Verifone, Adyen, Worldline, Planet, Elavon.
//!
//! Speaks standard `SaleToPOIRequest` / `SaleToPOIResponse` JSON over HTTPS
//! (local LAN `https://<ip>:8443/nexo` or cloud endpoint via `vendorConfig.apiUrl`).

use crate::bridge::config;
use crate::bridge::db::Terminal;
use crate::bridge::protocol::{fire_state, BatchResponse, CreditResponse, InitializeInfo, OnState, PaxError};
use crate::bridge::tcp;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};

pub const DEFAULT_PORT: u16 = 8443;

static ACTIVE_SERVICE_IDS: OnceLock<StdMutex<HashMap<String, String>>> = OnceLock::new();

fn active_service_ids() -> &'static StdMutex<HashMap<String, String>> {
    ACTIVE_SERVICE_IDS.get_or_init(|| StdMutex::new(HashMap::new()))
}

struct ServiceIdGuard(String);
impl Drop for ServiceIdGuard {
    fn drop(&mut self) {
        active_service_ids().lock().unwrap().remove(&self.0);
    }
}

pub fn terminal_key(terminal: &Terminal) -> String {
    if terminal.conn_type == "cloud" {
        format!("nexo:cloud:{}", terminal.config_str("poiId"))
    } else {
        format!("nexo:https:{}:{}", terminal.ip, effective_port(terminal))
    }
}

pub fn effective_port(terminal: &Terminal) -> u16 {
    if terminal.port != 0 { terminal.port } else { DEFAULT_PORT }
}

fn endpoint_url(terminal: &Terminal) -> String {
    let custom = terminal.config_str("apiUrl");
    if !custom.is_empty() {
        return custom;
    }
    let path = {
        let p = terminal.config_str("path");
        if !p.is_empty() { p } else { "/nexo".to_string() }
    };
    let scheme = if terminal.config_bool("insecureHttp") { "http" } else { "https" };
    format!("{}://{}:{}{}", scheme, terminal.ip, effective_port(terminal), path)
}

fn sale_id(terminal: &Terminal) -> String {
    let s = terminal.config_str("saleId");
    if !s.is_empty() { s } else { "SalesgentPOS".to_string() }
}

fn poi_id(terminal: &Terminal) -> String {
    let p = terminal.config_str("poiId");
    if !p.is_empty() { p } else { terminal.name.clone() }
}

fn currency(terminal: &Terminal) -> String {
    let c = terminal.config_str("currency");
    if !c.is_empty() { c.to_uppercase() } else { "USD".to_string() }
}

fn nexo_service_id(ecr_ref_num: &str) -> String {
    // NEXO ServiceID is max 10 alphanumeric chars.
    let digits: String = ecr_ref_num.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    if digits.len() > 10 {
        digits[digits.len() - 10..].to_string()
    } else {
        digits
    }
}

pub fn build_payment_request(terminal: &Terminal, service_id: &str, txn_type: &str, amount_cents: i64, ecr_ref_num: &str) -> Value {
    let now = chrono::Utc::now().to_rfc3339();
    json!({
        "SaleToPOIRequest": {
            "MessageHeader": {
                "ProtocolVersion": "3.0",
                "MessageClass": "Service",
                "MessageCategory": "Payment",
                "MessageType": "Request",
                "ServiceID": service_id,
                "SaleID": sale_id(terminal),
                "POIID": poi_id(terminal),
            },
            "PaymentRequest": {
                "SaleData": {
                    "SaleTransactionID": {
                        "TransactionID": ecr_ref_num,
                        "TimeStamp": now,
                    }
                },
                "PaymentTransaction": {
                    "AmountsReq": {
                        "Currency": currency(terminal),
                        "RequestedAmount": (amount_cents as f64) / 100.0,
                    }
                },
                "PaymentData": {
                    "PaymentType": txn_type,
                }
            }
        }
    })
}

pub fn parse_nexo_payment_response(raw: &Value, requested_cents: i64, ecr_ref_num: &str) -> CreditResponse {
    let resp = raw.get("SaleToPOIResponse").unwrap_or(raw);
    let pay_resp = resp
        .get("PaymentResponse")
        .or_else(|| resp.get("ReversalResponse"))
        .unwrap_or(&Value::Null);
    let result_obj = pay_resp.get("Response").unwrap_or(&Value::Null);
    let result_str = result_obj.get("Result").and_then(Value::as_str).unwrap_or("");
    let err_cond = result_obj.get("ErrorCondition").and_then(Value::as_str).unwrap_or("");
    let addl = result_obj.get("AdditionalResponse").and_then(Value::as_str).unwrap_or("");

    let pay_result = pay_resp.get("PaymentResult").unwrap_or(&Value::Null);
    let inst = pay_result.get("PaymentInstrumentData").and_then(|d| d.get("CardData")).unwrap_or(&Value::Null);
    let acq = pay_result.get("PaymentAcquirerData").unwrap_or(&Value::Null);
    let poi_tx_id = pay_resp
        .get("POIData")
        .and_then(|d| d.get("POITransactionID"))
        .and_then(|t| t.get("TransactionID"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let auth_code = acq.get("ApprovalCode").and_then(Value::as_str).unwrap_or("").to_string();
    let masked_pan = inst.get("MaskedPAN").and_then(Value::as_str).unwrap_or("").to_string();
    let last4: String = {
        let digits: Vec<char> = masked_pan.chars().filter(|c| c.is_ascii_digit()).collect();
        let start = digits.len().saturating_sub(4);
        digits[start..].iter().collect()
    };
    let card_type = inst.get("PaymentBrand").and_then(Value::as_str).unwrap_or("").to_string();
    let entry_mode = inst.get("EntryMode").and_then(Value::as_str).unwrap_or("").to_string();
    let approved_cents = pay_result
        .get("AmountsResp")
        .and_then(|a| a.get("AuthorizedAmount"))
        .and_then(Value::as_f64)
        .map(|d| (d * 100.0).round() as i64)
        .unwrap_or(requested_cents);

    let approved = result_str.eq_ignore_ascii_case("Success");

    CreditResponse {
        result_code: if approved { "000000".into() } else if !err_cond.is_empty() { err_cond.into() } else { result_str.into() },
        result_txt: if approved { "APPROVED".into() } else if !addl.is_empty() { addl.into() } else { err_cond.into() },
        host_response_code: err_cond.to_string(),
        host_response_text: addl.to_string(),
        approved,
        offline_approval: false,
        auth_code,
        host_ref_num: acq.get("AcquirerTransactionID").and_then(|t| t.get("TransactionID")).and_then(Value::as_str).unwrap_or("").to_string(),
        ref_num: poi_tx_id.clone(),
        transaction_num: poi_tx_id,
        ecr_ref_num: ecr_ref_num.to_string(),
        masked_pan,
        last4,
        card_type,
        entry_mode,
        approved_amount_cents: if approved { approved_cents } else { 0 },
        tip_amount_cents: 0,
        timestamp: String::new(),
        raw: raw.clone(),
    }
}

async fn send_nexo(terminal: &Terminal, body: Value, timeout_ms: u64, on_state: Option<OnState>) -> Result<Value, PaxError> {
    fire_state(&on_state, "SENDING");
    let url = endpoint_url(terminal);
    let is_local = terminal.conn_type != "cloud";
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .danger_accept_invalid_certs(is_local)
        .build()
        .map_err(|e| PaxError::new("SOCKET_ERROR", format!("NEXO HTTP client error: {}", e)))?;

    let mut req = client.post(&url).json(&body);
    let api_key = terminal.config_str("apiKey");
    if !api_key.is_empty() {
        req = req.header("X-API-Key", api_key);
    }

    fire_state(&on_state, "WAITING");
    let resp = req.send().await.map_err(|e| {
        if e.is_timeout() {
            PaxError::new("TIMEOUT", format!("NEXO terminal timed out after {}ms", timeout_ms))
        } else {
            PaxError::new("HOST_UNREACHABLE", format!("Could not reach NEXO terminal ({}): {}", url, e))
        }
    })?;

    fire_state(&on_state, "RECEIVING");
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let val: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "rawText": text }));
    if !status.is_success() {
        return Err(PaxError::new("SOCKET_ERROR", format!("NEXO endpoint returned HTTP {}", status.as_u16())));
    }
    Ok(val)
}

pub async fn initialize(terminal: &Terminal) -> Result<InitializeInfo, PaxError> {
    let started = Instant::now();
    let sid = nexo_service_id(&chrono::Utc::now().timestamp_millis().to_string());
    let req = json!({
        "SaleToPOIRequest": {
            "MessageHeader": {
                "ProtocolVersion": "3.0",
                "MessageClass": "Service",
                "MessageCategory": "Diagnosis",
                "MessageType": "Request",
                "ServiceID": sid,
                "SaleID": sale_id(terminal),
                "POIID": poi_id(terminal),
            },
            "DiagnosisRequest": { "HostDiagnosisFlag": false }
        }
    });
    let raw = send_nexo(terminal, req, config::ping_timeout_ms(), None).await?;
    let ok = raw
        .pointer("/SaleToPOIResponse/DiagnosisResponse/Response/Result")
        .and_then(Value::as_str)
        .map(|s| s.eq_ignore_ascii_case("Success"))
        .unwrap_or(true);
    Ok(InitializeInfo {
        result_code: if ok { "000000".into() } else { "ERROR".into() },
        result_txt: if ok { "NEXO online".into() } else { "Diagnosis failed".into() },
        approved: ok,
        serial_number: poi_id(terminal),
        model: terminal.model.clone(),
        app_version: "NEXO 3.0".into(),
        mac_address: String::new(),
        raw,
        latency_ms: started.elapsed().as_millis() as i64,
    })
}

pub async fn diagnose(terminal: &Terminal) -> Result<Value, PaxError> {
    if terminal.conn_type == "cloud" {
        let ok = !terminal.config_str("apiUrl").is_empty() && !terminal.config_str("poiId").is_empty();
        return Ok(json!({
            "vendor": "nexo",
            "connType": "cloud",
            "poiId": poi_id(terminal),
            "ecrLikelyListening": ok,
            "nextSteps": if ok {
                vec!["NEXO Cloud endpoint and POI ID configured. Run Test Connection."]
            } else {
                vec!["Enter the NEXO Cloud API URL, API Key, and POI ID."]
            }
        }));
    }
    let port = effective_port(terminal);
    let (open, host_up, err) = tcp::probe(&terminal.ip, port, 3000).await;
    Ok(json!({
        "vendor": "nexo",
        "connType": terminal.conn_type,
        "ip": terminal.ip,
        "port": port,
        "hostReachable": host_up,
        "tcpOpen": open,
        "tcpError": err,
        "ecrLikelyListening": open,
        "nextSteps": if open {
            vec!["NEXO local port is open. Run Test Connection."]
        } else {
            vec!["Enable Local Communications / NEXO Retailer API on the terminal (default port 8443) and verify same LAN."]
        }
    }))
}

pub async fn sale(
    terminal: &Terminal,
    amount_cents: i64,
    ecr_ref_num: String,
    _tip_cents: i64,
    on_state: Option<OnState>,
) -> Result<CreditResponse, PaxError> {
    let key = terminal_key(terminal);
    let sid = nexo_service_id(&ecr_ref_num);
    active_service_ids().lock().unwrap().insert(key.clone(), sid.clone());
    let _guard = ServiceIdGuard(key);

    let req = build_payment_request(terminal, &sid, "Normal", amount_cents, &ecr_ref_num);
    let raw = send_nexo(terminal, req, config::payment_timeout_ms(), on_state).await?;
    Ok(parse_nexo_payment_response(&raw, amount_cents, &ecr_ref_num))
}

pub async fn refund(
    terminal: &Terminal,
    amount_cents: i64,
    ecr_ref_num: String,
    on_state: Option<OnState>,
) -> Result<CreditResponse, PaxError> {
    let key = terminal_key(terminal);
    let sid = nexo_service_id(&ecr_ref_num);
    active_service_ids().lock().unwrap().insert(key.clone(), sid.clone());
    let _guard = ServiceIdGuard(key);

    let req = build_payment_request(terminal, &sid, "Refund", amount_cents, &ecr_ref_num);
    let raw = send_nexo(terminal, req, config::payment_timeout_ms(), on_state).await?;
    Ok(parse_nexo_payment_response(&raw, amount_cents, &ecr_ref_num))
}

pub async fn void_transaction(
    terminal: &Terminal,
    orig_ref_num: String,
    ecr_ref_num: String,
    amount_cents: i64,
    on_state: Option<OnState>,
) -> Result<CreditResponse, PaxError> {
    let sid = nexo_service_id(&ecr_ref_num);
    let req = json!({
        "SaleToPOIRequest": {
            "MessageHeader": {
                "ProtocolVersion": "3.0",
                "MessageClass": "Service",
                "MessageCategory": "Reversal",
                "MessageType": "Request",
                "ServiceID": sid,
                "SaleID": sale_id(terminal),
                "POIID": poi_id(terminal),
            },
            "ReversalRequest": {
                "OriginalPOITransaction": {
                    "POITransactionID": {
                        "TransactionID": orig_ref_num,
                        "TimeStamp": chrono::Utc::now().to_rfc3339(),
                    }
                },
                "ReversalReason": "MerchantCancel",
                "ReversedAmount": (amount_cents as f64) / 100.0,
            }
        }
    });
    let raw = send_nexo(terminal, req, config::payment_timeout_ms(), on_state).await?;
    Ok(parse_nexo_payment_response(&raw, amount_cents, &ecr_ref_num))
}

pub async fn cancel_on_terminal(terminal: &Terminal) -> Result<(), PaxError> {
    let key = terminal_key(terminal);
    let ref_sid = active_service_ids().lock().unwrap().get(&key).cloned().unwrap_or_default();
    let sid = nexo_service_id(&chrono::Utc::now().timestamp_millis().to_string());
    let req = json!({
        "SaleToPOIRequest": {
            "MessageHeader": {
                "ProtocolVersion": "3.0",
                "MessageClass": "Service",
                "MessageCategory": "Abort",
                "MessageType": "Request",
                "ServiceID": sid,
                "SaleID": sale_id(terminal),
                "POIID": poi_id(terminal),
            },
            "AbortRequest": {
                "MessageReference": {
                    "MessageCategory": "Payment",
                    "SaleID": sale_id(terminal),
                    "ServiceID": ref_sid,
                },
                "AbortReason": "MerchantAbort"
            }
        }
    });
    let _ = send_nexo(terminal, req, 10_000, None).await?;
    Ok(())
}

pub async fn batch_close(terminal: &Terminal) -> Result<BatchResponse, PaxError> {
    let sid = nexo_service_id(&chrono::Utc::now().timestamp_millis().to_string());
    let req = json!({
        "SaleToPOIRequest": {
            "MessageHeader": {
                "ProtocolVersion": "3.0",
                "MessageClass": "Service",
                "MessageCategory": "Reconciliation",
                "MessageType": "Request",
                "ServiceID": sid,
                "SaleID": sale_id(terminal),
                "POIID": poi_id(terminal),
            },
            "ReconciliationRequest": { "ReconciliationType": "SaleReconciliation" }
        }
    });
    let raw = send_nexo(terminal, req, config::payment_timeout_ms(), None).await?;
    let settled = raw
        .pointer("/SaleToPOIResponse/ReconciliationResponse/Response/Result")
        .and_then(Value::as_str)
        .map(|s| s.eq_ignore_ascii_case("Success"))
        .unwrap_or(false);
    Ok(BatchResponse {
        result_code: if settled { "000000".into() } else { "ERROR".into() },
        result_txt: if settled { "RECONCILED".into() } else { "FAILED".into() },
        settled,
        batch_number: raw
            .pointer("/SaleToPOIResponse/ReconciliationResponse/POIReconciliationID")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        credit_count: String::new(),
        credit_amount_cents: 0,
        raw,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nexo_payment_success() {
        let raw = json!({
            "SaleToPOIResponse": {
                "PaymentResponse": {
                    "Response": { "Result": "Success" },
                    "POIData": { "POITransactionID": { "TransactionID": "POI-TX-99" } },
                    "PaymentResult": {
                        "PaymentInstrumentData": {
                            "CardData": { "MaskedPAN": "411111******1111", "PaymentBrand": "VISA", "EntryMode": "ICC" }
                        },
                        "AmountsResp": { "AuthorizedAmount": 42.50 },
                        "PaymentAcquirerData": { "ApprovalCode": "NX1234" }
                    }
                }
            }
        });
        let res = parse_nexo_payment_response(&raw, 4250, "E1");
        assert!(res.approved);
        assert_eq!(res.auth_code, "NX1234");
        assert_eq!(res.last4, "1111");
        assert_eq!(res.approved_amount_cents, 4250);
    }
}
