/**
 * pax.js — live PAX BroadPOS terminal protocol (constants + TCP/USB + operations).
 * Single module used by index.js. No mock/emulator.
 */


/**
 * PAX ECR (POSLink wire) protocol constants.
 *
 * =====================================================================
 *  >>> THIS IS THE FILE TO VERIFY AGAINST THE OFFICIAL PAX PDF <<<
 *  "Interface Specification Between ECR and Terminal"
 * =====================================================================
 *
 * ALL protocol magic values live here. No other file should hard-code a
 * command code, a transaction-type code, or a field index. If the spec
 * PDF disagrees with anything below, change it HERE ONLY -- the client,
 * service and parser all read from this module, so logic never needs to
 * be touched.
 *
 * Wire framing (implemented in paxClient.js, described here for context):
 *   message = STX + <body> + ETX + LRC
 *     - body fields   separated by FS (0x1C)
 *     - sub-fields    separated by US (0x1F)
 *     - LRC = XOR of every byte AFTER STX, up to and INCLUDING ETX
 *   Protocol version string is sent as field #2 of most commands.
 */

// ---------------------------------------------------------------------------
// Control bytes (ASCII / wire framing)
// ---------------------------------------------------------------------------
export const CONTROL = {
  STX: 0x02, // Start of text  -- begins every message
  ETX: 0x03, // End of text    -- ends the body, included in LRC
  FS: 0x1c, // Field separator (between top-level fields)
  US: 0x1f, // Unit separator  (between sub-fields inside one field)
  ACK: 0x06, // Acknowledge (some terminals ACK before the response frame)
  NAK: 0x15, // Negative acknowledge
  ENQ: 0x05, // Enquiry (some POSLink stacks send ENQ to open a session)
  EOT: 0x04, // End of transmission
};

// Protocol version string sent as field #2 of most commands.
// EMPIRICAL: BroadPOS TSYS Sierra (A35 / A920 Pro) responds with "1.54" in A01.
// Sending a mismatched version can cause silent rejects / timeouts. Override
// with PAX_PROTOCOL_VERSION if a specific BroadPOS build needs another value.
export const PROTOCOL_VERSION = process.env.PAX_PROTOCOL_VERSION || '1.54';

// ---------------------------------------------------------------------------
// Command codes  (request code  ->  expected response code)
// ---------------------------------------------------------------------------
export const COMMAND = {
  INITIALIZE: 'A00', // Initialize / ping terminal   -> A01
  GET_INPUT: 'A08', // Get input (optional)          -> A09
  DO_CREDIT: 'T00', // DoCredit (sale/auth/return/void/postauth) -> T01
  BATCH_CLOSE: 'B00', // Batch close / settle        -> B01
};

// Expected response code for each request command.
export const RESPONSE_FOR = {
  [COMMAND.INITIALIZE]: 'A01',
  [COMMAND.GET_INPUT]: 'A09',
  [COMMAND.DO_CREDIT]: 'T01',
  [COMMAND.BATCH_CLOSE]: 'B01',
};

// ---------------------------------------------------------------------------
// Transaction type sub-codes for DoCredit (T00), field #3.
// Verify each numeric code against the PDF's "Transaction Type" table.
// ---------------------------------------------------------------------------
export const TXN_TYPE = {
  AUTH: '01', // Pre-authorization
  SALE: '02', // Sale / DoCredit
  RETURN: '03', // Return / refund
  VOID: '04', // Void a previous transaction
  POSTAUTH: '05', // Post-authorization (capture) -- verify code in PDF
};

// Human labels for logging / UI.
export const TXN_TYPE_LABEL = {
  [TXN_TYPE.AUTH]: 'AUTH',
  [TXN_TYPE.SALE]: 'SALE',
  [TXN_TYPE.RETURN]: 'RETURN',
  [TXN_TYPE.VOID]: 'VOID',
  [TXN_TYPE.POSTAUTH]: 'POSTAUTH',
};

// ---------------------------------------------------------------------------
// DoCredit (T00) REQUEST field layout.
//
// Each entry is the ZERO-BASED index of that field in the top-level
// FS-separated body. Field 0 is the command code, field 1 the version.
// If the PDF orders these differently, renumber here.
// ---------------------------------------------------------------------------
export const T00_REQ_FIELD = {
  COMMAND: 0, // "T00"
  VERSION: 1, // PROTOCOL_VERSION
  TXN_TYPE: 2, // one of TXN_TYPE.*
  AMOUNT_INFO: 3, // US-sub: [ amountCents, tipCents, ... ]
  ACCOUNT_INFO: 4, // empty -> terminal prompts for card
  TRACE_INFO: 5, // US-sub: [ ecrRefNum, invoiceNum, origRefNum, ... ]
  AVS_INFO: 6, // empty
  CASHIER_INFO: 7, // optional cashier id
  COMMERCIAL_INFO: 8, // empty
  MOTO_ECOMMERCE: 9, // empty
  ADDITIONAL_INFO: 10, // empty
};

// Sub-field indexes inside the AMOUNT information field (US-separated).
export const AMOUNT_SUB = {
  TRANSACTION_AMOUNT: 0, // transaction amount, in CENTS, no decimal point
  TIP_AMOUNT: 1, // tip amount, in cents (optional)
  CASH_BACK: 2, // cash-back amount (optional)
};

// Sub-field indexes inside the TRACE information field (US-separated).
export const TRACE_SUB = {
  ECR_REF_NUM: 0, // ECR reference number (our per-day sequence)
  INVOICE_NUM: 1, // invoice number (optional)
  AUTH_CODE: 2, // auth code (force/postauth only)
  ORIG_REF_NUM: 3, // original terminal transaction number -- REQUIRED for VOID
};

// ---------------------------------------------------------------------------
// DoCredit (T01) RESPONSE field layout.
//
// Zero-based top-level field indexes. Built defensively in the parser:
// any missing trailing field is treated as empty. Reorder here to match
// the PDF's response table if needed.
// ---------------------------------------------------------------------------
export const T01_RES_FIELD = {
  COMMAND: 0, // "T01"
  VERSION: 1, // protocol version echoed back
  RESULT_CODE: 2, // "000000" == approved (see RESULT_CODE below)
  RESULT_TXT: 3, // human-readable result text
  HOST_INFO: 4, // US-sub: host response, auth code, host ref, trace...
  TXN_TYPE: 5, // echoed transaction type
  AMOUNT_INFO: 6, // US-sub: approved amount, tip, ...
  ACCOUNT_INFO: 7, // US-sub: masked PAN, card type/brand, entry mode...
  TRACE_INFO: 8, // US-sub: ecr ref, transaction #, ref #, timestamp...
  ADDITIONAL_INFO: 9, // US-sub: misc extras
};

// Sub-field indexes inside the T01 HOST information field.
export const HOST_SUB = {
  HOST_RESP_CODE: 0,
  HOST_RESP_TEXT: 1,
  AUTH_CODE: 2, // approval / authorization code
  HOST_REF_NUM: 3,
  TRACE_NUMBER: 4,
};

// Sub-field indexes inside the T01 AMOUNT information field.
export const RES_AMOUNT_SUB = {
  APPROVED_AMOUNT: 0, // approved amount in CENTS
  AMOUNT_DUE: 1,
  TIP_AMOUNT: 2,
};

// Sub-field indexes inside the T01 ACCOUNT information field.
export const ACCOUNT_SUB = {
  MASKED_PAN: 0, // last 4 digits, e.g. "6951"
  ENTRY_MODE: 1, // see ENTRY_MODE_LABEL
  EXP_DATE: 2, // MMYY -- never stored
  CARD_TYPE: 6, // see CARD_TYPE_LABEL
};

export const CARD_TYPE_LABEL = {
  '01': 'VISA',
  '02': 'MASTERCARD',
  '03': 'AMEX',
  '04': 'DISCOVER',
  '05': 'DINERS',
  '07': 'JCB',
};

export const ENTRY_MODE_LABEL = {
  0: 'MANUAL',
  1: 'SWIPE',
  2: 'CONTACTLESS',
  3: 'SCANNER',
  4: 'CHIP',
  5: 'CHIP_FALLBACK_SWIPE',
};

// Sub-field indexes inside the T01 TRACE information field.
export const RES_TRACE_SUB = {
  TRANSACTION_NUM: 0, // terminal transaction number -- the original ref for VOID
  ECR_REF_NUM: 1, // echoed ECR reference number
  TIMESTAMP: 2, // terminal timestamp, YYYYMMDDhhmmss
};

// ---------------------------------------------------------------------------
// Result codes
// ---------------------------------------------------------------------------
export const RESULT_CODE = {
  APPROVED: '000000', // approval / success
};

// ---------------------------------------------------------------------------
// Batch close (B00) request field layout.
// ---------------------------------------------------------------------------
export const B00_REQ_FIELD = {
  COMMAND: 0, // "B00"
  VERSION: 1, // PROTOCOL_VERSION
  EDC_TYPE: 2, // "00" = ALL (verify in PDF)
};

export const EDC_TYPE = {
  ALL: '00',
};

// ---------------------------------------------------------------------------
// Batch close (B01) response field layout.
// ---------------------------------------------------------------------------
export const B01_RES_FIELD = {
  COMMAND: 0, // "B01"
  VERSION: 1,
  RESULT_CODE: 2, // "000000" == settled OK
  RESULT_TXT: 3,
  HOST_INFO: 4, // US-sub: batch number, host trace...
  TOTAL_INFO: 5, // US-sub: credit count, credit amount, debit count...
};

/**
 * paxClient.js -- low-level PAX ECR TCP transport.
 *
 * Responsibilities (protocol bytes live ONLY in this file + protocol-constants):
 *   - buildMessage(fields)     : fields[] -> framed Buffer (STX ... ETX LRC)
 *   - computeLrc(buffer)       : XOR checksum
 *   - parseResponse(buffer)    : framed Buffer -> string[] fields (validated)
 *   - sendCommand(fields, opts): open TCP socket, write, accumulate, resolve
 *
 * Concurrency: PAX terminals handle exactly ONE transaction at a time. Each
 * terminal (keyed by ip:port) gets a serialized queue so we never interleave
 * commands on the wire. Callers that must fail fast on a busy terminal should
 * check isBusy() first and surface 409 TERMINAL_BUSY rather than queueing.
 */

import net from 'node:net';

const hex = (buf) => [...buf].map((b) => b.toString(16).padStart(2, '0')).join(' ');
const sendEnq = () => String(process.env.PAX_SEND_ENQ).toLowerCase() === 'true';

/** Readable field dump for server logs (split US sub-fields). */
function fieldsForLog(fields) {
  const US = String.fromCharCode(CONTROL.US);
  return fields.map((f) => (String(f).includes(US) ? String(f).split(US) : f));
}

/** Error with a machine-readable `.code` for the HTTP layer to map. */
export class PaxError extends Error {
  constructor(code, message, extra = {}) {
    super(message);
    this.name = 'PaxError';
    this.code = code; // e.g. TIMEOUT, CONNECTION_REFUSED, LRC_MISMATCH
    Object.assign(this, extra);
  }
}

/**
 * Compute the LRC (longitudinal redundancy check) for a PAX frame.
 * LRC = XOR of every byte AFTER STX, up to and INCLUDING ETX.
 *
 * @param {Buffer} bytes - the bytes to XOR (body + ETX, without STX).
 * @returns {number} single-byte LRC (0-255)
 */
function computeLrc(bytes) {
  let lrc = 0;
  for (const b of bytes) lrc ^= b;
  return lrc & 0xff;
}

/**
 * Build a framed request buffer from an ordered list of field strings.
 * Fields are joined with FS. Sub-field arrays are US-joined first.
 *
 * @param {Array<string|string[]>} fields - top-level fields; a nested array
 *   is treated as US-separated sub-fields of a single top-level field.
 * @returns {Buffer} STX + body + ETX + LRC
 */
function buildMessage(fields) {
  const { FS, US, STX, ETX } = CONTROL;

  const body = fields
    .map((f) => (Array.isArray(f) ? f.map(subToStr).join(String.fromCharCode(US)) : subToStr(f)))
    .join(String.fromCharCode(FS));

  const bodyBuf = Buffer.from(body, 'ascii');
  // Bytes covered by LRC = body + ETX (STX excluded).
  const lrcRegion = Buffer.concat([bodyBuf, Buffer.from([ETX])]);
  const lrc = computeLrc(lrcRegion);

  return Buffer.concat([Buffer.from([STX]), bodyBuf, Buffer.from([ETX]), Buffer.from([lrc])]);
}

function subToStr(v) {
  if (v === null || v === undefined) return '';
  return String(v);
}

/**
 * Parse a framed response buffer into its field array.
 * Validates STX prefix, presence of ETX, and the trailing LRC.
 *
 * @param {Buffer} buf - the full received frame.
 * @returns {{ fields: string[], raw: string }}
 * @throws {PaxError} MALFORMED_RESPONSE / LRC_MISMATCH
 */
function parseResponse(buf) {
  const { STX, ETX, FS, US } = CONTROL;

  if (!buf || buf.length < 3) {
    throw new PaxError('MALFORMED_RESPONSE', 'Response too short to be a valid PAX frame');
  }
  // Some terminals prepend an ACK byte; skip leading ACK/EOT noise to the STX.
  let start = buf.indexOf(STX);
  if (start === -1) {
    throw new PaxError('MALFORMED_RESPONSE', 'No STX (0x02) found in response');
  }
  const etxIdx = buf.indexOf(ETX, start);
  if (etxIdx === -1 || etxIdx + 1 >= buf.length) {
    throw new PaxError('MALFORMED_RESPONSE', 'No ETX (0x03) + LRC found in response');
  }

  const bodyBuf = buf.slice(start + 1, etxIdx); // between STX and ETX
  const receivedLrc = buf[etxIdx + 1];
  const lrcRegion = buf.slice(start + 1, etxIdx + 1); // body + ETX
  const expectedLrc = computeLrc(lrcRegion);

  if (receivedLrc !== expectedLrc) {
    throw new PaxError(
      'LRC_MISMATCH',
      `LRC check failed (received 0x${receivedLrc.toString(16)}, expected 0x${expectedLrc.toString(16)})`,
    );
  }

  const body = bodyBuf.toString('ascii');
  let fields = body.split(String.fromCharCode(FS)).map((f) =>
    // Preserve US sub-structure by exposing sub-split lazily; here we keep
    // the raw field string. Sub-field parsing happens in paxService.
    f,
  );
  // BroadPOS (TSYS Sierra etc.) prefixes a push/app index before the command
  // code: ["0","A01",...] instead of ["A01",...]. Strip it so parsers align.
  if (
    fields.length >= 2 &&
    /^\d+$/.test(fields[0]) &&
    /^[A-Z]\d{2}$/.test(fields[1])
  ) {
    fields = fields.slice(1);
  }
  // expose helper metadata
  return { fields, raw: body, US: String.fromCharCode(US) };
}

/** True once the buffer contains a full frame (STX ... ETX + 1 LRC byte). */
function hasCompleteFrame(buf) {
  const start = buf.indexOf(CONTROL.STX);
  if (start === -1) return false;
  const etxIdx = buf.indexOf(CONTROL.ETX, start);
  return etxIdx !== -1 && buf.length >= etxIdx + 2; // ETX + LRC present
}

// Per-terminal serialization. Key comes from the target (transport-agnostic:
// `tcp:ip:port` or `usb:/dev/...`). `pending` counts queued+in-flight commands.
const tcpQueues = new Map(); // key -> { chain, pending }

function getTcpQueue(key) {
  if (!tcpQueues.has(key)) tcpQueues.set(key, { chain: Promise.resolve(), pending: 0 });
  return tcpQueues.get(key);
}

/** Build the queue/busy key for a target (accepts a legacy {ip,port} too). */
function targetKey(target) {
  return target.key || `tcp:${target.ip}:${target.port}`;
}

/** Whether a command is currently queued or in flight on the given terminal. */
function tcpIsBusy(target) {
  const q = tcpQueues.get(targetKey(target));
  return !!(q && q.pending > 0);
}

/**
 * Send a command to a terminal and await its response frame.
 * Serialized per terminal: only one in-flight command at a time.
 *
 * @param {object} target - { ip, port }
 * @param {Array<string|string[]>} fields - request fields for buildMessage
 * @param {object} [opts]
 * @param {number} [opts.timeoutMs=120000] - overall response timeout
 * @param {string} [opts.expect] - expected response command code (validated)
 * @param {(state:string)=>void} [opts.onState] - lifecycle hook: 'SENDING',
 *        'WAITING' (bytes written), 'RECEIVING' (first byte in)
 * @returns {Promise<{ fields: string[], raw: string, US: string }>}
 */
function tcpSendCommand(target, fields, opts = {}) {
  const { ip, port } = target;
  const q = getTcpQueue(targetKey(target));

  const run = () =>
    new Promise((resolve, reject) => {
      const timeoutMs = opts.timeoutMs ?? 120_000;
      const request = buildMessage(fields);
      const where = `${ip}:${port}`;
      let chunks = [];
      let settled = false;
      let timer = null;

      const socket = new net.Socket();

      const cleanup = () => {
        if (timer) clearTimeout(timer);
        socket.removeAllListeners();
        socket.destroy();
      };
      const fail = (err) => {
        if (settled) return;
        settled = true;
        console.log(`   ✗ ${where}  ${err.code || 'ERROR'}: ${err.message}`);
        cleanup();
        reject(err);
      };
      const ok = (val) => {
        if (settled) return;
        settled = true;
        cleanup();
        resolve(val);
      };

      timer = setTimeout(() => {
        // CRITICAL: a timeout on a payment command does NOT mean the card was
        // not charged. The caller must treat this as UNKNOWN and never auto-retry.
        fail(new PaxError('TIMEOUT', `No complete response within ${timeoutMs}ms`));
      }, timeoutMs);

      socket.setNoDelay(true);

      socket.on('error', (err) => {
        const code =
          err.code === 'ECONNREFUSED'
            ? 'CONNECTION_REFUSED'
            : err.code === 'ETIMEDOUT'
              ? 'TIMEOUT'
              : err.code === 'EHOSTUNREACH' || err.code === 'ENETUNREACH'
                ? 'HOST_UNREACHABLE'
                : 'SOCKET_ERROR';
        fail(new PaxError(code, describeSocketError(err), { cause: err.code }));
      });

      socket.on('data', (data) => {
        if (chunks.length === 0 && opts.onState) opts.onState('RECEIVING');
        console.log(`   ← ${where}  ${data.length} bytes RAW: ${hex(data)}`);
        chunks.push(data);
        const buf = Buffer.concat(chunks);
        if (hasCompleteFrame(buf)) {
          try {
            const parsed = parseResponse(buf);
            console.log(`   ✅ ${where}  response="${parsed.fields[0]}"  fields=${JSON.stringify(fieldsForLog(parsed.fields))}`);
            const expected = opts.expect ?? RESPONSE_FOR[fields[0]];
            if (expected && parsed.fields[0] !== expected) {
              return fail(
                new PaxError(
                  'UNEXPECTED_RESPONSE',
                  `Expected ${expected} response, got "${parsed.fields[0]}"`,
                  { raw: parsed.raw },
                ),
              );
            }
            // Classic link: ACK the response frame before closing.
            try {
              socket.write(Buffer.from([CONTROL.ACK]));
            } catch {
              /* ignore — we're about to close */
            }
            ok(parsed);
          } catch (err) {
            console.log(`   ⚠️  ${where}  parse failed: ${err.message}`);
            fail(err);
          }
        } else {
          console.log(`   … ${where}  waiting for complete frame (${buf.length} bytes so far)`);
        }
      });

      socket.on('close', () => {
        if (!settled) {
          fail(new PaxError('CONNECTION_CLOSED', 'Terminal closed the connection before a full response'));
        }
      });

      if (opts.onState) opts.onState('SENDING');
      console.log(`\n🔌 Connecting to terminal ${where}  cmd=${fields[0]}`);
      socket.connect(port, ip, () => {
        const writeFrame = () => {
          console.log(`   → ${where}  ${request.length} bytes RAW: ${hex(request)}`);
          console.log(`      fields: ${JSON.stringify(fieldsForLog(fields))}`);
          socket.write(request, (err) => {
            if (err) return fail(new PaxError('WRITE_FAILED', 'Failed to write to terminal', { cause: err.message }));
            // Bytes are on the wire -> customer is now being prompted at the terminal.
            console.log(`   ⏳ ${where}  waiting for response…`);
            if (opts.onState) opts.onState('WAITING');
          });
        };
        // Optional ENQ handshake (PAX_SEND_ENQ=true) for BroadPOS builds that require it.
        if (sendEnq()) {
          console.log(`   → ${where}  ENQ handshake`);
          socket.write(Buffer.from([CONTROL.ENQ]), (err) => {
            if (err) return fail(new PaxError('WRITE_FAILED', 'Failed to write ENQ', { cause: err.message }));
            setTimeout(writeFrame, 50);
          });
        } else {
          writeFrame();
        }
      });
    });

  // Chain onto this terminal's queue so commands never overlap on the wire.
  q.pending += 1;
  const result = q.chain.then(run, run);
  // Advance the chain (swallow errors so one failure doesn't poison the queue)
  // and decrement pending once this command has fully settled.
  q.chain = result.catch(() => {}).finally(() => {
    q.pending -= 1;
  });
  return result;
}

function describeSocketError(err) {
  switch (err.code) {
    case 'ECONNREFUSED':
      return 'Connection refused — BroadPOS is not listening (enable External POS / ECR, Comm=TCP port 10009, leave BroadPOS idle).';
    case 'ETIMEDOUT':
      return 'Connection timed out — terminal may be off, asleep, or on a different network/VLAN.';
    case 'EHOSTUNREACH':
    case 'ENETUNREACH':
      return 'Host unreachable — verify the terminal IP and that it is on the same LAN/subnet as the server.';
    default:
      return `Socket error (${err.code || 'unknown'}): ${err.message}`;
  }
}

/**
 * paxSerialClient.js -- USB / serial PAX ECR transport.
 *
 * Mirror image of paxClient (TCP) but over a serial port. When a PAX terminal
 * is set to Communication Type = USB (or UART) in the payment app, it exposes a
 * CDC-ACM virtual serial device to the host:
 *   - macOS: /dev/tty.usbmodemXXXX
 *   - Linux: /dev/ttyACM0
 *   - Windows: COMx
 *
 * The wire protocol is IDENTICAL to TCP (STX/ETX/LRC, FS/US) -- only the pipe
 * changes -- so we reuse buildMessage / parseResponse / hasCompleteFrame.
 *
 * `serialport` is a native module and is imported lazily: a TCP-only deployment
 * never loads it, and a missing/broken build only affects USB terminals (with a
 * clear error) instead of crashing the whole server.
 */


const DEFAULT_BAUD = 115200;

// Lazy singleton for the serialport module.
let _serialMod = null;
async function getSerial() {
  if (_serialMod) return _serialMod;
  try {
    _serialMod = await import('serialport');
    return _serialMod;
  } catch (err) {
    throw new PaxError(
      'SERIAL_UNAVAILABLE',
      'Serial support is not installed or failed to load (native module "serialport"). Reinstall server dependencies.',
      { cause: err.message },
    );
  }
}

/** List available serial ports (for the UI's "detect device" helper). */
export async function listPorts() {
  const { SerialPort } = await getSerial();
  const ports = await SerialPort.list();
  return ports.map((p) => ({
    path: p.path,
    manufacturer: p.manufacturer || '',
    vendorId: p.vendorId || '',
    productId: p.productId || '',
    serialNumber: p.serialNumber || '',
  }));
}

// Per-port serialization. Key = target.key (usb:/dev/...). Only one command in
// flight per physical port at a time.
const serialQueues = new Map(); // key -> { chain, pending }

function serialTargetKey(target) {
  return target.key || `usb:${target.path}`;
}
function getSerialQueue(key) {
  if (!serialQueues.has(key)) serialQueues.set(key, { chain: Promise.resolve(), pending: 0 });
  return serialQueues.get(key);
}

/** Whether a command is currently queued or in flight on the given port. */
function serialIsBusy(target) {
  const q = serialQueues.get(serialTargetKey(target));
  return !!(q && q.pending > 0);
}

/**
 * Send a command over serial and await its framed response.
 * @param {object} target - { path, baudRate?, key? }
 * @param {Array<string|string[]>} fields - request fields for buildMessage
 * @param {object} [opts] - { timeoutMs, expect, onState }
 * @returns {Promise<{ fields: string[], raw: string, US: string }>}
 */
function serialSendCommand(target, fields, opts = {}) {
  const q = getSerialQueue(serialTargetKey(target));

  const run = async () => {
    const { SerialPort } = await getSerial();
    return new Promise((resolve, reject) => {
      const timeoutMs = opts.timeoutMs ?? 120_000;
      const request = buildMessage(fields);
      let chunks = [];
      let settled = false;
      let timer = null;
      let port = null;

      const cleanup = () => {
        if (timer) clearTimeout(timer);
        if (port && port.isOpen) {
          try {
            port.removeAllListeners();
            port.close(() => {});
          } catch {
            /* ignore */
          }
        }
      };
      const fail = (err) => {
        if (settled) return;
        settled = true;
        cleanup();
        reject(err);
      };
      const ok = (val) => {
        if (settled) return;
        settled = true;
        cleanup();
        resolve(val);
      };

      timer = setTimeout(() => {
        // SAFETY: a serial timeout on a payment is UNKNOWN, never a decline.
        // The caller must not auto-retry.
        fail(new PaxError('TIMEOUT', `No complete response within ${timeoutMs}ms`));
      }, timeoutMs);

      try {
        port = new SerialPort({
          path: target.path,
          baudRate: Number(target.baudRate) || DEFAULT_BAUD,
          autoOpen: false,
        });
      } catch (err) {
        return fail(new PaxError('SERIAL_OPEN_FAILED', `Could not create serial port ${target.path}: ${err.message}`));
      }

      port.on('error', (err) => {
        const code = /no such file|cannot open|access denied|permission/i.test(err.message)
          ? 'SERIAL_OPEN_FAILED'
          : 'SERIAL_ERROR';
        fail(
          new PaxError(
            code,
            code === 'SERIAL_OPEN_FAILED'
              ? `Cannot open ${target.path} — check the USB cable and that the terminal's payment app is set to USB/ECR mode. (${err.message})`
              : `Serial error on ${target.path}: ${err.message}`,
            { cause: err.message },
          ),
        );
      });

      port.on('data', (data) => {
        if (chunks.length === 0 && opts.onState) opts.onState('RECEIVING');
        console.log(`   ← ${target.path}  ${data.length} bytes RAW: ${hex(data)}`);
        chunks.push(data);
        const buf = Buffer.concat(chunks);
        if (hasCompleteFrame(buf)) {
          try {
            const parsed = parseResponse(buf);
            console.log(`   ✅ ${target.path}  response="${parsed.fields[0]}"  fields=${JSON.stringify(parsed.fields)}`);
            const expected = opts.expect ?? RESPONSE_FOR[fields[0]];
            if (expected && parsed.fields[0] !== expected) {
              return fail(
                new PaxError('UNEXPECTED_RESPONSE', `Expected ${expected} response, got "${parsed.fields[0]}"`, {
                  raw: parsed.raw,
                }),
              );
            }
            // Match TCP / Go bridge: ACK the response frame before closing.
            try {
              port.write(Buffer.from([CONTROL.ACK]));
            } catch {
              /* ignore — about to close */
            }
            ok(parsed);
          } catch (err) {
            console.log(`   ⚠️  ${target.path}  parse failed: ${err.message}  RAW so far: ${hex(buf)}`);
            fail(err);
          }
        } else {
          console.log(`   … ${target.path}  waiting for complete frame (${buf.length} bytes so far)`);
        }
      });

      if (opts.onState) opts.onState('SENDING');
      port.open((err) => {
        if (err) {
          return fail(
            new PaxError(
              'SERIAL_OPEN_FAILED',
              `Cannot open ${target.path} — is the terminal plugged in and in USB/ECR mode? (${err.message})`,
              { cause: err.message },
            ),
          );
        }
        const writeFrame = () => {
          console.log(`   → ${target.path}  ${request.length} bytes RAW: ${hex(request)}`);
          port.write(request, (werr) => {
            if (werr) return fail(new PaxError('WRITE_FAILED', `Failed to write to ${target.path}: ${werr.message}`));
            port.drain(() => {
              console.log(`   ⏳ ${target.path}  waiting for response…`);
              if (opts.onState) opts.onState('WAITING');
            });
          });
        };
        // Some BroadPOS USB builds expect ENQ before the first STX frame.
        if (sendEnq()) {
          console.log(`   → ${target.path}  ENQ handshake`);
          port.write(Buffer.from([CONTROL.ENQ]), (enqErr) => {
            if (enqErr) return fail(new PaxError('WRITE_FAILED', `Failed to write ENQ: ${enqErr.message}`));
            setTimeout(writeFrame, 50);
          });
        } else {
          writeFrame();
        }
      });
    });
  };

  q.pending += 1;
  const result = q.chain.then(run, run);
  q.chain = result.catch(() => {}).finally(() => {
    q.pending -= 1;
  });
  return result;
}

/**
 * paxService.js -- high-level PAX operations.
 *
 * Turns intent (sale/refund/void/ping/batch) into protocol field arrays,
 * sends them via the live terminal transport (TCP or USB), and parses the
 * response frames into structured, cents-based objects.
 *
 * No HTTP, no DB, no WebSocket here -- that orchestration lives in the routes.
 * Callers pass an `onState` hook to receive lifecycle transitions.
 */


const isUsb = (terminal) => terminal.connType === 'usb';

/**
 * Build a transport-agnostic target for a terminal, including a stable `key`
 * used for per-terminal command serialization (busy/queue).
 *   TCP:  { key: 'tcp:<ip>:<port>', ip, port }
 *   USB:  { key: 'usb:<path>',      path, baudRate }
 */
function targetFor(terminal) {
  if (isUsb(terminal)) {
    return {
      key: `usb:${terminal.serialPath}`,
      path: terminal.serialPath,
      baudRate: Number(terminal.baudRate) || 115200,
    };
  }
  return { key: `tcp:${terminal.ip}:${terminal.port}`, ip: terminal.ip, port: terminal.port };
}

/** Pick the live transport for a terminal: USB serial or TCP. */
function clientFor(terminal) {
  return isUsb(terminal)
    ? { sendCommand: serialSendCommand, isBusy: serialIsBusy }
    : { sendCommand: tcpSendCommand, isBusy: tcpIsBusy };
}

/** True if a command is already in flight/queued for this terminal. */
export function isTerminalBusy(terminal) {
  return clientFor(terminal).isBusy(targetFor(terminal));
}

/** List serial ports available on the host (for the "detect USB device" UI). */
export function listSerialPorts() {
  return listPorts();
}

/**
 * Lightweight LAN/USB diagnostics without sending a full payment.
 * Helps distinguish "device offline" vs "BroadPOS ECR not listening".
 */
export async function diagnose(terminal) {
  const protocolVersion = PROTOCOL_VERSION;
  if (isUsb(terminal)) {
    const ports = await listPorts();
    const path = terminal.serialPath;
    const match = ports.find((p) => p.path === path);
    return {
      connType: 'usb',
      protocolVersion,
      serialPath: path,
      portPresent: !!match,
      portsFound: ports.length,
      matchingPort: match || null,
      ecrLikelyListening: !!match,
      nextSteps: match
        ? [
            'Serial device is present. Run Test Connection.',
            'In BroadPOS: External POS / ECR ON, Communication = USB, leave idle.',
          ]
        : [
            'No matching USB serial device. Set Android USB to PAX POSVCOM USB MODE.',
            'In BroadPOS: External POS / ECR ON, Communication = USB.',
            'Replug the USB cable into this Mac, then Detect again.',
          ],
    };
  }

  const ip = terminal.ip;
  const port = Number(terminal.port) || 10009;
  const tcp = await probeTcp(ip, port, 3000);
  return {
    connType: 'tcp',
    protocolVersion,
    ip,
    port,
    hostReachable: tcp.hostReachable,
    tcpOpen: tcp.open,
    tcpError: tcp.error || null,
    ecrLikelyListening: tcp.open,
    nextSteps: tcp.open
      ? [
          'TCP port is open — BroadPOS ECR appears to be listening. Run Test Connection.',
        ]
      : tcp.hostReachable
        ? [
            'Device is on the network but port is closed — BroadPOS ECR is not listening.',
            'Open BroadPOS TSYS Sierra → Settings (squares) → password = today’s date MMDDYYYY (try ±1 day).',
            'System Settings → ECR-Terminal Integration Mode → External POS.',
            'ECR Comm Settings → Protocol Type = TCP/IP, Host Port = 10009.',
            'Leave BroadPOS on the idle / ready screen, then Test Connection again.',
          ]
        : [
            'Cannot reach this IP. Confirm the terminal Wi‑Fi IP and that the Mac is on the same LAN (AP isolation off).',
          ],
  };
}

function probeTcp(ip, port, timeoutMs) {
  return new Promise((resolve) => {
    const socket = new net.Socket();
    let settled = false;
    const done = (result) => {
      if (settled) return;
      settled = true;
      try {
        socket.destroy();
      } catch {
        /* ignore */
      }
      resolve(result);
    };
    const timer = setTimeout(() => done({ open: false, hostReachable: false, error: 'TIMEOUT' }), timeoutMs);
    socket.once('connect', () => {
      clearTimeout(timer);
      done({ open: true, hostReachable: true });
    });
    socket.once('error', (err) => {
      clearTimeout(timer);
      if (err.code === 'ECONNREFUSED') {
        done({ open: false, hostReachable: true, error: 'ECONNREFUSED' });
      } else if (err.code === 'EHOSTUNREACH' || err.code === 'ENETUNREACH' || err.code === 'EHOSTDOWN') {
        done({ open: false, hostReachable: false, error: err.code });
      } else {
        done({ open: false, hostReachable: false, error: err.code || err.message });
      }
    });
    socket.connect(port, ip);
  });
}

// Timeouts (ms). Payments get a long window so the customer can tap/insert.
const PING_TIMEOUT = Number(process.env.PAX_PING_TIMEOUT_MS ?? 10_000);
const PAYMENT_TIMEOUT = Number(process.env.PAX_PAYMENT_TIMEOUT_MS ?? 120_000);

// ---------------------------------------------------------------------------
// Sub-field helpers -- split a top-level field into its US sub-fields, and
// read a sub-field defensively (missing trailing fields -> '').
// ---------------------------------------------------------------------------
function subFields(field, US) {
  if (field === undefined || field === null) return [];
  return String(field).split(US);
}
function at(arr, i) {
  return arr[i] !== undefined ? arr[i] : '';
}

/**
 * Produce a control-byte-free `raw` for logging/JSON. Each top-level field
 * that contains US sub-separators becomes a nested array; others stay strings.
 * This keeps raw human-debuggable and safe for strict JSON parsers / files.
 */
function sanitizeRaw(fields, US) {
  return fields.map((f) => (String(f).includes(US) ? String(f).split(US) : f));
}

// ---------------------------------------------------------------------------
// Response parsers
// ---------------------------------------------------------------------------

/**
 * Parse an A01 (initialize) response into terminal info.
 * Per POSLink spec the device fields are TOP-LEVEL FS fields:
 *   [A01, version, resultCode, resultTxt, SN, model, OSversion, MAC, ...]
 */
export function parseInitialize(parsed) {
  const { fields, US } = parsed;
  return {
    resultCode: at(fields, 2),
    resultTxt: at(fields, 3),
    approved: at(fields, 2) === RESULT_CODE.APPROVED,
    serialNumber: at(fields, 4),
    model: at(fields, 5),
    appVersion: at(fields, 6),
    macAddress: at(fields, 7),
    raw: sanitizeRaw(fields, US),
  };
}

/**
 * Parse a T01 (DoCredit) response into a structured, defensive object.
 * Tolerates missing trailing fields.
 */
export function parseCreditResponse(parsed) {
  const { fields, US } = parsed;
  const host = subFields(fields[T01_RES_FIELD.HOST_INFO], US);
  const amount = subFields(fields[T01_RES_FIELD.AMOUNT_INFO], US);
  const account = subFields(fields[T01_RES_FIELD.ACCOUNT_INFO], US);
  const trace = subFields(fields[T01_RES_FIELD.TRACE_INFO], US);

  const resultCode = at(fields, T01_RES_FIELD.RESULT_CODE);
  const maskedPan = at(account, ACCOUNT_SUB.MASKED_PAN);

  return {
    resultCode,
    resultTxt: at(fields, T01_RES_FIELD.RESULT_TXT),
    approved: resultCode === RESULT_CODE.APPROVED,
    authCode: at(host, HOST_SUB.AUTH_CODE),
    hostRefNum: at(host, HOST_SUB.HOST_REF_NUM),
    refNum: at(trace, RES_TRACE_SUB.TRANSACTION_NUM),
    transactionNum: at(trace, RES_TRACE_SUB.TRANSACTION_NUM),
    ecrRefNum: at(trace, RES_TRACE_SUB.ECR_REF_NUM),
    maskedPAN: maskedPan, // full masked value
    last4: maskedPan ? maskedPan.replace(/[^0-9]/g, '').slice(-4) : '',
    cardType: labelFor(CARD_TYPE_LABEL, at(account, ACCOUNT_SUB.CARD_TYPE)),
    entryMode: labelFor(ENTRY_MODE_LABEL, at(account, ACCOUNT_SUB.ENTRY_MODE)),
    approvedAmountCents: toCents(at(amount, RES_AMOUNT_SUB.APPROVED_AMOUNT)),
    tipAmountCents: toCents(at(amount, RES_AMOUNT_SUB.TIP_AMOUNT)),
    timestamp: at(trace, RES_TRACE_SUB.TIMESTAMP),
    raw: redactExpiry(sanitizeRaw(fields, US)), // keep everything else for debugging
  };
}

function labelFor(labels, code) {
  return labels[code] || code;
}

/** Blank the card expiry inside the raw ACCOUNT field before it is logged or stored. */
function redactExpiry(raw) {
  const account = raw[T01_RES_FIELD.ACCOUNT_INFO];
  if (Array.isArray(account) && account[ACCOUNT_SUB.EXP_DATE]) {
    account[ACCOUNT_SUB.EXP_DATE] = '';
  }
  return raw;
}

/** Parse a B01 (batch close) response. */
export function parseBatchResponse(parsed) {
  const { fields, US } = parsed;
  const totals = subFields(fields[B01_RES_FIELD.TOTAL_INFO], US);
  const host = subFields(fields[B01_RES_FIELD.HOST_INFO], US);
  const resultCode = at(fields, B01_RES_FIELD.RESULT_CODE);
  return {
    resultCode,
    resultTxt: at(fields, B01_RES_FIELD.RESULT_TXT),
    settled: resultCode === RESULT_CODE.APPROVED,
    batchNumber: at(host, 0),
    creditCount: at(totals, 0),
    creditAmountCents: toCents(at(totals, 1)),
    raw: sanitizeRaw(fields, US),
  };
}

function toCents(v) {
  const n = parseInt(String(v).replace(/[^0-9-]/g, ''), 10);
  return Number.isFinite(n) ? n : 0;
}

// ---------------------------------------------------------------------------
// High-level operations
// ---------------------------------------------------------------------------

/** A00 initialize / ping. Returns { ...terminalInfo, latencyMs }. */
export async function initialize(terminal, { onState } = {}) {
  const started = Date.now();
  const parsed = await clientFor(terminal).sendCommand(
    targetFor(terminal),
    [COMMAND.INITIALIZE, PROTOCOL_VERSION],
    { timeoutMs: PING_TIMEOUT, onState },
  );
  const info = parseInitialize(parsed);
  info.latencyMs = Date.now() - started;
  return info;
}

/**
 * Build the ordered T00 field array. Kept here (not in routes) so the field
 * order matches T00_REQ_FIELD in the constants module.
 */
function buildCreditFields({ txnType, amountCents, tipCents = 0, ecrRefNum, cashierId = '', origRefNum }) {
  const amountInfo = [String(amountCents)];
  if (tipCents) amountInfo[1] = String(tipCents);

  const traceInfo = [String(ecrRefNum)];
  if (origRefNum !== undefined) {
    traceInfo[TRACE_SUB.INVOICE_NUM] = '';
    traceInfo[TRACE_SUB.AUTH_CODE] = '';
    traceInfo[TRACE_SUB.ORIG_REF_NUM] = String(origRefNum);
  }

  return [
    COMMAND.DO_CREDIT, // 0
    PROTOCOL_VERSION, // 1
    txnType, // 2
    amountInfo, // 3 amount info (US-sub)
    '', // 4 account info (empty -> prompt for card)
    traceInfo, // 5 trace info (US-sub)
    '', // 6 AVS info
    cashierId, // 7 cashier info
    '', // 8 commercial info
    '', // 9 moto/ecommerce
    '', // 10 additional info
  ];
}

async function doCredit(terminal, fields, { onState } = {}) {
  const parsed = await clientFor(terminal).sendCommand(
    targetFor(terminal),
    fields,
    { timeoutMs: PAYMENT_TIMEOUT, onState },
  );
  return parseCreditResponse(parsed);
}

/** T00 SALE. amountCents/tipCents are integer cents. */
export function sale(terminal, amountCents, ecrRefNum, { tipCents = 0, cashierId = '', onState } = {}) {
  return doCredit(
    terminal,
    buildCreditFields({ txnType: TXN_TYPE.SALE, amountCents, tipCents, ecrRefNum, cashierId }),
    { onState },
  );
}

/** T00 AUTH (pre-authorization). */
export function auth(terminal, amountCents, ecrRefNum, { tipCents = 0, cashierId = '', onState } = {}) {
  return doCredit(
    terminal,
    buildCreditFields({ txnType: TXN_TYPE.AUTH, amountCents, tipCents, ecrRefNum, cashierId }),
    { onState },
  );
}

/** T00 RETURN / refund. */
export function refund(terminal, amountCents, ecrRefNum, { cashierId = '', onState } = {}) {
  return doCredit(
    terminal,
    buildCreditFields({ txnType: TXN_TYPE.RETURN, amountCents, ecrRefNum, cashierId }),
    { onState },
  );
}

/**
 * T00 VOID. Voids a previous transaction by its original ref number.
 * amountCents is optional (some hosts require it, some ignore it for voids).
 */
export function voidTransaction(terminal, origRefNum, ecrRefNum, { amountCents = 0, cashierId = '', onState } = {}) {
  return doCredit(
    terminal,
    buildCreditFields({
      txnType: TXN_TYPE.VOID,
      amountCents,
      ecrRefNum,
      cashierId,
      origRefNum,
    }),
    { onState },
  );
}

/** B00 batch close / settle. */
export async function batchClose(terminal, { onState } = {}) {
  const parsed = await clientFor(terminal).sendCommand(
    targetFor(terminal),
    (() => {
      const f = [];
      f[B00_REQ_FIELD.COMMAND] = COMMAND.BATCH_CLOSE;
      f[B00_REQ_FIELD.VERSION] = PROTOCOL_VERSION;
      f[B00_REQ_FIELD.EDC_TYPE] = EDC_TYPE.ALL;
      return f;
    })(),
    { timeoutMs: PAYMENT_TIMEOUT, onState },
  );
  return parseBatchResponse(parsed);
}
