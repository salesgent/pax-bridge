//! End-to-end mock-terminal integration tests for all supported terminal vendors:
//!   - Valor PayTech VP100 (Local TCP :5000 + out-of-band cancel :5001)
//!   - Clover Flex SNPD (WebSocket /remote_pay: pairing, discovery, PING/PONG,
//!     VERIFY_SIGNATURE, CONFIRM_PAYMENT_MESSAGE, TX_START sale/credit, VOID, BREAK cancel)
//!   - Clover REST Pay Display (HTTP /connect/v1/payments + /device/reset)
//!   - Dejavoo SPIn v2 (HTTP /Payment/Sale)
//!   - NEXO Retailer / ISO 20022 (HTTP Diagnosis + Payment + Abort)
//!   - Legacy PAX DB migration (deserializing records with no vendor/vendorConfig)

use axum::routing::{post, put};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use pax_bridge_desktop_lib::bridge::db::{Db, Terminal};
use pax_bridge_desktop_lib::bridge::drivers;
use pax_bridge_desktop_lib::bridge::protocol::{ETX, STX};
use pax_bridge_desktop_lib::bridge::transport;
use serde_json::{json, Map, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

fn mk_terminal(vendor: &str, model: &str, conn_type: &str, ip: &str, port: u16, vc: Value) -> Terminal {
    Terminal {
        id: format!("term_{}", vendor),
        name: format!("Test {}", model),
        vendor: vendor.into(),
        model: model.into(),
        conn_type: conn_type.into(),
        ip: ip.into(),
        port,
        serial_path: String::new(),
        baud_rate: 115_200,
        vendor_config: vc.as_object().cloned().unwrap_or_else(Map::new),
        created_at: String::new(),
    }
}

// ===========================================================================
// 1. Valor VP100 Local TCP (:5000 + :5001 cancel)
// ===========================================================================

#[tokio::test]
async fn valor_vp100_sale_decline_void_refund_and_cancel() {
    let txn_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let txn_port = txn_listener.local_addr().unwrap().port();
    let cancel_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let cancel_port = cancel_listener.local_addr().unwrap().port();

    let captured_cancel: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let cancel_store = captured_cancel.clone();

    // Cancel port (:5001) mock
    tokio::spawn(async move {
        while let Ok((mut s, _)) = cancel_listener.accept().await {
            let cancel_store = cancel_store.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                if let Ok(n) = s.read(&mut buf).await {
                    if let Ok(Some(v)) = drivers::valor::try_extract_json(&buf[..n]) {
                        *cancel_store.lock().unwrap() = Some(v);
                    }
                    let _ = s.write_all(&drivers::valor::build_frame(&json!({"STATE":"0","MSG":"CANCELED"}))).await;
                }
            });
        }
    });

    // Primary port (:5000) mock
    tokio::spawn(async move {
        while let Ok((mut s, _)) = txn_listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let n = match s.read(&mut buf).await {
                    Ok(n) if n > 0 => n,
                    _ => return,
                };
                let req = match drivers::valor::try_extract_json(&buf[..n]) {
                    Ok(Some(v)) => v,
                    _ => return,
                };
                assert_eq!(buf[0], STX, "Valor request must start with STX");
                assert_eq!(buf[n - 1], ETX, "Valor request must end with ETX");

                let code = req["TRAN_CODE"].as_str().unwrap_or("");
                let amount = req["AMOUNT"].as_str().unwrap_or("0");
                let req_id = req["REQ_TXN_ID"].as_str().unwrap_or("");

                if amount == "9999" {
                    // Simulate slow customer at card prompt for cancel test
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    return;
                }

                let reply = if amount == "555" {
                    // Bare JSON decline (no STX/ETX)
                    serde_json::to_vec(&json!({
                        "STATE": "1",
                        "ERROR_CODE": "05",
                        "ERROR_MSG": "DO NOT HONOR",
                        "REQ_TXN_ID": req_id
                    }))
                    .unwrap()
                } else {
                    drivers::valor::build_frame(&json!({
                        "STATE": "0",
                        "ERROR_CODE": "00",
                        "AUTH_CODE": format!("VP{}", code),
                        "RRN": "0111518118181831",
                        "TXN_ID": "88001",
                        "MASKED_PAN": "************4242",
                        "CARD_LABEL": "VISA",
                        "TRAN_METHOD": "CHIP",
                        "AMOUNT": amount,
                        "REQ_TXN_ID": req_id
                    }))
                };
                let _ = s.write_all(&reply).await;
                let _ = s.flush().await;
            });
        }
    });

    let term = mk_terminal(
        "valor",
        "VP100",
        "tcp",
        "127.0.0.1",
        txn_port,
        json!({ "cancelPort": cancel_port, "epi": "2501411289" }),
    );

    // 1. Diagnose checks BOTH 5000 and 5001
    let diag = transport::diagnose(&term).await.unwrap();
    assert_eq!(diag["tcpOpen"], true);
    assert_eq!(diag["cancelPortOpen"], true);

    // 2. Ping succeeds
    let ping = transport::initialize(&term, None).await.unwrap();
    assert!(ping.approved);
    assert_eq!(ping.serial_number, "2501411289");

    // 3. Sale approved ($25.00 = 2500 cents)
    let sale = transport::sale(&term, 2500, "202611230001".into(), 0, None, None).await.unwrap();
    assert!(sale.approved);
    assert_eq!(sale.auth_code, "VP1");
    assert_eq!(sale.last4, "4242");
    assert_eq!(sale.approved_amount_cents, 2500);
    assert_eq!(sale.ref_num, "0111518118181831");

    // 4. Bare-JSON decline ($5.55)
    let dec = transport::sale(&term, 555, "202611230002".into(), 0, None, None).await.unwrap();
    assert!(!dec.approved);
    assert_eq!(dec.result_code, "05");
    assert_eq!(dec.result_txt, "DO NOT HONOR");

    // 5. Refund (TRAN_CODE=5) & Void (TRAN_CODE=2)
    let rf = transport::refund(&term, 1000, "202611230003".into(), None, None).await.unwrap();
    assert!(rf.approved);
    assert_eq!(rf.auth_code, "VP5");

    let vd = transport::void_transaction(&term, sale.ref_num.clone(), "202611230004".into(), 2500, None, None)
        .await
        .unwrap();
    assert!(vd.approved);
    assert_eq!(vd.auth_code, "VP2");

    // 6. Cancel in-flight sale -> sends TRAN_MODE=99 with REQ_TXN_ID to port 5001
    let slow_term = term.clone();
    let handle = tokio::spawn(async move {
        transport::sale(&slow_term, 9999, "202611230099".into(), 0, None, None).await
    });
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(30)).await;
        if transport::is_busy(&term) {
            break;
        }
    }
    assert!(transport::cancel(&term));
    transport::cancel_on_terminal(&term).await.unwrap();
    let res = tokio::time::timeout(Duration::from_secs(5), handle).await.unwrap().unwrap();
    assert_eq!(res.unwrap_err().code, "CANCELED");

    tokio::time::sleep(Duration::from_millis(100)).await;
    let sent_cancel = captured_cancel.lock().unwrap().clone().expect("cancel payload should arrive on port 5001");
    assert_eq!(sent_cancel["TRAN_MODE"], "99");
    assert_eq!(sent_cancel["REQ_TXN_ID"], "202611230099");
}

// ===========================================================================
// 2. Clover Flex SNPD (WebSocket /remote_pay)
// ===========================================================================

fn clover_env(method: &str, payload: Value) -> String {
    json!({
        "id": "srv1",
        "type": "COMMAND",
        "packageName": drivers::clover::PACKAGE_NAME,
        "method": method,
        "version": 1,
        "payload": serde_json::to_string(&payload).unwrap(),
    })
    .to_string()
}

#[tokio::test]
async fn clover_flex_snpd_pair_sale_signature_confirm_void_refund_and_cancel() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let saw_sig_verified = Arc::new(Mutex::new(false));
    let saw_pay_confirmed = Arc::new(Mutex::new(false));
    let saw_pong = Arc::new(Mutex::new(false));
    let saw_break = Arc::new(Mutex::new(false));

    let s_sig = saw_sig_verified.clone();
    let s_conf = saw_pay_confirmed.clone();
    let s_pong = saw_pong.clone();
    let s_brk = saw_break.clone();

    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let s_sig = s_sig.clone();
            let s_conf = s_conf.clone();
            let s_pong = s_pong.clone();
            let s_brk = s_brk.clone();
            tokio::spawn(async move {
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                while let Some(Ok(Message::Text(txt))) = ws.next().await {
                    let env: Value = serde_json::from_str(&txt).unwrap();
                    if env["type"] == "PONG" {
                        *s_pong.lock().unwrap() = true;
                        continue;
                    }
                    let method = env["method"].as_str().unwrap_or("");
                    let payload: Value = serde_json::from_str(env["payload"].as_str().unwrap_or("{}")).unwrap();

                    match method {
                        "PAIRING_REQUEST" => {
                            let tok = payload.get("authenticationToken").and_then(Value::as_str).unwrap_or("");
                            if tok.is_empty() {
                                let _ = ws.send(Message::Text(clover_env("PAIRING_CODE", json!({"pairingCode": "4829"})).into())).await;
                                tokio::time::sleep(Duration::from_millis(40)).await;
                                let _ = ws
                                    .send(Message::Text(
                                        clover_env(
                                            "PAIRING_RESPONSE",
                                            json!({"pairingState": "PAIRED", "authenticationToken": "clover_tok_42"}),
                                        )
                                        .into(),
                                    ))
                                    .await;
                            } else {
                                let _ = ws
                                    .send(Message::Text(
                                        clover_env(
                                            "PAIRING_RESPONSE",
                                            json!({"pairingState": "PAIRED", "authenticationToken": tok}),
                                        )
                                        .into(),
                                    ))
                                    .await;
                            }
                        }
                        "DISCOVERY_REQUEST" => {
                            let _ = ws
                                .send(Message::Text(
                                    clover_env(
                                        "DISCOVERY_RESPONSE",
                                        json!({
                                            "merchantId": "MID_CLOVER_1",
                                            "deviceInfo": { "serial": "C043UQ1234", "model": "Clover Flex" }
                                        }),
                                    )
                                    .into(),
                                ))
                                .await;
                        }
                        "TX_START" => {
                            let pi = &payload["payIntent"];
                            let amt = pi["amount"].as_i64().unwrap_or(0);
                            let ext_id = pi["externalPaymentId"].as_str().unwrap_or("");
                            let ttype = pi["transactionType"].as_str().unwrap_or("");

                            if amt == 7777 {
                                // Wait for BREAK cancel
                                let _ = ws.send(Message::Text(clover_env("UI_STATE", json!({"uiState": "START"})).into())).await;
                                continue;
                            }

                            let _ = ws.send(Message::Text(clover_env("UI_STATE", json!({"uiState": "START"})).into())).await;
                            let _ = ws.send(Message::Text(json!({"type": "PING"}).to_string().into())).await;
                            let _ = ws.send(Message::Text(clover_env("UI_STATE", json!({"uiState": "PROCESSING"})).into())).await;

                            let payment = json!({
                                "id": "PAY_CLV_1",
                                "order": { "id": "ORD_CLV_1" },
                                "amount": amt.abs(),
                                "tipAmount": pi.get("tipAmount").and_then(Value::as_i64).unwrap_or(0),
                                "result": "SUCCESS",
                                "externalPaymentId": ext_id,
                                "cardTransaction": {
                                    "authCode": "CLV999",
                                    "last4": "8888",
                                    "cardType": "AMEX",
                                    "entryType": "NFC_CONTACTLESS",
                                    "referenceId": "CLVREF1"
                                }
                            });

                            if ttype == "PAYMENT" {
                                // Exercise auto-handling of VERIFY_SIGNATURE + CONFIRM_PAYMENT_MESSAGE
                                let _ = ws.send(Message::Text(clover_env("VERIFY_SIGNATURE", json!({"payment": payment})).into())).await;
                                let _ = ws.send(Message::Text(clover_env("CONFIRM_PAYMENT_MESSAGE", json!({"payment": payment})).into())).await;
                            } else {
                                // CREDIT manual refund must receive negative cents
                                assert!(amt < 0, "Clover CREDIT refund must send negative cents");
                                let _ = ws.send(Message::Text(clover_env("FINISH_OK", json!({"credit": payment})).into())).await;
                            }
                        }
                        "SIGNATURE_VERIFIED" => {
                            assert_eq!(payload["verified"], true);
                            *s_sig.lock().unwrap() = true;
                        }
                        "PAYMENT_CONFIRMED" => {
                            *s_conf.lock().unwrap() = true;
                            let pay = payload["payment"].clone();
                            let _ = ws.send(Message::Text(clover_env("FINISH_OK", json!({"payment": pay})).into())).await;
                        }
                        "VOID_PAYMENT" => {
                            assert_eq!(payload["payment"]["id"], "PAY_CLV_1");
                            assert_eq!(payload["payment"]["order"]["id"], "ORD_CLV_1");
                            let _ = ws
                                .send(Message::Text(
                                    clover_env(
                                        "VOID_PAYMENT_RESPONSE",
                                        json!({"status": "SUCCESS", "payment": payload["payment"]}),
                                    )
                                    .into(),
                                ))
                                .await;
                        }
                        "BREAK" => {
                            *s_brk.lock().unwrap() = true;
                            let _ = ws.send(Message::Text(clover_env("FINISH_CANCEL", json!({"reason": "USER_CANCEL"})).into())).await;
                        }
                        _ => {}
                    }
                }
            });
        }
    });

    // 1. Pair without authToken -> receives PAIRING_CODE "4829" & returns persistent authToken
    let mut term = mk_terminal(
        "clover",
        "Clover Flex",
        "snpd",
        "127.0.0.1",
        port,
        json!({ "insecureWs": true }),
    );
    let received_code = Arc::new(Mutex::new(String::new()));
    let rc = received_code.clone();
    let (token, disc) = transport::pair(
        &term,
        Arc::new(move |code| {
            *rc.lock().unwrap() = code;
        }),
    )
    .await
    .unwrap();
    assert_eq!(*received_code.lock().unwrap(), "4829");
    assert_eq!(token, "clover_tok_42");
    assert_eq!(disc["deviceInfo"]["serial"], "C043UQ1234");

    // Save token and verify silent Ping
    term.vendor_config.insert("authToken".into(), json!(token));
    let info = transport::initialize(&term, None).await.unwrap();
    assert!(info.approved);
    assert_eq!(info.serial_number, "C043UQ1234");

    // 2. Sale -> exercises PING/PONG + VERIFY_SIGNATURE + CONFIRM_PAYMENT_MESSAGE + FINISH_OK
    let sale = transport::sale(&term, 1999, "CLV_SALE_1".into(), 200, None, None).await.unwrap();
    assert!(sale.approved);
    assert_eq!(sale.auth_code, "CLV999");
    assert_eq!(sale.last4, "8888");
    assert_eq!(sale.ref_num, "PAY_CLV_1:ORD_CLV_1");
    assert!(*saw_pong.lock().unwrap(), "bridge must reply PONG to Clover PING");
    assert!(*saw_sig_verified.lock().unwrap(), "bridge must auto-verify Clover signature");
    assert!(*saw_pay_confirmed.lock().unwrap(), "bridge must auto-confirm Clover payment challenges");

    // 3. Void -> passes both paymentId and orderId
    let void_res = transport::void_transaction(&term, sale.ref_num.clone(), "CLV_VOID_1".into(), 1999, None, None)
        .await
        .unwrap();
    assert!(void_res.approved);

    // 4. Manual Refund (CREDIT with negative amount)
    let ref_res = transport::refund(&term, 500, "CLV_REF_1".into(), None, None).await.unwrap();
    assert!(ref_res.approved);
    assert_eq!(ref_res.approved_amount_cents, 500);

    // 5. Cancel -> sends BREAK down live WS
    let cancel_term = term.clone();
    let handle = tokio::spawn(async move {
        transport::sale(&cancel_term, 7777, "CLV_CANCEL_1".into(), 0, None, None).await
    });
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(30)).await;
        if transport::is_busy(&term) {
            break;
        }
    }
    assert!(transport::cancel(&term));
    transport::cancel_on_terminal(&term).await.unwrap();
    let cancel_err = tokio::time::timeout(Duration::from_secs(5), handle).await.unwrap().unwrap().unwrap_err();
    assert_eq!(cancel_err.code, "CANCELED");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(*saw_break.lock().unwrap(), "Clover SNPD cancel must transmit BREAK command");
}

// ===========================================================================
// 3. Clover REST Pay Display + Dejavoo SPIn + NEXO HTTP mocks
// ===========================================================================

#[tokio::test]
async fn clover_rest_dejavoo_spin_and_nexo_end_to_end() {
    let app = Router::new()
        .route(
            "/connect/v1/payments",
            post(|| async {
                Json(json!({
                    "payment": {
                        "id": "RPAY1",
                        "order": { "id": "RORD1" },
                        "amount": 3200,
                        "result": "SUCCESS",
                        "cardTransaction": { "authCode": "REST01", "last4": "5678", "cardType": "VISA" }
                    }
                }))
            }),
        )
        .route("/connect/v1/device/reset", put(|| async { Json(json!({"ok": true})) }))
        .route(
            "/v2/Payment/Sale",
            post(|| async {
                Json(json!({
                    "GeneralResponse": { "ResultCode": "0", "StatusCode": "0000", "Message": "Approved" },
                    "AuthCode": "DJV123",
                    "ReferenceId": "DJREF1",
                    "CardData": { "Last4": "9012", "CardType": "MASTERCARD" },
                    "Amounts": { "TotalAmount": 14.50 }
                }))
            }),
        )
        .route(
            "/nexo",
            post(|| async {
                Json(json!({
                    "SaleToPOIResponse": {
                        "PaymentResponse": {
                            "Response": { "Result": "Success" },
                            "POIData": { "POITransactionID": { "TransactionID": "NX-77" } },
                            "PaymentResult": {
                                "PaymentInstrumentData": { "CardData": { "MaskedPAN": "************3333", "PaymentBrand": "VISA" } },
                                "AmountsResp": { "AuthorizedAmount": 50.00 },
                                "PaymentAcquirerData": { "ApprovalCode": "NXOK99" }
                            }
                        }
                    }
                }))
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Clover REST
    let clover_rest = mk_terminal(
        "clover",
        "Clover Flex",
        "rest",
        "127.0.0.1",
        port,
        json!({ "oauthToken": "test_token", "deviceId": "DEV1", "insecureHttp": true }),
    );
    let cr = transport::sale(&clover_rest, 3200, "CR1".into(), 0, None, None).await.unwrap();
    assert!(cr.approved);
    assert_eq!(cr.auth_code, "REST01");
    transport::cancel_on_terminal(&clover_rest).await.unwrap();

    // Dejavoo SPIn
    let dejavoo = mk_terminal(
        "dejavoo",
        "Z11",
        "tcp",
        "127.0.0.1",
        port,
        json!({ "tpn": "12345678", "authKey": "SECRET" }),
    );
    let dj = transport::sale(&dejavoo, 1450, "DJREF1".into(), 0, None, None).await.unwrap();
    assert!(dj.approved);
    assert_eq!(dj.auth_code, "DJV123");
    assert_eq!(dj.last4, "9012");

    // NEXO Retailer
    let nexo = mk_terminal(
        "nexo",
        "Ingenico",
        "https",
        "127.0.0.1",
        port,
        json!({ "poiId": "P400-1", "insecureHttp": true }),
    );
    let nx = transport::sale(&nexo, 5000, "NX1".into(), 0, None, None).await.unwrap();
    assert!(nx.approved);
    assert_eq!(nx.auth_code, "NXOK99");
    assert_eq!(nx.last4, "3333");
}

// ===========================================================================
// 4. Legacy PAX DB Backwards Compatibility
// ===========================================================================

#[test]
fn legacy_pax_db_loads_without_vendor_field() {
    let dir = std::env::temp_dir().join(format!("pax_legacy_test_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("pax-db.json"),
        r#"{
          "terminals": [{
            "id": "term_old",
            "name": "Front Counter",
            "model": "A920 Pro",
            "connType": "tcp",
            "ip": "192.168.1.50",
            "port": 10009,
            "serialPath": "",
            "baudRate": 115200,
            "createdAt": "2026-01-01T00:00:00Z"
          }],
          "ecrSeq": {}
        }"#,
    )
    .unwrap();

    let db = Db::load(&dir);
    let list = db.list_terminals();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].vendor, "pax");
    assert_eq!(list[0].effective_vendor(), "pax");
    assert!(list[0].vendor_config.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}
