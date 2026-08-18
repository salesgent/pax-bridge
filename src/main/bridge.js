// Manages the embedded PAX Bridge (the Express server from bridge/index.js).
//
// The bridge runs in a separate Node context via Electron's utilityProcess, so
// a crash in the payment server can never take down the UI. We stream its
// stdout/stderr to the renderer as live logs, persist every line to a rotating
// daily file (so the full payment history survives restarts and can be
// exported), and poll /api/health to know when it's actually accepting
// connections.
const fs = require('node:fs');
const path = require('node:path');
const http = require('node:http');
const { app, utilityProcess } = require('electron');

const MAX_LOG_LINES = 500; // in-memory buffer shown live in the UI

function todayLogFile(logsDir) {
  const d = new Date();
  const stamp = `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
  return path.join(logsDir, `bridge-${stamp}.log`);
}

class Bridge {
  constructor({ onLog, onStatus }) {
    this.proc = null;
    this.port = 5000;
    this.status = 'stopped'; // stopped | starting | running | error
    this.logs = [];
    this.onLog = onLog || (() => {});
    this.onStatus = onStatus || (() => {});
    this.logsDir = path.join(app.getPath('userData'), 'logs');
    fs.mkdirSync(this.logsDir, { recursive: true });
  }

  /** Absolute path to bridge/index.js in dev and when packaged. */
  entryPath() {
    // In production the bridge/ folder ships as an unpacked resource (see
    // electron-builder.yml → extraResources), so it resolves next to the app.
    if (app.isPackaged) {
      return path.join(process.resourcesPath, 'bridge', 'index.js');
    }
    return path.join(__dirname, '..', '..', 'bridge', 'index.js');
  }

  _setStatus(status, extra = {}) {
    this.status = status;
    this.onStatus({ status, port: this.port, ...extra });
  }

  _pushLog(line, stream = 'out') {
    const entry = { ts: Date.now(), stream, line: String(line).replace(/\s+$/, '') };
    if (!entry.line) return;
    this.logs.push(entry);
    if (this.logs.length > MAX_LOG_LINES) this.logs.shift();
    this.onLog(entry);
    this._appendToFile(entry);
  }

  /** Append one line to today's rotating log file (full, unbounded history). */
  _appendToFile(entry) {
    try {
      const time = new Date(entry.ts).toISOString();
      const tag = entry.stream === 'err' ? 'ERR' : entry.stream === 'sys' ? 'SYS' : 'OUT';
      fs.appendFileSync(todayLogFile(this.logsDir), `${time} [${tag}] ${entry.line}\n`);
    } catch (err) {
      // Never let a disk/logging failure take down the bridge.
      console.error('[bridge] failed to write log file:', err.message);
    }
  }

  /**
   * Every log file's contents, concatenated oldest → newest, for a full
   * payment-history export. Returns '' if nothing has been logged yet.
   */
  readAllLogs() {
    let files = [];
    try {
      files = fs
        .readdirSync(this.logsDir)
        .filter((f) => /^bridge-\d{4}-\d{2}-\d{2}\.log$/.test(f))
        .sort();
    } catch {
      return '';
    }
    return files
      .map((f) => {
        const body = fs.readFileSync(path.join(this.logsDir, f), 'utf8');
        return `\n===== ${f} =====\n${body}`;
      })
      .join('');
  }

  getState() {
    return { status: this.status, port: this.port, url: `http://localhost:${this.port}` };
  }

  async start(port = 5000) {
    if (this.proc) await this.stop();
    this.port = Number(port) || 5000;
    this._setStatus('starting');
    this._pushLog(`Starting Salesgent Hardware Bridge on port ${this.port}…`, 'sys');

    const env = {
      ...process.env,
      // Keep all writable state (config + DB) in the app's userData folder.
      PAX_HOME: app.getPath('userData'),
      PORT: String(this.port),
      NODE_ENV: 'production',
    };

    this.proc = utilityProcess.fork(this.entryPath(), [], {
      stdio: 'pipe',
      env,
      serviceName: 'pax-bridge',
    });

    this.proc.stdout?.on('data', (d) => d.toString().split('\n').forEach((l) => this._pushLog(l, 'out')));
    this.proc.stderr?.on('data', (d) => d.toString().split('\n').forEach((l) => this._pushLog(l, 'err')));

    this.proc.on('exit', (code) => {
      this._pushLog(`Bridge process exited (code ${code}).`, 'sys');
      this.proc = null;
      if (this.status !== 'stopped') this._setStatus('error', { code });
    });

    // Wait for the server to actually accept connections.
    const ok = await this._waitForHealth(15000);
    if (ok) {
      this._setStatus('running');
      this._pushLog(`Bridge is live at http://localhost:${this.port}`, 'sys');
    } else {
      this._setStatus('error', { reason: 'health-timeout' });
      this._pushLog('Bridge did not become healthy in time.', 'err');
    }
    return this.getState();
  }

  async stop() {
    if (!this.proc) {
      this._setStatus('stopped');
      return;
    }
    this._pushLog('Stopping bridge…', 'sys');
    const dead = new Promise((res) => this.proc.once('exit', res));
    this._setStatus('stopped');
    this.proc.kill();
    await Promise.race([dead, new Promise((r) => setTimeout(r, 3000))]);
    this.proc = null;
  }

  async restart(port) {
    await this.stop();
    return this.start(port || this.port);
  }

  _waitForHealth(timeoutMs) {
    const deadline = Date.now() + timeoutMs;
    const url = `http://127.0.0.1:${this.port}/api/health`;
    return new Promise((resolve) => {
      const tick = () => {
        const req = http.get(url, (res) => {
          res.resume();
          if (res.statusCode === 200) return resolve(true);
          retry();
        });
        req.on('error', retry);
        req.setTimeout(1500, () => req.destroy());
      };
      const retry = () => {
        if (Date.now() > deadline) return resolve(false);
        setTimeout(tick, 400);
      };
      tick();
    });
  }
}

module.exports = { Bridge };
