//! Multi-vendor payment terminal driver registry & self-describing catalog.
//!
//! Exposes `driver_catalog()` for `GET /api/drivers` so both `erp-frontend` and
//! `reatil-payment-working` dynamically render supported vendors, models,
//! connection types, default ports, config fields, and setup checklists without
//! hardcoding terminal types in the frontend.

pub mod clover;
pub mod dejavoo;
pub mod nexo;
pub mod valor;

use crate::bridge::db::{infer_vendor, Terminal};
use serde_json::{json, Value};

/// Complete self-describing catalog returned by `GET /api/drivers`.
pub fn driver_catalog() -> Value {
    json!({
        "drivers": [
            {
                "vendor": "pax",
                "label": "PAX (BroadPOS / POSLink)",
                "models": ["A920 Pro", "A35", "A80", "S300", "E500", "E700"],
                "defaultModel": "A920 Pro",
                "defaultConnType": "tcp",
                "connTypes": [
                    { "id": "tcp", "label": "LAN (Wi-Fi / Ethernet)", "defaultPort": 10009, "requiresIp": true, "requiresPort": true },
                    { "id": "usb", "label": "USB Serial", "requiresSerial": true }
                ],
                "capabilities": { "cancel": true, "refund": true, "void": true, "batchClose": true, "pair": false, "diagnose": true },
                "fields": [],
                "setupSteps": [
                    "Open BroadPOS TSYS Sierra on the PAX terminal and leave it on the idle screen.",
                    "Settings (squares) → password = today's date MMDDYYYY → System Settings → ECR-Terminal Integration Mode = External POS.",
                    "ECR Comm Settings → Protocol Type = TCP/IP, Host Port = 10009 (or USB if cabled).",
                    "Note the terminal's local Wi-Fi/LAN IP and click Diagnose → Test Connection."
                ]
            },
            {
                "vendor": "valor",
                "label": "Valor PayTech (VP100 / VL-Series)",
                "models": ["VP100", "VL100", "VL110", "VP300", "VP500", "VL500", "VL550"],
                "defaultModel": "VP100",
                "defaultConnType": "tcp",
                "connTypes": [
                    { "id": "tcp", "label": "Local LAN (TCP :5000)", "defaultPort": 5000, "requiresIp": true, "requiresPort": true },
                    { "id": "cloud", "label": "Valor Connect Cloud (EPI)", "requiresIp": false, "requiresPort": false },
                    { "id": "usb", "label": "USB Serial", "requiresSerial": true }
                ],
                "capabilities": { "cancel": true, "refund": true, "void": true, "batchClose": true, "pair": false, "diagnose": true },
                "fields": [
                    { "key": "cancelPort", "label": "Cancel port", "type": "number", "default": 5001, "connTypes": ["tcp"], "placeholder": "5001" },
                    { "key": "epi", "label": "Terminal ID (EPI)", "type": "text", "connTypes": ["tcp", "cloud", "usb"], "requiredFor": ["cloud"], "placeholder": "2501411289" },
                    { "key": "appId", "label": "App ID", "type": "text", "connTypes": ["cloud"], "requiredFor": ["cloud"], "placeholder": "Valor Portal App ID" },
                    { "key": "appKey", "label": "App Key", "type": "password", "connTypes": ["cloud"], "requiredFor": ["cloud"], "placeholder": "Valor Portal App Key" },
                    { "key": "channelId", "label": "Channel ID", "type": "text", "connTypes": ["cloud"], "placeholder": "Valor Portal Channel ID" },
                    { "key": "environment", "label": "Environment", "type": "select", "options": ["production", "sandbox"], "default": "production", "connTypes": ["cloud"] }
                ],
                "setupSteps": [
                    "In Valor Portal: Device Management → locate your EPI → Edit Parameters → Terminal & Transaction → Valor Connect = Yes, Connection = TCP (or Cloud).",
                    "On the VP100: tap Menu (≡) → Download Package → OK, then reboot the terminal.",
                    "Confirm the VP100 idle screen shows its local IP and 'Server is Waiting for Transaction' (TCP mode listens on port 5000, cancel on 5001).",
                    "Enter the terminal IP + port 5000 here and click Diagnose → Test Connection."
                ]
            },
            {
                "vendor": "clover",
                "label": "Clover (Flex / Mini / Station)",
                "models": ["Clover Flex", "Clover Mini", "Clover Compact", "Clover Station"],
                "defaultModel": "Clover Flex",
                "defaultConnType": "snpd",
                "connTypes": [
                    { "id": "snpd", "label": "Local LAN — Secure Network Pay Display (:12345)", "defaultPort": 12345, "requiresIp": true, "requiresPort": true },
                    { "id": "rest", "label": "Local LAN — REST Pay Display (:12346)", "defaultPort": 12346, "requiresIp": true, "requiresPort": true },
                    { "id": "cloud", "label": "Clover Cloud — REST Pay Display", "requiresIp": false, "requiresPort": false }
                ],
                "capabilities": { "cancel": true, "refund": true, "void": true, "batchClose": true, "pair": true, "diagnose": true },
                "fields": [
                    { "key": "posName", "label": "POS name", "type": "text", "default": "Salesgent POS", "connTypes": ["snpd"] },
                    { "key": "oauthToken", "label": "OAuth Bearer Token", "type": "password", "connTypes": ["rest", "cloud"], "requiredFor": ["rest", "cloud"] },
                    { "key": "deviceId", "label": "Clover Device Serial (X-Clover-Device-Id)", "type": "text", "connTypes": ["rest", "cloud"], "requiredFor": ["cloud"] },
                    { "key": "posId", "label": "POS ID (X-POS-Id)", "type": "text", "default": "SalesgentPOS", "connTypes": ["rest", "cloud"] },
                    { "key": "environment", "label": "Environment", "type": "select", "options": ["production", "sandbox"], "default": "production", "connTypes": ["cloud"] }
                ],
                "setupSteps": [
                    "For Local SNPD (no OAuth required): On the Clover Flex, open App Market → install & launch 'Secure Network Pay Display' → tap Start.",
                    "Enter the Clover Flex Wi-Fi IP and port 12345, save the terminal, then click Pair.",
                    "Type the 4-digit pairing code shown on this screen into the Clover Flex to complete one-time pairing.",
                    "For REST Pay Display (:12346 or Cloud): install 'REST Pay Display' on the Flex and provide your Clover OAuth Bearer Token + Device Serial."
                ]
            },
            {
                "vendor": "dejavoo",
                "label": "Dejavoo (SPIn v2)",
                "models": ["Z8", "Z9", "Z11", "QD2", "QD4", "P1", "P3", "P8"],
                "defaultModel": "Z11",
                "defaultConnType": "cloud",
                "connTypes": [
                    { "id": "cloud", "label": "SPIn Cloud (TPN + AuthKey)", "requiresIp": false, "requiresPort": false },
                    { "id": "tcp", "label": "SPIn Local LAN (:8080)", "defaultPort": 8080, "requiresIp": true, "requiresPort": true }
                ],
                "capabilities": { "cancel": false, "refund": true, "void": true, "batchClose": true, "pair": false, "diagnose": true },
                "fields": [
                    { "key": "tpn", "label": "TPN (Terminal Profile Number)", "type": "text", "connTypes": ["cloud", "tcp"], "requiredFor": ["cloud", "tcp"] },
                    { "key": "authKey", "label": "Auth Key", "type": "password", "connTypes": ["cloud", "tcp"], "requiredFor": ["cloud", "tcp"] },
                    { "key": "environment", "label": "Environment", "type": "select", "options": ["production", "sandbox"], "default": "production", "connTypes": ["cloud"] }
                ],
                "setupSteps": [
                    "Enable SPIn integration for the terminal in the Dejavoo STEAM / iPOSpays portal.",
                    "Enter the terminal's TPN and Auth Key (plus local IP if using SPIn Local LAN).",
                    "Click Test Connection to verify."
                ]
            },
            {
                "vendor": "nexo",
                "label": "NEXO Retailer / ISO 20022 (Ingenico, Verifone, Adyen)",
                "models": ["Ingenico", "Verifone", "Adyen", "Worldline", "Generic NEXO"],
                "defaultModel": "Ingenico",
                "defaultConnType": "https",
                "connTypes": [
                    { "id": "https", "label": "Local LAN HTTPS (:8443)", "defaultPort": 8443, "requiresIp": true, "requiresPort": true },
                    { "id": "cloud", "label": "Cloud NEXO API", "requiresIp": false, "requiresPort": false }
                ],
                "capabilities": { "cancel": true, "refund": true, "void": true, "batchClose": true, "pair": false, "diagnose": true },
                "fields": [
                    { "key": "poiId", "label": "POI ID (Terminal ID)", "type": "text", "connTypes": ["https", "cloud"], "requiredFor": ["cloud"] },
                    { "key": "saleId", "label": "Sale ID", "type": "text", "default": "SalesgentPOS", "connTypes": ["https", "cloud"] },
                    { "key": "apiKey", "label": "API Key (optional)", "type": "password", "connTypes": ["https", "cloud"] },
                    { "key": "apiUrl", "label": "Endpoint URL (Cloud)", "type": "text", "connTypes": ["cloud"], "requiredFor": ["cloud"], "placeholder": "https://terminal-api-live.adyen.com/sync" },
                    { "key": "currency", "label": "Currency", "type": "text", "default": "USD", "connTypes": ["https", "cloud"] }
                ],
                "setupSteps": [
                    "Enable Local Communications / NEXO Retailer 3.0 on the Ingenico, Verifone, or Adyen terminal.",
                    "Enter the terminal IP + port (default 8443) and POI ID, then click Test Connection."
                ]
            }
        ]
    })
}

pub fn allowed_conn_types(vendor: &str) -> &'static [&'static str] {
    match vendor {
        "valor" => &["tcp", "usb", "cloud"],
        "clover" => &["snpd", "tcp", "rest", "cloud"],
        "dejavoo" => &["cloud", "tcp"],
        "nexo" => &["https", "tcp", "cloud"],
        _ => &["tcp", "usb"],
    }
}

pub fn validate_terminal(body: &Value, partial: bool) -> Vec<String> {
    let mut errors = Vec::new();

    let name_present = body.get("name").is_some();
    if !partial || name_present {
        let empty = body.get("name").and_then(Value::as_str).map(|s| s.trim().is_empty()).unwrap_or(true);
        if empty {
            errors.push("name is required".to_string());
        }
    }

    let model = body.get("model").and_then(Value::as_str).unwrap_or("A920 Pro");
    let vendor = infer_vendor(body.get("vendor").and_then(Value::as_str), model);
    if let Some(v) = body.get("vendor").and_then(Value::as_str) {
        let low = v.trim().to_ascii_lowercase();
        if !matches!(low.as_str(), "pax" | "valor" | "clover" | "dejavoo" | "nexo") {
            errors.push("vendor must be one of: pax, valor, clover, dejavoo, nexo".to_string());
        }
    }

    let conn_field = body.get("connType").and_then(Value::as_str).map(|s| s.trim().to_ascii_lowercase());
    if let Some(ref ct) = conn_field {
        let allowed = allowed_conn_types(&vendor);
        if !allowed.contains(&ct.as_str()) {
            errors.push(format!("connType '{}' is not valid for vendor '{}' (allowed: {})", ct, vendor, allowed.join(", ")));
        }
    }

    let effective_conn = conn_field.as_deref().or_else(|| {
        if partial {
            None
        } else {
            Some(crate::bridge::db::default_conn_type_for(&vendor))
        }
    });

    match effective_conn {
        Some("usb") => {
            if !partial || body.get("serialPath").is_some() || conn_field.is_some() {
                let empty = body.get("serialPath").and_then(Value::as_str).map(|s| s.trim().is_empty()).unwrap_or(true);
                if empty {
                    errors.push("serialPath is required for USB terminals".to_string());
                }
            }
        }
        Some("tcp") | Some("snpd") | Some("rest") | Some("https") => {
            if !partial || body.get("ip").is_some() || conn_field.is_some() {
                let empty = body.get("ip").and_then(Value::as_str).map(|s| s.trim().is_empty()).unwrap_or(true);
                if empty {
                    errors.push("ip is required for LAN terminals".to_string());
                }
            }
        }
        Some("cloud") => {
            let vc = body.get("vendorConfig").and_then(Value::as_object);
            let has = |k: &str| vc.and_then(|m| m.get(k)).and_then(Value::as_str).map(|s| !s.trim().is_empty()).unwrap_or(false);
            if !partial {
                match vendor.as_str() {
                    "valor" => {
                        if !has("epi") || !has("appId") || !has("appKey") {
                            errors.push("Valor Connect Cloud requires vendorConfig.epi, appId, and appKey".to_string());
                        }
                    }
                    "clover" => {
                        if !has("oauthToken") || !has("deviceId") {
                            errors.push("Clover Cloud REST requires vendorConfig.oauthToken and deviceId".to_string());
                        }
                    }
                    "dejavoo" => {
                        if !has("tpn") || !has("authKey") {
                            errors.push("Dejavoo SPIn Cloud requires vendorConfig.tpn and authKey".to_string());
                        }
                    }
                    "nexo" => {
                        if !has("apiUrl") || !has("poiId") {
                            errors.push("NEXO Cloud requires vendorConfig.apiUrl and poiId".to_string());
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }

    errors
}

/// Stable concurrency queue key for any terminal regardless of vendor.
pub fn queue_key(terminal: &Terminal) -> String {
    match terminal.effective_vendor() {
        "valor" => valor::terminal_key(terminal),
        "clover" => clover::terminal_key(terminal),
        "dejavoo" => dejavoo::terminal_key(terminal),
        "nexo" => nexo::terminal_key(terminal),
        _ => {
            if terminal.conn_type == "usb" {
                format!("usb:{}", terminal.serial_path)
            } else {
                format!("tcp:{}:{}", terminal.ip, terminal.port)
            }
        }
    }
}
