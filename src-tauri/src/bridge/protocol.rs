//! PAX ECR (POSLink wire) protocol — ported byte-for-byte from `bridge/pax.js`.
//!
//! =====================================================================
//!  >>> THIS IS THE FILE TO VERIFY AGAINST THE OFFICIAL PAX PDF <<<
//!  "Interface Specification Between ECR and Terminal"
//! =====================================================================
//!
//! Wire framing:
//!   message = STX + <body> + ETX + LRC
//!     - body fields   separated by FS (0x1C)
//!     - sub-fields    separated by US (0x1F)
//!     - LRC = XOR of every byte AFTER STX, up to and INCLUDING ETX
//!   Protocol version string is sent as field #2 of most commands.

use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Control bytes (ASCII / wire framing)
// ---------------------------------------------------------------------------
pub const STX: u8 = 0x02; // Start of text  -- begins every message
pub const ETX: u8 = 0x03; // End of text    -- ends the body, included in LRC
pub const FS: u8 = 0x1c; // Field separator (between top-level fields)
pub const US: u8 = 0x1f; // Unit separator  (between sub-fields inside one field)
pub const ACK: u8 = 0x06; // Acknowledge (some terminals ACK before the response frame)
#[allow(dead_code)]
pub const NAK: u8 = 0x15; // Negative acknowledge
pub const ENQ: u8 = 0x05; // Enquiry (some POSLink stacks send ENQ to open a session)
#[allow(dead_code)]
pub const EOT: u8 = 0x04; // End of transmission

// ---------------------------------------------------------------------------
// Command codes  (request code  ->  expected response code)
// ---------------------------------------------------------------------------
pub const COMMAND_INITIALIZE: &str = "A00"; // Initialize / ping terminal   -> A01
#[allow(dead_code)]
pub const COMMAND_GET_INPUT: &str = "A08"; // Get input (optional)          -> A09
pub const COMMAND_DO_CREDIT: &str = "T00"; // DoCredit (sale/auth/return/void/postauth) -> T01
pub const COMMAND_BATCH_CLOSE: &str = "B00"; // Batch close / settle        -> B01

/// Expected response code for each request command.
pub fn response_for(command: &str) -> Option<&'static str> {
    match command {
        "A00" => Some("A01"),
        "A08" => Some("A09"),
        "T00" => Some("T01"),
        "B00" => Some("B01"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Transaction type sub-codes for DoCredit (T00), field #3.
// ---------------------------------------------------------------------------
pub const TXN_TYPE_AUTH: &str = "01"; // Pre-authorization
pub const TXN_TYPE_SALE: &str = "02"; // Sale / DoCredit
pub const TXN_TYPE_RETURN: &str = "03"; // Return / refund
pub const TXN_TYPE_VOID: &str = "04"; // Void a previous transaction
#[allow(dead_code)]
pub const TXN_TYPE_POSTAUTH: &str = "05"; // Post-authorization (capture)

// ---------------------------------------------------------------------------
// Result / EDC codes
// ---------------------------------------------------------------------------
pub const RESULT_CODE_APPROVED: &str = "000000";
pub const EDC_TYPE_ALL: &str = "00";

// ---------------------------------------------------------------------------
// T00 request field indexes (zero-based, top-level FS-separated body).
// ---------------------------------------------------------------------------
#[allow(dead_code)]
pub mod t00_req_field {
    pub const COMMAND: usize = 0;
    pub const VERSION: usize = 1;
    pub const TXN_TYPE: usize = 2;
    pub const AMOUNT_INFO: usize = 3;
    pub const ACCOUNT_INFO: usize = 4;
    pub const TRACE_INFO: usize = 5;
    pub const AVS_INFO: usize = 6;
    pub const CASHIER_INFO: usize = 7;
    pub const COMMERCIAL_INFO: usize = 8;
    pub const MOTO_ECOMMERCE: usize = 9;
    pub const ADDITIONAL_INFO: usize = 10;
}

// T01 response field indexes.
mod t01_res_field {
    pub const RESULT_CODE: usize = 2;
    pub const RESULT_TXT: usize = 3;
    pub const HOST_INFO: usize = 4;
    pub const AMOUNT_INFO: usize = 6;
    pub const ACCOUNT_INFO: usize = 7;
    pub const TRACE_INFO: usize = 8;
}

// B01 response field indexes.
mod b01_res_field {
    pub const RESULT_CODE: usize = 2;
    pub const RESULT_TXT: usize = 3;
    pub const HOST_INFO: usize = 4;
    pub const TOTAL_INFO: usize = 5;
}

// ---------------------------------------------------------------------------
// Error type — machine-readable `.code` for the HTTP layer to map.
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct PaxError {
    pub code: &'static str, // e.g. TIMEOUT, CONNECTION_REFUSED, LRC_MISMATCH
    pub message: String,
    pub raw: Option<Value>,
    pub cause: Option<String>,
}

impl PaxError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), raw: None, cause: None }
    }

    pub fn with_raw(mut self, raw: Value) -> Self {
        self.raw = Some(raw);
        self
    }

    #[allow(dead_code)]
    pub fn with_cause(mut self, cause: impl Into<String>) -> Self {
        self.cause = Some(cause.into());
        self
    }
}

/// Lifecycle hook: 'SENDING', 'WAITING' (bytes written), 'RECEIVING' (first byte in).
pub type OnState = Arc<dyn Fn(&str) + Send + Sync + 'static>;

pub fn fire_state(on_state: &Option<OnState>, state: &str) {
    if let Some(cb) = on_state {
        cb(state);
    }
}

// ---------------------------------------------------------------------------
// Field framing (build/parse)
// ---------------------------------------------------------------------------

/// A single request field: either a plain string or a set of US-separated sub-fields.
#[derive(Debug, Clone)]
pub enum Field {
    Single(String),
    Sub(Vec<String>),
}

impl From<&str> for Field {
    fn from(s: &str) -> Self {
        Field::Single(s.to_string())
    }
}

impl From<String> for Field {
    fn from(s: String) -> Self {
        Field::Single(s)
    }
}

impl From<Vec<String>> for Field {
    fn from(v: Vec<String>) -> Self {
        Field::Sub(v)
    }
}

/// Compute the LRC (longitudinal redundancy check) for a PAX frame.
/// LRC = XOR of every byte AFTER STX, up to and INCLUDING ETX.
pub fn compute_lrc(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |acc, b| acc ^ b)
}

/// Build a framed request buffer from an ordered list of fields.
/// Fields are joined with FS. `Field::Sub` entries are US-joined first.
/// Returns STX + body + ETX + LRC.
pub fn build_message(fields: &[Field]) -> Vec<u8> {
    let us_char = US as char;
    let fs_char = FS as char;

    let body = fields
        .iter()
        .map(|f| match f {
            Field::Single(s) => s.clone(),
            Field::Sub(v) => v.join(&us_char.to_string()),
        })
        .collect::<Vec<_>>()
        .join(&fs_char.to_string());

    let body_bytes = body.into_bytes();
    let mut lrc_region = body_bytes.clone();
    lrc_region.push(ETX);
    let lrc = compute_lrc(&lrc_region);

    let mut out = Vec::with_capacity(body_bytes.len() + 3);
    out.push(STX);
    out.extend_from_slice(&body_bytes);
    out.push(ETX);
    out.push(lrc);
    out
}

/// True once the buffer contains a full frame (STX ... ETX + 1 LRC byte).
pub fn has_complete_frame(buf: &[u8]) -> bool {
    match buf.iter().position(|&b| b == STX) {
        Some(start) => match buf[start..].iter().position(|&b| b == ETX) {
            Some(rel) => buf.len() >= start + rel + 2,
            None => false,
        },
        None => false,
    }
}

#[derive(Debug, Clone)]
pub struct ParsedResponse {
    pub fields: Vec<String>,
    pub raw: String,
}

fn is_all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn is_cmd_code(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 3 && b[0].is_ascii_uppercase() && b[1].is_ascii_digit() && b[2].is_ascii_digit()
}

/// Parse a framed response buffer into its field array.
/// Validates STX prefix, presence of ETX, and the trailing LRC.
pub fn parse_response(buf: &[u8]) -> Result<ParsedResponse, PaxError> {
    if buf.len() < 3 {
        return Err(PaxError::new("MALFORMED_RESPONSE", "Response too short to be a valid PAX frame"));
    }

    let start = match buf.iter().position(|&b| b == STX) {
        Some(i) => i,
        None => return Err(PaxError::new("MALFORMED_RESPONSE", "No STX (0x02) found in response")),
    };

    let etx_idx = match buf[start..].iter().position(|&b| b == ETX) {
        Some(rel) => start + rel,
        None => return Err(PaxError::new("MALFORMED_RESPONSE", "No ETX (0x03) + LRC found in response")),
    };

    if etx_idx + 1 >= buf.len() {
        return Err(PaxError::new("MALFORMED_RESPONSE", "No ETX (0x03) + LRC found in response"));
    }

    let body_buf = &buf[start + 1..etx_idx];
    let received_lrc = buf[etx_idx + 1];
    let lrc_region = &buf[start + 1..etx_idx + 1]; // body + ETX
    let expected_lrc = compute_lrc(lrc_region);

    if received_lrc != expected_lrc {
        return Err(PaxError::new(
            "LRC_MISMATCH",
            format!("LRC check failed (received 0x{:x}, expected 0x{:x})", received_lrc, expected_lrc),
        ));
    }

    let body = String::from_utf8_lossy(body_buf).to_string();
    let mut fields: Vec<String> = body.split(FS as char).map(|s| s.to_string()).collect();

    // BroadPOS (TSYS Sierra etc.) prefixes a push/app index before the command
    // code: ["0","A01",...] instead of ["A01",...]. Strip it so parsers align.
    if fields.len() >= 2 && is_all_digits(&fields[0]) && is_cmd_code(&fields[1]) {
        fields.remove(0);
    }

    Ok(ParsedResponse { fields, raw: body })
}

// ---------------------------------------------------------------------------
// Sub-field helpers
// ---------------------------------------------------------------------------
fn sub_fields(field: Option<&String>) -> Vec<String> {
    match field {
        None => vec![],
        Some(f) => f.split(US as char).map(|s| s.to_string()).collect(),
    }
}

fn at(arr: &[String], i: usize) -> String {
    arr.get(i).cloned().unwrap_or_default()
}

/// Produce a control-byte-free `raw` for logging/JSON. Each top-level field
/// that contains US sub-separators becomes a nested array; others stay strings.
fn sanitize_raw(fields: &[String]) -> Value {
    let us_char = US as char;
    Value::Array(
        fields
            .iter()
            .map(|f| {
                if f.contains(us_char) {
                    Value::Array(f.split(us_char).map(|s| Value::String(s.to_string())).collect())
                } else {
                    Value::String(f.clone())
                }
            })
            .collect(),
    )
}

fn to_cents(v: &str) -> i64 {
    let filtered: String = v.chars().filter(|c| c.is_ascii_digit() || *c == '-').collect();
    filtered.parse::<i64>().unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Response DTOs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeInfo {
    pub result_code: String,
    pub result_txt: String,
    pub approved: bool,
    pub serial_number: String,
    pub model: String,
    pub app_version: String,
    pub mac_address: String,
    pub raw: Value,
    pub latency_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreditResponse {
    pub result_code: String,
    pub result_txt: String,
    pub approved: bool,
    pub auth_code: String,
    pub host_ref_num: String,
    pub ref_num: String,
    pub transaction_num: String,
    pub ecr_ref_num: String,
    #[serde(rename = "maskedPAN")]
    pub masked_pan: String,
    pub last4: String,
    pub card_type: String,
    pub entry_mode: String,
    pub approved_amount_cents: i64,
    pub tip_amount_cents: i64,
    pub timestamp: String,
    pub raw: Value,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchResponse {
    pub result_code: String,
    pub result_txt: String,
    pub settled: bool,
    pub batch_number: String,
    pub credit_count: String,
    pub credit_amount_cents: i64,
    pub raw: Value,
}

/// Parse an A01 (initialize) response into terminal info.
/// Per POSLink spec the device fields are TOP-LEVEL FS fields:
///   [A01, version, resultCode, resultTxt, SN, model, OSversion, MAC, ...]
pub fn parse_initialize(parsed: &ParsedResponse) -> InitializeInfo {
    let f = &parsed.fields;
    let result_code = at(f, 2);
    InitializeInfo {
        approved: result_code == RESULT_CODE_APPROVED,
        result_code,
        result_txt: at(f, 3),
        serial_number: at(f, 4),
        model: at(f, 5),
        app_version: at(f, 6),
        mac_address: at(f, 7),
        raw: sanitize_raw(f),
        latency_ms: 0,
    }
}

/// Parse a T01 (DoCredit) response into a structured, defensive object.
/// Tolerates missing trailing fields.
pub fn parse_credit_response(parsed: &ParsedResponse) -> CreditResponse {
    let f = &parsed.fields;
    let host = sub_fields(f.get(t01_res_field::HOST_INFO));
    let amount = sub_fields(f.get(t01_res_field::AMOUNT_INFO));
    let account = sub_fields(f.get(t01_res_field::ACCOUNT_INFO));
    let trace = sub_fields(f.get(t01_res_field::TRACE_INFO));

    let result_code = at(f, t01_res_field::RESULT_CODE);
    let masked_pan = at(&account, 0);
    let last4: String = {
        let digits: Vec<char> = masked_pan.chars().filter(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            String::new()
        } else {
            let start = digits.len().saturating_sub(4);
            digits[start..].iter().collect()
        }
    };

    CreditResponse {
        approved: result_code == RESULT_CODE_APPROVED,
        result_code,
        result_txt: at(f, t01_res_field::RESULT_TXT),
        auth_code: at(&host, 2),
        host_ref_num: at(&host, 3),
        ref_num: at(&trace, 2),
        transaction_num: at(&trace, 1),
        ecr_ref_num: at(&trace, 0),
        masked_pan,
        last4,
        card_type: at(&account, 2),
        entry_mode: at(&account, 3),
        approved_amount_cents: to_cents(&at(&amount, 0)),
        tip_amount_cents: to_cents(&at(&amount, 1)),
        timestamp: at(&trace, 3),
        raw: sanitize_raw(f),
    }
}

/// Parse a B01 (batch close) response.
pub fn parse_batch_response(parsed: &ParsedResponse) -> BatchResponse {
    let f = &parsed.fields;
    let totals = sub_fields(f.get(b01_res_field::TOTAL_INFO));
    let host = sub_fields(f.get(b01_res_field::HOST_INFO));
    let result_code = at(f, b01_res_field::RESULT_CODE);
    BatchResponse {
        settled: result_code == RESULT_CODE_APPROVED,
        result_code,
        result_txt: at(f, b01_res_field::RESULT_TXT),
        batch_number: at(&host, 0),
        credit_count: at(&totals, 0),
        credit_amount_cents: to_cents(&at(&totals, 1)),
        raw: sanitize_raw(f),
    }
}

// ---------------------------------------------------------------------------
// Request builders
// ---------------------------------------------------------------------------

pub struct CreditFieldsInput {
    pub txn_type: &'static str,
    pub amount_cents: i64,
    pub tip_cents: i64,
    pub ecr_ref_num: String,
    pub cashier_id: String,
    pub orig_ref_num: Option<String>,
    pub orig_trans_num: Option<String>,
}

/// Build the ordered T00 field array. Field order matches T00_REQ_FIELD.
pub fn build_credit_fields(input: CreditFieldsInput) -> Vec<Field> {
    let mut amount_info = vec![input.amount_cents.to_string()];
    if input.tip_cents != 0 {
        amount_info.push(input.tip_cents.to_string());
    }

    let mut trace_info = vec![input.ecr_ref_num];
    if let Some(orig_ref) = input.orig_ref_num {
        trace_info.push(String::new()); // invoice
        trace_info.push(orig_ref); // original ref num (VOID)
        if let Some(orig_trans) = input.orig_trans_num {
            trace_info.push(orig_trans);
        }
    }

    vec![
        Field::Single(COMMAND_DO_CREDIT.to_string()), // 0
        Field::Single(crate::bridge::config::protocol_version()), // 1
        Field::Single(input.txn_type.to_string()),    // 2
        Field::Sub(amount_info),                       // 3 amount info
        Field::Single(String::new()),                   // 4 account info (empty -> prompt for card)
        Field::Sub(trace_info),                         // 5 trace info
        Field::Single(String::new()),                   // 6 AVS info
        Field::Single(input.cashier_id),                // 7 cashier info
        Field::Single(String::new()),                   // 8 commercial info
        Field::Single(String::new()),                   // 9 moto/ecommerce
        Field::Single(String::new()),                   // 10 additional info
    ]
}

/// Readable field dump for server logs (split US sub-fields).
pub fn fields_for_log(fields: &[Field]) -> Value {
    let us_char = US as char;
    Value::Array(
        fields
            .iter()
            .map(|f| match f {
                Field::Single(s) => {
                    if s.contains(us_char) {
                        Value::Array(s.split(us_char).map(|p| Value::String(p.to_string())).collect())
                    } else {
                        Value::String(s.clone())
                    }
                }
                Field::Sub(v) => Value::Array(v.iter().map(|p| Value::String(p.clone())).collect()),
            })
            .collect(),
    )
}
