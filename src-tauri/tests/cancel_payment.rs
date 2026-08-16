//! Cancelling stops the POS from waiting, but the command stays on the wire
//! because the terminal keeps its card prompt regardless. These tests use a
//! fake PAX terminal to prove both halves: the caller is released immediately,
//! and a card completed after the cancel still reaches the late-result hook.

use pax_bridge_desktop_lib::bridge::db::Terminal;
use pax_bridge_desktop_lib::bridge::protocol::{self, Field};
use pax_bridge_desktop_lib::bridge::transport;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;

/// Accepts connections and never replies: a card sitting at the prompt.
async fn silent_terminal() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let _held = stream;
                tokio::time::sleep(Duration::from_secs(120)).await;
            });
        }
    });
    port
}

/// Approves the sale after `delay`, i.e. the customer paid anyway.
async fn approving_terminal(delay: Duration) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                let _ = stream.write_all(&approved_t01()).await;
                let _ = stream.flush().await;
                tokio::time::sleep(Duration::from_secs(5)).await;
            });
        }
    });
    port
}

/// A T01 frame the real parser accepts: approved, $110.00, ref number 42.
fn approved_t01() -> Vec<u8> {
    protocol::build_message(&[
        Field::Single("T01".into()),
        Field::Single("1.28".into()),
        Field::Single("000000".into()),
        Field::Single("OK".into()),
        Field::Sub(vec!["0".into(), "APPROVAL".into(), "AUTH123".into(), "HOST9".into()]),
        Field::Single(String::new()),
        Field::Sub(vec!["11000".into(), "0".into()]),
        Field::Sub(vec!["************1234".into(), String::new(), "VISA".into(), "CHIP".into()]),
        Field::Sub(vec!["1".into(), "7".into(), "42".into(), "20260816120000".into()]),
    ])
}

fn terminal_on(port: u16) -> Terminal {
    Terminal {
        id: "t1".into(),
        name: "Fake".into(),
        model: "A35".into(),
        conn_type: "tcp".into(),
        ip: "127.0.0.1".into(),
        port,
        serial_path: String::new(),
        baud_rate: 9600,
        created_at: String::new(),
    }
}

/// Waits until a command is genuinely on the wire.
async fn await_in_flight(terminal: &Terminal) {
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if transport::is_busy(terminal) {
            return;
        }
    }
    panic!("command never became in-flight");
}

#[tokio::test]
async fn cancel_releases_the_caller_without_waiting_for_the_terminal() {
    // Long timeout so the ONLY thing that can end this early is our cancel.
    std::env::set_var("PAX_PAYMENT_TIMEOUT_MS", "120000");

    let terminal = terminal_on(silent_terminal().await);

    let sale_terminal = terminal.clone();
    let started = Instant::now();
    let sale = tokio::spawn(async move { transport::sale(&sale_terminal, 11000, "1".to_string(), 0, None, None).await });

    await_in_flight(&terminal).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(transport::cancel(&terminal), "cancel found nothing in flight");

    let result = tokio::time::timeout(Duration::from_secs(10), sale)
        .await
        .expect("sale did not return after cancel — it hung")
        .unwrap();

    let err = result.expect_err("cancelled sale should not report success");
    assert_eq!(err.code, "CANCELED", "unexpected error: {err:?}");
    assert!(started.elapsed() < Duration::from_secs(30), "cancel did not short-circuit the 120s payment timeout");

    // The prompt is still up on the device, so the terminal stays busy: a new
    // sale must not be sent while the old one can still take a card.
    assert!(transport::is_busy(&terminal), "terminal freed while its card prompt is still live");
}

#[tokio::test]
async fn a_card_completed_after_cancel_reaches_the_late_handler() {
    std::env::set_var("PAX_PAYMENT_TIMEOUT_MS", "120000");

    let terminal = terminal_on(approving_terminal(Duration::from_millis(600)).await);

    let (tx, rx) = mpsc::channel();
    let sale_terminal = terminal.clone();
    let sale = tokio::spawn(async move {
        transport::sale(
            &sale_terminal,
            11000,
            "1".to_string(),
            0,
            None,
            Some(Box::new(move |result| {
                let _ = tx.send(result);
            })),
        )
        .await
    });

    await_in_flight(&terminal).await;
    // Cancel while the customer is still at the prompt.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(transport::cancel(&terminal), "cancel found nothing in flight");

    let err = tokio::time::timeout(Duration::from_secs(10), sale)
        .await
        .expect("sale hung after cancel")
        .unwrap()
        .expect_err("cancelled sale should not report success");
    assert_eq!(err.code, "CANCELED", "unexpected error: {err:?}");

    // The terminal approved after we walked away — the hook must see it so the
    // charge can be voided instead of quietly standing.
    let late = tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(10)))
        .await
        .unwrap()
        .expect("late result never reached the handler")
        .expect("late result should be a parsed approval");

    assert!(late.approved, "late result should be approved: {late:?}");
    assert_eq!(late.approved_amount_cents, 11000);
    assert_eq!(late.ref_num, "42");
}

#[tokio::test]
async fn cancel_with_nothing_in_flight_is_a_noop() {
    let terminal = terminal_on(1);
    assert!(!transport::cancel(&terminal));
}
