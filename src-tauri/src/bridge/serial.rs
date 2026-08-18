//! USB / serial transport for the PAX ECR wire protocol — mirrors `paxSerialClient.js`.
//!
//! The wire protocol is IDENTICAL to TCP (STX/ETX/LRC, FS/US) — only the pipe
//! changes — so we reuse `protocol::build_message` / `parse_response` /
//! `has_complete_frame`. The `serialport` crate is blocking, so all I/O runs
//! on a blocking thread via `spawn_blocking`.

use crate::bridge::config;
use crate::bridge::protocol::{self, OnState, ParsedResponse, PaxError};
use serde::Serialize;
use std::io::{Read, Write};
use std::time::{Duration, Instant};

const DEFAULT_BAUD: u32 = 115_200;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SerialPortInfo {
    pub path: String,
    pub manufacturer: String,
    pub vendor_id: String,
    pub product_id: String,
    pub serial_number: String,
}

fn hex(buf: &[u8]) -> String {
    buf.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" ")
}

/// List available serial ports (for the UI's "detect USB device" helper).
pub async fn list_ports() -> Result<Vec<SerialPortInfo>, PaxError> {
    tokio::task::spawn_blocking(list_ports_blocking)
        .await
        .map_err(|e| PaxError::new("SERIAL_UNAVAILABLE", format!("Serial task failed: {}", e)))?
}

fn list_ports_blocking() -> Result<Vec<SerialPortInfo>, PaxError> {
    let ports = serialport::available_ports()
        .map_err(|e| PaxError::new("SERIAL_UNAVAILABLE", format!("Serial support is not installed or failed to load: {}", e)))?;

    Ok(ports
        .into_iter()
        .map(|p| {
            let (manufacturer, vendor_id, product_id, serial_number) = match p.port_type {
                serialport::SerialPortType::UsbPort(info) => (
                    info.manufacturer.unwrap_or_default(),
                    format!("{:04x}", info.vid),
                    format!("{:04x}", info.pid),
                    info.serial_number.unwrap_or_default(),
                ),
                _ => (String::new(), String::new(), String::new(), String::new()),
            };
            SerialPortInfo { path: p.port_name, manufacturer, vendor_id, product_id, serial_number }
        })
        .collect())
}

fn classify_open_err(path: &str, err: &serialport::Error) -> PaxError {
    PaxError::new(
        "SERIAL_OPEN_FAILED",
        format!("Cannot open {} — is the terminal plugged in and in USB/ECR mode? ({})", path, err),
    )
}

fn classify_runtime_err(path: &str, msg: &str) -> PaxError {
    let lower = msg.to_lowercase();
    if lower.contains("no such file") || lower.contains("cannot open") || lower.contains("access denied") || lower.contains("permission") {
        PaxError::new(
            "SERIAL_OPEN_FAILED",
            format!(
                "Cannot open {} — check the USB cable and that the terminal's payment app is set to USB/ECR mode. ({})",
                path, msg
            ),
        )
    } else {
        PaxError::new("SERIAL_ERROR", format!("Serial error on {}: {}", path, msg))
    }
}

/// Send a command over serial and await its framed response.
pub async fn send_command(
    path: &str,
    baud_rate: u32,
    request: Vec<u8>,
    expected: Option<String>,
    timeout_ms: u64,
    on_state: Option<OnState>,
) -> Result<ParsedResponse, PaxError> {
    let path_owned = path.to_string();
    let baud = if baud_rate == 0 { DEFAULT_BAUD } else { baud_rate };
    let send_enq = config::send_enq();

    let handle = tokio::task::spawn_blocking(move || -> Result<ParsedResponse, PaxError> {
        send_command_blocking(&path_owned, baud, &request, expected.as_deref(), timeout_ms, send_enq, &on_state)
    });

    // A little slack over timeout_ms for the blocking-thread join overhead;
    // the inner deadline already enforces the real timeout.
    match tokio::time::timeout(Duration::from_millis(timeout_ms + 1000), handle).await {
        Ok(Ok(res)) => res,
        Ok(Err(join_err)) => Err(PaxError::new("SERIAL_ERROR", format!("Serial task failed: {}", join_err))),
        Err(_) => Err(PaxError::new("TIMEOUT", format!("No complete response within {}ms", timeout_ms))),
    }
}

fn send_command_blocking(
    path: &str,
    baud_rate: u32,
    request: &[u8],
    expected: Option<&str>,
    timeout_ms: u64,
    send_enq: bool,
    on_state: &Option<OnState>,
) -> Result<ParsedResponse, PaxError> {
    protocol::fire_state(on_state, "SENDING");

    let mut port = serialport::new(path, baud_rate)
        .timeout(Duration::from_millis(200))
        .open()
        .map_err(|e| classify_open_err(path, &e))?;

    if send_enq {
        port.write_all(&[protocol::ENQ]).map_err(|e| classify_runtime_err(path, &format!("Failed to write ENQ: {}", e)))?;
        std::thread::sleep(Duration::from_millis(50));
    }

    tracing::debug!("{}  {} bytes RAW: {}", path, request.len(), hex(request));
    port.write_all(request)
        .map_err(|e| classify_runtime_err(path, &format!("Failed to write to {}: {}", path, e)))?;
    let _ = port.flush();
    protocol::fire_state(on_state, "WAITING");

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut buf: Vec<u8> = Vec::new();
    let mut first_byte = true;
    let mut chunk = [0u8; 4096];

    loop {
        if Instant::now() >= deadline {
            return Err(PaxError::new("TIMEOUT", format!("No complete response within {}ms", timeout_ms)));
        }
        match port.read(&mut chunk) {
            Ok(0) => { /* nothing this poll; keep going until deadline */ }
            Ok(n) => {
                if first_byte {
                    protocol::fire_state(on_state, "RECEIVING");
                    first_byte = false;
                }
                tracing::debug!("{}  {} bytes RAW: {}", path, n, hex(&chunk[..n]));
                buf.extend_from_slice(&chunk[..n]);
                if protocol::has_complete_frame(&buf) {
                    let parsed = protocol::parse_response(&buf)?;
                    let got = parsed.fields.first().cloned().unwrap_or_default();
                    if let Some(exp) = expected {
                        if got != exp {
                            return Err(PaxError::new("UNEXPECTED_RESPONSE", format!("Expected {} response, got \"{}\"", exp, got))
                                .with_raw(serde_json::Value::String(parsed.raw.clone())));
                        }
                    }
                    let _ = port.write_all(&[protocol::ACK]);
                    return Ok(parsed);
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => { /* poll timeout, loop again */ }
            Err(e) => return Err(classify_runtime_err(path, &e.to_string())),
        }
    }
}
