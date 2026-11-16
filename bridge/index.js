/**
 * PAX Bridge — local Express server for live BroadPOS terminals.
 * Layout: index.js (HTTP/WS/DB/API) + pax.js (wire protocol + TCP/USB).
 */

import cors from 'cors';
import dotenv from 'dotenv';
import express, { Router } from 'express';
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { WebSocketServer } from 'ws';
import {
  isTerminalBusy,
  listSerialPorts,
  initialize,
  diagnose,
  batchClose,
  sale,
  refund,
  voidTransaction,
} from './pax.js';



/**
 * Directory where config + data live.
 * - PAX_HOME (set by the installers/launchers) always wins — it points at a
 *   user-writable folder (~/PAXBridge, %LOCALAPPDATA%\PAX Bridge) so nothing is
 *   written inside a read-only app bundle or Program Files.
 * - Otherwise: exe folder when packaged, server/ in dev.
 */
function runtimeDir() {
  if (process.env.PAX_HOME) return path.resolve(process.env.PAX_HOME);
  if (process.pkg) return path.dirname(process.execPath);
  return path.resolve(path.dirname(fileURLToPath(import.meta.url)));
}

const DEFAULT_ENV = `# Auto-created on first run — live BroadPOS / TSYS Sierra mode.
PORT=5000
PAX_PING_TIMEOUT_MS=10000
PAX_PAYMENT_TIMEOUT_MS=120000
`;

/**
 * Create data/ + .env with production-safe defaults if missing.
 * @returns {{ dir: string, envPath: string, dataDir: string, createdEnv: boolean }}
 */
function ensureRuntimeSetup() {
  const dir = runtimeDir();
  const envPath = path.join(dir, '.env');
  const dataDir = path.join(dir, 'data');
  let createdEnv = false;

  // Config/data writes can fail if the folder is read-only (e.g. a Gatekeeper'd
  // app bundle). Degrade to built-in defaults instead of crashing on launch.
  try {
    if (!fs.existsSync(dataDir)) {
      fs.mkdirSync(dataDir, { recursive: true });
      console.log(`[setup] Created data folder: ${dataDir}`);
    }

    if (!fs.existsSync(envPath)) {
      fs.writeFileSync(envPath, DEFAULT_ENV, 'utf8');
      createdEnv = true;
      console.log(`[setup] Created default config: ${envPath}`);
    }
  } catch (err) {
    console.error(`[setup] Could not write to ${dir} (${err.message}). Using defaults; set PAX_HOME to a writable folder to persist config.`);
  }

  // When packaged, pin data next to the exe unless the merchant overrides.
  if (process.pkg && !process.env.PAX_DATA_DIR) {
    process.env.PAX_DATA_DIR = dataDir;
  }

  return { dir, envPath, dataDir, createdEnv };
}


const { envPath } = ensureRuntimeSetup();
dotenv.config({ path: envPath });



/**
 * Where to store the JSON database.
 *
 * - Packaged as a single .exe (pkg): the source tree lives inside a read-only
 *   virtual snapshot, so we persist next to the executable instead -- i.e. a
 *   `data/` folder beside pax-bridge.exe. Merchants can find/back it up easily.
 * - Normal `node` run: keep the original source-relative `server/data`.
 * - Override either with the PAX_DATA_DIR env var.
 */
function resolveDataDir() {
  if (process.env.PAX_DATA_DIR) return path.resolve(process.env.PAX_DATA_DIR);
  // Installer-provided home (writable) — keeps data with the config.
  if (process.env.PAX_HOME) return path.join(path.resolve(process.env.PAX_HOME), 'data');
  // `process.pkg` is injected by the pkg runtime; process.execPath is the .exe.
  if (process.pkg) return path.join(path.dirname(process.execPath), 'data');
  return path.resolve(path.dirname(fileURLToPath(import.meta.url)), 'data');
}

const DATA_DIR = resolveDataDir();
const DB_FILE = path.join(DATA_DIR, 'pax-db.json');

const DEFAULT_DATA = {
  terminals: [],
  transactions: [],
  // per-day ECR reference counter: { [yyyymmdd]: lastSeq }
  ecrSeq: {},
};

let data = null;
let writeChain = Promise.resolve();

function load() {
  if (data) return data;
  try {
    if (!fs.existsSync(DATA_DIR)) fs.mkdirSync(DATA_DIR, { recursive: true });
    if (fs.existsSync(DB_FILE)) {
      data = { ...DEFAULT_DATA, ...JSON.parse(fs.readFileSync(DB_FILE, 'utf8')) };
    } else {
      data = structuredClone(DEFAULT_DATA);
      persist();
    }
  } catch (err) {
    // Corrupt file: back it up and start fresh rather than crash.
    console.error('[db] failed to load, starting fresh:', err.message);
    try {
      if (fs.existsSync(DB_FILE)) fs.renameSync(DB_FILE, `${DB_FILE}.corrupt-${Date.now()}`);
    } catch {
      /* ignore */
    }
    data = structuredClone(DEFAULT_DATA);
  }
  return data;
}

/** Atomically persist current in-memory data. Serialized to avoid races. */
function persist() {
  const snapshot = JSON.stringify(data, null, 2);
  writeChain = writeChain.then(
    () =>
      new Promise((resolve) => {
        const tmp = `${DB_FILE}.tmp-${process.pid}`;
        try {
          fs.writeFileSync(tmp, snapshot);
          fs.renameSync(tmp, DB_FILE);
        } catch (err) {
          console.error('[db] persist failed:', err.message);
        }
        resolve();
      }),
  );
  return writeChain;
}

function genId(prefix) {
  return `${prefix}_${Date.now().toString(36)}${Math.random().toString(36).slice(2, 8)}`;
}

// ---------------------------------------------------------------------------
// Terminals
// ---------------------------------------------------------------------------
function listTerminals() {
  return load().terminals.slice();
}

function getTerminal(id) {
  return load().terminals.find((t) => t.id === id) || null;
}

function createTerminal({ name, ip, port = 10009, model = 'A920 Pro', connType = 'tcp', serialPath, baudRate = 115200 }) {
  const d = load();
  const terminal = {
    id: genId('term'),
    name: name?.trim() || 'Terminal',
    model,
    // Connection: 'tcp' (LAN) or 'usb' (serial cable).
    connType: connType === 'usb' ? 'usb' : 'tcp',
    // TCP fields:
    ip: ip?.trim() || '',
    port: Number(port) || 10009,
    // USB/serial fields:
    serialPath: serialPath?.trim() || '',
    baudRate: Number(baudRate) || 115200,
    createdAt: new Date().toISOString(),
  };
  d.terminals.push(terminal);
  persist();
  return terminal;
}

function updateTerminal(id, patch) {
  const d = load();
  const t = d.terminals.find((x) => x.id === id);
  if (!t) return null;
  if (patch.name !== undefined) t.name = String(patch.name).trim();
  if (patch.model !== undefined) t.model = patch.model;
  if (patch.connType !== undefined) t.connType = patch.connType === 'usb' ? 'usb' : 'tcp';
  if (patch.ip !== undefined) t.ip = String(patch.ip).trim();
  if (patch.port !== undefined) t.port = Number(patch.port) || t.port;
  if (patch.serialPath !== undefined) t.serialPath = String(patch.serialPath).trim();
  if (patch.baudRate !== undefined) t.baudRate = Number(patch.baudRate) || t.baudRate;
  persist();
  return t;
}

function deleteTerminal(id) {
  const d = load();
  const idx = d.terminals.findIndex((t) => t.id === id);
  if (idx === -1) return false;
  d.terminals.splice(idx, 1);
  persist();
  return true;
}

// ---------------------------------------------------------------------------
// ECR reference numbers -- unique, auto-incrementing, reset per day.
// Format: <yyyymmdd><4-digit seq>, e.g. 202607100001
// ---------------------------------------------------------------------------
function nextEcrRefNum(now = new Date()) {
  const d = load();
  const y = now.getFullYear();
  const m = String(now.getMonth() + 1).padStart(2, '0');
  const day = String(now.getDate()).padStart(2, '0');
  const dateKey = `${y}${m}${day}`;
  const next = (d.ecrSeq[dateKey] || 0) + 1;
  d.ecrSeq[dateKey] = next;
  persist();
  return `${dateKey}${String(next).padStart(4, '0')}`;
}

// ---------------------------------------------------------------------------
// Transactions
// ---------------------------------------------------------------------------
function createTransaction(tx) {
  const d = load();
  const record = {
    id: genId('txn'),
    createdAt: new Date().toISOString(),
    updatedAt: new Date().toISOString(),
    status: 'PENDING',
    ...tx,
  };
  d.transactions.unshift(record); // newest first
  persist();
  return record;
}

function updateTransaction(id, patch) {
  const d = load();
  const tx = d.transactions.find((t) => t.id === id);
  if (!tx) return null;
  Object.assign(tx, patch, { updatedAt: new Date().toISOString() });
  persist();
  return tx;
}

function getTransaction(id) {
  return load().transactions.find((t) => t.id === id) || null;
}

/**
 * Query transactions with optional filters and pagination.
 * @param {object} q
 * @param {string} [q.status]     - exact status match
 * @param {string} [q.from]       - ISO date lower bound (inclusive)
 * @param {string} [q.to]         - ISO date upper bound (inclusive)
 * @param {number} [q.page=1]
 * @param {number} [q.pageSize=25]
 */
function queryTransactions(q = {}) {
  let rows = load().transactions.slice();
  if (q.status) rows = rows.filter((t) => t.status === q.status);
  if (q.type) rows = rows.filter((t) => t.type === q.type);
  if (q.from) rows = rows.filter((t) => t.createdAt >= q.from);
  if (q.to) rows = rows.filter((t) => t.createdAt <= q.to);
  if (q.terminalId) rows = rows.filter((t) => t.terminalId === q.terminalId);

  const page = Math.max(1, Number(q.page) || 1);
  const pageSize = Math.min(200, Math.max(1, Number(q.pageSize) || 25));
  const total = rows.length;
  const start = (page - 1) * pageSize;
  return {
    total,
    page,
    pageSize,
    pageCount: Math.ceil(total / pageSize) || 1,
    rows: rows.slice(start, start + pageSize),
  };
}

/** Unsettled = approved sale/auth transactions not yet batch-closed today. */
function listUnsettled() {
  return load().transactions.filter(
    (t) => t.status === 'APPROVED' && !t.settled && (t.type === 'SALE' || t.type === 'AUTH' || t.type === 'RETURN'),
  );
}

/** Mark a set of transaction ids as settled (after a successful batch close). */
function markSettled(ids, batchInfo) {
  const d = load();
  const set = new Set(ids);
  for (const tx of d.transactions) {
    if (set.has(tx.id)) {
      tx.settled = true;
      tx.batchInfo = batchInfo;
      tx.updatedAt = new Date().toISOString();
    }
  }
  persist();
}



const MAP = {
  TERMINAL_BUSY: {
    status: 409,
    hint: 'The terminal is already processing another transaction. Wait for it to finish before sending a new one.',
  },
  CONNECTION_REFUSED: {
    status: 502,
    hint:
      'Device reachable but BroadPOS is not listening on this port. In BroadPOS TSYS Sierra: set ECR-Terminal Integration Mode = External POS, Communication = TCP, Host Port = 10009 (or USB if using a cable), then leave BroadPOS on the idle screen.',
  },
  HOST_UNREACHABLE: {
    status: 502,
    hint: 'Terminal unreachable — verify it is powered on and on the same LAN/subnet as this server.',
  },
  CONNECTION_CLOSED: {
    status: 502,
    hint: 'The terminal closed the connection before responding. Check terminal status and try again.',
  },
  TIMEOUT: {
    status: 504,
    hint: 'No response within the timeout. IMPORTANT: the card may already have been charged — verify on the terminal before retrying. Do NOT blindly retry.',
  },
  LRC_MISMATCH: {
    status: 502,
    hint: 'Corrupted response (LRC check failed). This can indicate a protocol version mismatch or a noisy connection.',
  },
  MALFORMED_RESPONSE: {
    status: 502,
    hint: 'Could not parse the terminal response. Verify the protocol version and constants against the PAX spec PDF.',
  },
  UNEXPECTED_RESPONSE: {
    status: 502,
    hint: 'The terminal returned an unexpected command code. Verify the constants file against the PAX spec.',
  },
  WRITE_FAILED: { status: 502, hint: 'Failed to send the command to the terminal.' },
  SOCKET_ERROR: { status: 502, hint: 'Network error communicating with the terminal.' },
  SERIAL_OPEN_FAILED: {
    status: 502,
    hint:
      'Could not open the USB/serial port. Plug in the cable, set Android USB computer connection to PAX POSVCOM USB MODE, and in BroadPOS set Communication = USB with External POS / ECR on. On macOS the device appears as /dev/tty.usbmodem…',
  },
  SERIAL_ERROR: { status: 502, hint: 'Serial communication error with the terminal.' },
  SERIAL_UNAVAILABLE: {
    status: 500,
    hint: 'Serial support (the "serialport" native module) is not installed. Run npm install in the server directory.',
  },
};

/**
 * @param {Error} err - a PaxError (has .code) or a generic error
 * @returns {{ status: number, body: object }}
 */
function toHttpError(err) {
  const code = err.code || 'ERROR';
  const entry = MAP[code] || { status: 500, hint: 'Unexpected server error.' };
  return {
    status: entry.status,
    body: {
      error: {
        code,
        message: err.message || 'Error',
        hint: entry.hint,
      },
    },
  };
}




let wss = null;

function attachWebSocket(server) {
  wss = new WebSocketServer({ server, path: '/ws' });
  wss.on('connection', (socket) => {
    socket.send(JSON.stringify({ type: 'HELLO', ts: Date.now() }));
    socket.on('error', () => {});
  });
  return wss;
}

/**
 * Broadcast a payment lifecycle event to all connected clients.
 * @param {object} evt - must include { type, txnId }
 */
function emitPaymentEvent(evt) {
  if (!wss) return;
  const payload = JSON.stringify({ ...evt, ts: Date.now() });
  for (const client of wss.clients) {
    if (client.readyState === 1 /* OPEN */) {
      try {
        client.send(payload);
      } catch {
        /* ignore individual client failures */
      }
    }
  }
}




const terminalsRouter = Router();

const MODELS = ['A920 Pro', 'A35'];

function validateTerminal(body, { partial = false } = {}) {
  const errors = [];
  if (!partial || body.name !== undefined) {
    if (!body.name || !String(body.name).trim()) errors.push('name is required');
  }
  if (body.connType !== undefined && !['tcp', 'usb'].includes(body.connType)) {
    errors.push("connType must be 'tcp' or 'usb'");
  }
  // Connection-type-specific required fields. On create we know the type; on a
  // partial update we only validate what is being changed.
  const type = body.connType || (partial ? undefined : 'tcp');
  if (type === 'usb') {
    if (!partial || body.serialPath !== undefined || body.connType !== undefined) {
      if (!body.serialPath || !String(body.serialPath).trim()) errors.push('serialPath is required for USB terminals');
    }
    if (body.baudRate !== undefined && (Number.isNaN(Number(body.baudRate)) || Number(body.baudRate) <= 0)) {
      errors.push('baudRate must be a positive number');
    }
  } else if (type === 'tcp') {
    if (!partial || body.ip !== undefined || body.connType !== undefined) {
      if (!body.ip || !String(body.ip).trim()) errors.push('ip is required for LAN (TCP) terminals');
    }
    if (body.port !== undefined && (Number.isNaN(Number(body.port)) || Number(body.port) <= 0)) {
      errors.push('port must be a positive number');
    }
  }
  if (body.model !== undefined && !MODELS.includes(body.model)) {
    errors.push(`model must be one of: ${MODELS.join(', ')}`);
  }
  return errors;
}

// GET /api/terminals/serial-ports -> list USB/serial ports on the host machine
// (helper for the "detect USB device" picker in the UI).
terminalsRouter.get('/serial-ports', async (_req, res) => {
  try {
    const ports = await listSerialPorts();
    res.json({ ports });
  } catch (err) {
    const { status, body } = toHttpError(err);
    res.status(status).json(body);
  }
});

// GET /api/terminals
terminalsRouter.get('/', (_req, res) => {
  res.json({ terminals: listTerminals() });
});

// POST /api/terminals
terminalsRouter.post('/', (req, res) => {
  const errors = validateTerminal(req.body);
  if (errors.length) return res.status(400).json({ error: { code: 'VALIDATION', message: errors.join('; ') } });
  const terminal = createTerminal(req.body);
  res.status(201).json({ terminal });
});

// PUT /api/terminals/:id
terminalsRouter.put('/:id', (req, res) => {
  const errors = validateTerminal(req.body, { partial: true });
  if (errors.length) return res.status(400).json({ error: { code: 'VALIDATION', message: errors.join('; ') } });
  const terminal = updateTerminal(req.params.id, req.body);
  if (!terminal) return res.status(404).json({ error: { code: 'NOT_FOUND', message: 'Terminal not found' } });
  res.json({ terminal });
});

// DELETE /api/terminals/:id
terminalsRouter.delete('/:id', (req, res) => {
  const ok = deleteTerminal(req.params.id);
  if (!ok) return res.status(404).json({ error: { code: 'NOT_FOUND', message: 'Terminal not found' } });
  res.status(204).end();
});

// POST /api/terminals/:id/ping  -> runs A00, returns terminal info + latency
terminalsRouter.post('/:id/ping', async (req, res) => {
  const terminal = getTerminal(req.params.id);
  if (!terminal) return res.status(404).json({ error: { code: 'NOT_FOUND', message: 'Terminal not found' } });
  try {
    const info = await initialize(terminal);
    res.json({ online: true, info });
  } catch (err) {
    const { status, body } = toHttpError(err);
    res.status(status).json({ online: false, ...body });
  }
});

// POST /api/terminals/:id/diagnose -> TCP/USB reachability without a full payment
terminalsRouter.post('/:id/diagnose', async (req, res) => {
  const terminal = getTerminal(req.params.id);
  if (!terminal) return res.status(404).json({ error: { code: 'NOT_FOUND', message: 'Terminal not found' } });
  try {
    const report = await diagnose(terminal);
    res.json({ report });
  } catch (err) {
    const { status, body } = toHttpError(err);
    res.status(status).json(body);
  }
});

// POST /api/terminals/:id/batch-close -> runs B00
terminalsRouter.post('/:id/batch-close', async (req, res) => {
  const terminal = getTerminal(req.params.id);
  if (!terminal) return res.status(404).json({ error: { code: 'NOT_FOUND', message: 'Terminal not found' } });

  if (isTerminalBusy(terminal)) {
    return res.status(409).json({ error: { code: 'TERMINAL_BUSY', message: 'Terminal is busy', hint: 'Wait for the current transaction to finish.' } });
  }

  const unsettled = listUnsettled().filter((t) => t.terminalId === terminal.id);
  try {
    const result = await batchClose(terminal);
    if (result.settled) {
      markSettled(unsettled.map((t) => t.id), {
        batchNumber: result.batchNumber,
        settledAt: new Date().toISOString(),
      });
    }
    res.json({ result, settledCount: result.settled ? unsettled.length : 0 });
  } catch (err) {
    const { status, body } = toHttpError(err);
    res.status(status).json(body);
  }
});





const paymentsRouter = Router();

// Map the low-level transport state -> user-facing lifecycle event.
const WS_STATE = {
  SENDING: 'SENDING',
  WAITING: 'WAITING_FOR_CARD',
  RECEIVING: 'PROCESSING',
};

/**
 * Shared runner for a payment-class command (sale/refund/void).
 * Handles logging, WS lifecycle, final-status mapping, and the timeout rule.
 *
 * @param {object} p
 * @param {object} p.terminal
 * @param {string} p.type       - SALE | RETURN | VOID | AUTH
 * @param {number} p.amountCents
 * @param {number} p.tipCents
 * @param {string} p.orderRef
 * @param {object} p.extra       - extra fields to store on the txn record
 * @param {(ecrRefNum, onState) => Promise<object>} p.invoke - calls the service
 * @param {import('express').Response} res
 */
const fmtMoney = (cents) => `$${(Number(cents || 0) / 100).toFixed(2)}`;

async function runPayment({ terminal, type, amountCents, tipCents = 0, orderRef, extra = {}, invoke }, res) {
  // Fail fast if the terminal is already busy (do not queue payments blindly).
  if (isTerminalBusy(terminal)) {
    return res.status(409).json({
      error: { code: 'TERMINAL_BUSY', message: 'Terminal is busy', hint: 'Wait for the current transaction to finish before starting another.' },
    });
  }

  const ecrRefNum = nextEcrRefNum();
  const txn = createTransaction({
    terminalId: terminal.id,
    terminalName: terminal.name,
    type,
    amountCents,
    tipCents,
    orderRef: orderRef || null,
    ecrRefNum,
    status: 'PENDING',
    ...extra,
  });

  // Audit trail: every payment attempt, in full, so it survives in the
  // downloadable logs even if the DB record is never inspected.
  console.log(
    `[payment] ${type} started — txnId=${txn.id} ecrRefNum=${ecrRefNum} terminal="${terminal.name}" ` +
      `amount=${fmtMoney(amountCents)} tip=${fmtMoney(tipCents)} orderRef=${orderRef || '-'}`,
  );

  const emit = (type_, payload = {}) => emitPaymentEvent({ type: type_, txnId: txn.id, ecrRefNum, ...payload });

  emit('SENDING');

  const onState = (state) => {
    const evt = WS_STATE[state];
    if (evt) emit(evt);
  };

  try {
    const result = await invoke(ecrRefNum, onState);
    const status = result.approved ? 'APPROVED' : 'DECLINED';
    const updated = updateTransaction(txn.id, {
      status,
      response: result,
      resultCode: result.resultCode,
      resultTxt: result.resultTxt,
      authCode: result.authCode,
      refNum: result.refNum,
      transactionNum: result.transactionNum,
      last4: result.last4,
      cardType: result.cardType,
      approvedAmountCents: result.approvedAmountCents,
    });
    console.log(
      `[payment] ${type} ${status} — txnId=${txn.id} ecrRefNum=${ecrRefNum} terminal="${terminal.name}" ` +
        `amount=${fmtMoney(result.approvedAmountCents ?? amountCents)} authCode=${result.authCode || '-'} ` +
        `card=${result.cardType || '-'} last4=${result.last4 || '-'} resultCode=${result.resultCode || '-'} ` +
        `resultTxt="${result.resultTxt || ''}"`,
    );
    emit(status, { result });
    res.status(status === 'APPROVED' ? 200 : 402).json({ transaction: updated, result });
    return updated;
  } catch (err) {
    if (err.code === 'TIMEOUT') {
      // CRITICAL: do not retry. Mark as unknown and tell the operator to verify.
      const updated = updateTransaction(txn.id, {
        status: 'TIMEOUT',
        unknown: true,
        error: { code: 'TIMEOUT', message: err.message },
      });
      console.error(
        `[payment] ${type} TIMEOUT — txnId=${txn.id} ecrRefNum=${ecrRefNum} terminal="${terminal.name}" ` +
          `amount=${fmtMoney(amountCents)} — CARD MAY HAVE BEEN CHARGED, verify on the terminal before retrying.`,
      );
      emit('TIMEOUT', { txnId: txn.id });
      const { body } = toHttpError(err);
      res.status(504).json({ transaction: updated, ...body });
      return updated;
    }

    const updated = updateTransaction(txn.id, {
      status: 'ERROR',
      error: { code: err.code || 'ERROR', message: err.message },
    });
    console.error(
      `[payment] ${type} ERROR — txnId=${txn.id} ecrRefNum=${ecrRefNum} terminal="${terminal.name}" ` +
        `amount=${fmtMoney(amountCents)} code=${err.code || 'ERROR'} message="${err.message || ''}"`,
    );
    emit('ERROR', { error: { code: err.code || 'ERROR', message: err.message } });
    const { status, body } = toHttpError(err);
    res.status(status).json({ transaction: updated, ...body });
    return updated;
  }
}

function requireTerminal(req, res) {
  const terminal = getTerminal(req.body.terminalId);
  if (!terminal) {
    res.status(404).json({ error: { code: 'NOT_FOUND', message: 'Terminal not found. Check terminalId.' } });
    return null;
  }
  return terminal;
}

function validAmount(v) {
  return Number.isInteger(v) && v > 0;
}

// POST /api/payments/sale  { terminalId, amountCents, tipCents?, orderRef? }
paymentsRouter.post('/sale', async (req, res) => {
  const terminal = requireTerminal(req, res);
  if (!terminal) return;
  const { amountCents, tipCents = 0, orderRef } = req.body;
  if (!validAmount(amountCents)) {
    return res.status(400).json({ error: { code: 'VALIDATION', message: 'amountCents must be a positive integer (cents)' } });
  }
  await runPayment(
    {
      terminal,
      type: 'SALE',
      amountCents,
      tipCents: Number.isInteger(tipCents) ? tipCents : 0,
      orderRef,
      invoke: (ecrRefNum, onState) => sale(terminal, amountCents, ecrRefNum, { tipCents, onState }),
    },
    res,
  );
});

// POST /api/payments/refund  { terminalId, amountCents, orderRef? }
paymentsRouter.post('/refund', async (req, res) => {
  const terminal = requireTerminal(req, res);
  if (!terminal) return;
  const { amountCents, orderRef } = req.body;
  if (!validAmount(amountCents)) {
    return res.status(400).json({ error: { code: 'VALIDATION', message: 'amountCents must be a positive integer (cents)' } });
  }
  await runPayment(
    {
      terminal,
      type: 'RETURN',
      amountCents,
      orderRef,
      invoke: (ecrRefNum, onState) => refund(terminal, amountCents, ecrRefNum, { onState }),
    },
    res,
  );
});

// POST /api/payments/void  { terminalId, origTxnId }  (void a same-day txn)
paymentsRouter.post('/void', async (req, res) => {
  const terminal = requireTerminal(req, res);
  if (!terminal) return;
  const { origTxnId } = req.body;
  const orig = origTxnId ? getTransaction(origTxnId) : null;
  if (!orig) {
    return res.status(404).json({ error: { code: 'NOT_FOUND', message: 'Original transaction not found (origTxnId).' } });
  }
  if (orig.status !== 'APPROVED') {
    return res.status(400).json({ error: { code: 'INVALID_STATE', message: 'Only APPROVED transactions can be voided.' } });
  }
  if (orig.settled) {
    return res.status(400).json({ error: { code: 'INVALID_STATE', message: 'Transaction already settled — issue a refund instead of a void.' } });
  }

  const voidTxn = await runPayment(
    {
      terminal,
      type: 'VOID',
      amountCents: orig.amountCents,
      orderRef: orig.orderRef,
      extra: { voidOfTxnId: orig.id, voidOfRefNum: orig.refNum },
      invoke: (ecrRefNum, onState) =>
        voidTransaction(terminal, orig.refNum, ecrRefNum, {
          amountCents: orig.amountCents,
          onState,
        }),
    },
    res,
  );

  // If the void was approved, flag the original transaction as VOIDED.
  if (voidTxn && voidTxn.status === 'APPROVED') {
    updateTransaction(orig.id, { status: 'VOIDED', voidedByTxnId: voidTxn.id });
  }
});

// GET /api/payments  (history, paginated, filter by date/status/type)
paymentsRouter.get('/', (req, res) => {
  const { status, type, from, to, terminalId, page, pageSize } = req.query;
  const result = queryTransactions({ status, type, from, to, terminalId, page, pageSize });
  res.json(result);
});

// GET /api/payments/:id
paymentsRouter.get('/:id', (req, res) => {
  const txn = getTransaction(req.params.id);
  if (!txn) return res.status(404).json({ error: { code: 'NOT_FOUND', message: 'Transaction not found' } });
  res.json({ transaction: txn });
});



const app = express();
const PORT = process.env.PORT || 5000;

app.use(cors());
app.use(express.json());

app.get('/api/health', (_req, res) => {
  res.json({
    status: 'ok',
    message: 'Server is running',
    bridgeVersion: 'node-dev',
  });
});

app.get('/api', (_req, res) => {
  res.json({ message: 'Welcome to the API' });
});

app.use('/api/terminals', terminalsRouter);
app.use('/api/payments', paymentsRouter);

const server = http.createServer(app);
attachWebSocket(server);

server.listen(PORT, () => {
  console.log(`Server listening on http://localhost:${PORT}`);
  console.log(`WebSocket lifecycle events on ws://localhost:${PORT}/ws`);
  console.log('PAX mode: live terminal');
});
