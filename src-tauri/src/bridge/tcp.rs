//! TCP transport for the PAX ECR wire protocol — mirrors `paxClient.js`.
//!
//! One connect-write-read cycle per command; the whole operation (connect +
//! write + read-until-complete-frame) is bounded by `timeout_ms`, matching the
//! Node implementation's single `setTimeout` covering the entire exchange.

use crate::bridge::config;
use crate::bridge::protocol::{self, OnState, ParsedResponse, PaxError};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn hex(buf: &[u8]) -> String {
    buf.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" ")
}

fn map_connect_err(err: &std::io::Error) -> PaxError {
    let code = match err.kind() {
        std::io::ErrorKind::ConnectionRefused => "CONNECTION_REFUSED",
        std::io::ErrorKind::TimedOut => "TIMEOUT",
        std::io::ErrorKind::HostUnreachable | std::io::ErrorKind::NetworkUnreachable => "HOST_UNREACHABLE",
        _ => "SOCKET_ERROR",
    };
    let message = match code {
        "CONNECTION_REFUSED" => {
            "Connection refused — BroadPOS is not listening (enable External POS / ECR, Comm=TCP port 10009, leave BroadPOS idle).".to_string()
        }
        "TIMEOUT" => "Connection timed out — terminal may be off, asleep, or on a different network/VLAN.".to_string(),
        "HOST_UNREACHABLE" => "Host unreachable — verify the terminal IP and that it is on the same LAN/subnet as the server.".to_string(),
        _ => format!("Socket error: {}", err),
    };
    PaxError::new(code, message).with_cause(err.to_string())
}

/// Send a command to a TCP terminal and await its framed response.
/// `expected` is the response command code to validate against (e.g. "T01").
pub async fn send_command(
    ip: &str,
    port: u16,
    request: &[u8],
    expected: Option<&str>,
    timeout_ms: u64,
    on_state: Option<OnState>,
) -> Result<ParsedResponse, PaxError> {
    let where_ = format!("{}:{}", ip, port);
    let fut = send_command_inner(ip, port, request, expected, &on_state, &where_);
    match tokio::time::timeout(Duration::from_millis(timeout_ms), fut).await {
        Ok(res) => res,
        Err(_) => Err(PaxError::new("TIMEOUT", format!("No complete response within {}ms", timeout_ms))),
    }
}

async fn send_command_inner(
    ip: &str,
    port: u16,
    request: &[u8],
    expected: Option<&str>,
    on_state: &Option<OnState>,
    where_: &str,
) -> Result<ParsedResponse, PaxError> {
    protocol::fire_state(on_state, "SENDING");
    tracing::debug!("Connecting to terminal {}", where_);

    let mut stream = TcpStream::connect((ip, port)).await.map_err(|e| map_connect_err(&e))?;
    stream.set_nodelay(true).ok();

    if config::send_enq() {
        tracing::debug!("{}  ENQ handshake", where_);
        stream
            .write_all(&[protocol::ENQ])
            .await
            .map_err(|e| PaxError::new("WRITE_FAILED", "Failed to write ENQ").with_cause(e.to_string()))?;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    tracing::debug!("{}  {} bytes RAW: {}", where_, request.len(), hex(request));
    stream
        .write_all(request)
        .await
        .map_err(|e| PaxError::new("WRITE_FAILED", "Failed to write to terminal").with_cause(e.to_string()))?;
    tracing::debug!("{}  waiting for response…", where_);
    protocol::fire_state(on_state, "WAITING");

    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut first_byte = true;

    loop {
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| PaxError::new("SOCKET_ERROR", format!("Socket error: {}", e)))?;
        if n == 0 {
            return Err(PaxError::new("CONNECTION_CLOSED", "Terminal closed the connection before a full response"));
        }
        if first_byte {
            protocol::fire_state(on_state, "RECEIVING");
            first_byte = false;
        }
        tracing::debug!("{}  {} bytes RAW: {}", where_, n, hex(&chunk[..n]));
        buf.extend_from_slice(&chunk[..n]);

        if protocol::has_complete_frame(&buf) {
            let parsed = protocol::parse_response(&buf)?;
            let got = parsed.fields.first().cloned().unwrap_or_default();
            tracing::debug!("{}  response=\"{}\"", where_, got);
            if let Some(exp) = expected {
                if got != exp {
                    return Err(PaxError::new("UNEXPECTED_RESPONSE", format!("Expected {} response, got \"{}\"", exp, got))
                        .with_raw(serde_json::Value::String(parsed.raw.clone())));
                }
            }
            // Classic link: ACK the response frame before closing.
            let _ = stream.write_all(&[protocol::ACK]).await;
            return Ok(parsed);
        } else {
            tracing::debug!("{}  waiting for complete frame ({} bytes so far)", where_, buf.len());
        }
    }
}

/// TCP reachability probe for diagnostics (no protocol bytes sent).
pub async fn probe(ip: &str, port: u16, timeout_ms: u64) -> (bool, bool, Option<String>) {
    match tokio::time::timeout(Duration::from_millis(timeout_ms), TcpStream::connect((ip, port))).await {
        Ok(Ok(_)) => (true, true, None),
        Ok(Err(err)) => match err.kind() {
            std::io::ErrorKind::ConnectionRefused => (false, true, Some("ECONNREFUSED".to_string())),
            std::io::ErrorKind::HostUnreachable | std::io::ErrorKind::NetworkUnreachable => {
                (false, false, Some(format!("{:?}", err.kind())))
            }
            _ => (false, false, Some(err.to_string())),
        },
        Err(_) => (false, false, Some("TIMEOUT".to_string())),
    }
}