//! Printing through a printer the operating system already knows about.
//!
//! This is the path that keeps a till usable. The bytes are handed to the
//! spooler as RAW, which means they travel through the printer's own vendor
//! driver without being interpreted — a complete ESC/POS receipt, cash drawer
//! kick and all, arrives exactly as built. Because the driver stays in place,
//! every other program on the machine can still print to the same printer.
//!
//! The alternative — claiming the USB device directly — requires replacing the
//! vendor driver with WinUSB, which silently removes the printer from every
//! other application on that computer. That trade is not worth making, so it is
//! not offered here.

use crate::bridge::protocol::PaxError;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemPrinter {
    pub name: String,
    pub is_default: bool,
}

/// Printers installed on this computer.
pub async fn list() -> Result<Vec<SystemPrinter>, PaxError> {
    tokio::task::spawn_blocking(imp::list_blocking)
        .await
        .map_err(|e| PaxError::new("PRINTER_UNAVAILABLE", format!("Could not list printers: {}", e)))?
}

/// Send raw bytes to a named printer.
pub async fn print_raw(name: String, payload: Vec<u8>) -> Result<(), PaxError> {
    tokio::task::spawn_blocking(move || imp::print_raw_blocking(&name, &payload))
        .await
        .map_err(|e| PaxError::new("PRINTER_WRITE_FAILED", format!("Print task failed: {}", e)))?
}

fn not_found(name: &str) -> PaxError {
    PaxError::new(
        "PRINTER_NOT_FOUND",
        format!(
            "No printer named \"{}\" on this computer. Check the name in the operating system's printer settings — it must match exactly.",
            name
        ),
    )
}

// ---------------------------------------------------------------------------
// Windows — the print spooler, addressed directly
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use super::{not_found, PaxError, SystemPrinter};
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Graphics::Printing::{
        ClosePrinter, EndDocPrinter, EndPagePrinter, EnumPrintersW, GetDefaultPrinterW, OpenPrinterW,
        StartDocPrinterW, StartPagePrinter, WritePrinter, DOC_INFO_1W, PRINTER_ENUM_CONNECTIONS,
        PRINTER_ENUM_LOCAL, PRINTER_HANDLE, PRINTER_INFO_4W,
    };

    /// Windows wants NUL-terminated UTF-16 for every string it is given.
    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn wide_to_string(ptr: PWSTR) -> String {
        if ptr.is_null() {
            return String::new();
        }
        unsafe { ptr.to_string().unwrap_or_default() }
    }

    fn default_printer_name() -> String {
        // The first call fails on purpose, having written the length needed.
        let mut len: u32 = 0;
        let _ = unsafe { GetDefaultPrinterW(None, &mut len) };
        if len == 0 {
            return String::new();
        }

        let mut buf = vec![0u16; len as usize];
        let ok = unsafe { GetDefaultPrinterW(Some(PWSTR(buf.as_mut_ptr())), &mut len) };
        if !ok.as_bool() {
            return String::new();
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    pub fn list_blocking() -> Result<Vec<SystemPrinter>, PaxError> {
        let flags = PRINTER_ENUM_LOCAL | PRINTER_ENUM_CONNECTIONS;
        let mut needed: u32 = 0;
        let mut returned: u32 = 0;

        // Level 4 carries just the names, and unlike level 2 it drags in
        // neither the GDI device mode nor a security descriptor.
        unsafe { EnumPrintersW(flags, PCWSTR::null(), 4, None, &mut needed, &mut returned).ok() };
        if needed == 0 {
            return Ok(Vec::new());
        }

        let mut buffer = vec![0u8; needed as usize];
        unsafe {
            EnumPrintersW(flags, PCWSTR::null(), 4, Some(&mut buffer), &mut needed, &mut returned).map_err(
                |e| PaxError::new("PRINTER_UNAVAILABLE", format!("Windows could not list printers: {}", e)),
            )?
        };

        let default_name = default_printer_name();
        // EnumPrintersW packs the structs at the front of the buffer and the
        // strings they point at behind them, so the buffer must outlive this.
        let infos = unsafe {
            std::slice::from_raw_parts(buffer.as_ptr() as *const PRINTER_INFO_4W, returned as usize)
        };

        Ok(infos
            .iter()
            .map(|info| {
                let name = wide_to_string(info.pPrinterName);
                let is_default = !default_name.is_empty() && name == default_name;
                SystemPrinter { name, is_default }
            })
            .filter(|p| !p.name.is_empty())
            .collect())
    }

    fn printer_exists(name: &str) -> bool {
        list_blocking().map(|list| list.iter().any(|p| p.name == name)).unwrap_or(false)
    }

    pub fn print_raw_blocking(name: &str, payload: &[u8]) -> Result<(), PaxError> {
        let name_w = wide(name);
        let mut handle = PRINTER_HANDLE::default();

        unsafe { OpenPrinterW(PCWSTR(name_w.as_ptr()), &mut handle, None) }.map_err(|_| not_found(name))?;

        // Everything past this point must close the handle, hence the closure.
        let result = (|| -> Result<(), PaxError> {
            let mut doc_name = wide("Salesgent receipt");
            // RAW is what stops the driver reinterpreting the ESC/POS.
            let mut datatype = wide("RAW");
            let doc_info = DOC_INFO_1W {
                pDocName: PWSTR(doc_name.as_mut_ptr()),
                pOutputFile: PWSTR::null(),
                pDatatype: PWSTR(datatype.as_mut_ptr()),
            };

            let job = unsafe { StartDocPrinterW(handle, 1, &doc_info) };
            if job == 0 {
                return Err(PaxError::new(
                    "PRINTER_WRITE_FAILED",
                    format!("Windows refused the print job for \"{}\". The printer may be offline or paused.", name),
                ));
            }

            unsafe { StartPagePrinter(handle) }
                .ok()
                .map_err(|e| PaxError::new("PRINTER_WRITE_FAILED", format!("Could not start the page: {}", e)))?;

            let mut written: u32 = 0;
            unsafe {
                WritePrinter(
                    handle,
                    payload.as_ptr() as *const core::ffi::c_void,
                    payload.len() as u32,
                    &mut written,
                )
            }
            .ok()
            .map_err(|e| PaxError::new("PRINTER_WRITE_FAILED", format!("Could not send the receipt: {}", e)))?;

            if written as usize != payload.len() {
                return Err(PaxError::new(
                    "PRINTER_WRITE_FAILED",
                    format!("Only {} of {} bytes reached the printer.", written, payload.len()),
                ));
            }

            unsafe { EndPagePrinter(handle) }
                .ok()
                .map_err(|e| PaxError::new("PRINTER_WRITE_FAILED", format!("Could not end the page: {}", e)))?;
            unsafe { EndDocPrinter(handle) }
                .ok()
                .map_err(|e| PaxError::new("PRINTER_WRITE_FAILED", format!("Could not finish the job: {}", e)))?;
            Ok(())
        })();

        unsafe { ClosePrinter(handle).ok() };
        result
    }
}

// ---------------------------------------------------------------------------
// macOS and Linux — CUPS
// ---------------------------------------------------------------------------

#[cfg(not(windows))]
mod imp {
    use super::{not_found, PaxError, SystemPrinter};
    use std::io::Write;
    use std::process::{Command, Stdio};

    fn cups_missing(tool: &str, err: &std::io::Error) -> PaxError {
        PaxError::new(
            "PRINTER_UNAVAILABLE",
            format!("Could not run `{}` — the printing system is not available on this computer: {}", tool, err),
        )
    }

    pub fn list_blocking() -> Result<Vec<SystemPrinter>, PaxError> {
        // `lpstat -e` lists every destination, one per line, and stays quiet
        // when there are none.
        let output = Command::new("lpstat").arg("-e").output().map_err(|e| cups_missing("lpstat", &e))?;
        if !output.status.success() {
            return Ok(Vec::new());
        }

        let default_name = default_printer_name();
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(|name| SystemPrinter {
                name: name.to_string(),
                is_default: name == default_name,
            })
            .collect())
    }

    fn default_printer_name() -> String {
        // "system default destination: NAME", or a line saying there is none.
        let Ok(output) = Command::new("lpstat").arg("-d").output() else {
            return String::new();
        };
        String::from_utf8_lossy(&output.stdout)
            .split(':')
            .nth(1)
            .map(|name| name.trim().to_string())
            .unwrap_or_default()
    }

    fn printer_exists(name: &str) -> bool {
        list_blocking().map(|list| list.iter().any(|p| p.name == name)).unwrap_or(false)
    }

    pub fn print_raw_blocking(name: &str, payload: &[u8]) -> Result<(), PaxError> {
        // `-o raw` is the CUPS equivalent of the RAW datatype on Windows: hand
        // the bytes to the printer without letting a filter rewrite them.
        let mut child = Command::new("lp")
            .arg("-d")
            .arg(name)
            .arg("-o")
            .arg("raw")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| cups_missing("lp", &e))?;

        child
            .stdin
            .as_mut()
            .ok_or_else(|| PaxError::new("PRINTER_WRITE_FAILED", "Could not write to the print job."))?
            .write_all(payload)
            .map_err(|e| PaxError::new("PRINTER_WRITE_FAILED", format!("Failed sending the receipt: {}", e)))?;

        let output = child
            .wait_with_output()
            .map_err(|e| PaxError::new("PRINTER_WRITE_FAILED", format!("The print job did not finish: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            // `lp` words a missing destination differently on every platform —
            // macOS says "No such file or directory" — so ask CUPS what exists
            // rather than reading tea leaves. Only on the failure path, so a
            // successful print never pays for the extra call.
            if !printer_exists(name) {
                return Err(not_found(name));
            }
            return Err(PaxError::new(
                "PRINTER_WRITE_FAILED",
                if stderr.is_empty() { format!("Printing to \"{}\" failed.", name) } else { stderr },
            ));
        }

        Ok(())
    }
}
