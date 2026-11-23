/* global pax */
const $ = (id) => document.getElementById(id);
const state = { port: 5000, status: 'stopped', appVersion: '—' };

// ---- tabs -----------------------------------------------------------------
document.querySelectorAll('.tab').forEach((t) => {
  t.addEventListener('click', () => {
    document.querySelectorAll('.tab').forEach((x) => x.classList.remove('active'));
    document.querySelectorAll('.panel').forEach((x) => x.classList.remove('active'));
    t.classList.add('active');
    document.querySelector(`.panel[data-panel="${t.dataset.tab}"]`).classList.add('active');
    if (t.dataset.tab === 'terminals') loadTerminals();
  });
});

// ---- status ---------------------------------------------------------------
const STATUS_HINT = {
  running: 'Serving requests. Payments can be taken now.',
  starting: 'Starting up…',
  stopped: 'Not running — press Start before taking payments.',
  error: 'The bridge stopped unexpectedly. Check the Logs tab.',
};

function paintStatus(s) {
  state.status = s.status;
  state.port = s.port ?? state.port;
  const pill = $('statusPill');
  pill.className = `status-pill ${s.status}`;
  $('statusText').textContent = s.status;
  $('glanceStatus').textContent = s.status;
  $('glancePort').textContent = state.port;
  $('bridgeUrl').textContent = `http://localhost:${state.port}`;
  $('heroBeacon').className = `beacon ${s.status}`;
  $('heroHint').textContent = STATUS_HINT[s.status] || s.status;
  const running = s.status === 'running';
  $('btnStart').disabled = running || s.status === 'starting';
  $('btnStop').disabled = !running && s.status !== 'error';
  $('btnOpen').disabled = !running;
}

const PLATFORM_NAME = { darwin: 'macOS', win32: 'Windows', linux: 'Linux' };

async function refreshState() {
  paintStatus(await pax.bridge.state());
  const info = await pax.app.info();
  state.appVersion = info.version;
  $('glanceVersion').textContent = `v${info.version}`;
  $('appMeta').textContent = `v${info.version} · ${PLATFORM_NAME[info.platform] || info.platform}`;
}

$('btnStart').addEventListener('click', () => pax.bridge.start(state.port));
$('btnStop').addEventListener('click', () => pax.bridge.stop());
$('btnRestart').addEventListener('click', () => pax.bridge.restart(state.port));
$('btnOpen').addEventListener('click', () => pax.app.openExternal(`http://localhost:${state.port}`));
$('btnData').addEventListener('click', () => pax.app.openUserData());

pax.bridge.onStatus(paintStatus);

// ---- terminals (talk to the local bridge REST API) ------------------------
const api = (path, opts) => fetch(`http://localhost:${state.port}/api${path}`, opts).then(async (r) => {
  const body = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(body?.error?.message || `HTTP ${r.status}`);
  return body;
});

const FALLBACK_DRIVERS = [
  {
    vendor: 'pax', label: 'PAX (BroadPOS / POSLink)',
    models: ['A920 Pro', 'A35', 'A80', 'S300', 'E500', 'E700'],
    defaultModel: 'A920 Pro', defaultConnType: 'tcp',
    connTypes: [{ id: 'tcp', label: 'LAN (Wi-Fi / Ethernet)', defaultPort: 10009, requiresIp: true, requiresPort: true }, { id: 'usb', label: 'USB Serial', requiresSerial: true }],
    capabilities: { pair: false }, fields: [], setupSteps: [],
  },
];

let driversCatalog = FALLBACK_DRIVERS;
let bridgeWs = null;

function ensurePairingWs() {
  if (bridgeWs && bridgeWs.readyState <= 1) return;
  try {
    bridgeWs = new WebSocket(`ws://localhost:${state.port}/ws`);
    bridgeWs.onmessage = (ev) => {
      try {
        const msg = JSON.parse(ev.data);
        if (msg.type === 'PAIRING_CODE' && msg.pairingCode) {
          $('pairingCodeDisplay').textContent = msg.pairingCode;
          $('pairingBanner').hidden = false;
        } else if (msg.type === 'PAIRED') {
          $('pairingBanner').hidden = true;
          loadTerminals();
        }
      } catch { /* ignore */ }
    };
    bridgeWs.onclose = () => { bridgeWs = null; };
  } catch { /* ignore */ }
}

$('btnClosePairing')?.addEventListener('click', () => { $('pairingBanner').hidden = true; });

function currentDriver() {
  const v = $('termVendor')?.value || 'pax';
  return driversCatalog.find((d) => d.vendor === v) || driversCatalog[0];
}

function syncTerminalForm() {
  const drv = currentDriver();
  if (!drv) return;
  const modelSel = $('termModel');
  const prevModel = modelSel.value;
  modelSel.innerHTML = '';
  for (const m of drv.models || []) {
    const opt = document.createElement('option');
    opt.value = m; opt.textContent = m;
    modelSel.appendChild(opt);
  }
  modelSel.value = (drv.models || []).includes(prevModel) ? prevModel : (drv.defaultModel || drv.models?.[0] || '');

  const connSel = $('termConnType');
  const prevConn = connSel.value;
  connSel.innerHTML = '';
  for (const ct of drv.connTypes || []) {
    const opt = document.createElement('option');
    opt.value = ct.id; opt.textContent = ct.label;
    connSel.appendChild(opt);
  }
  const validConn = (drv.connTypes || []).some((c) => c.id === prevConn) ? prevConn : (drv.defaultConnType || drv.connTypes?.[0]?.id || 'tcp');
  connSel.value = validConn;

  syncConnFields();
}

function syncConnFields() {
  const drv = currentDriver();
  const connId = $('termConnType').value;
  const ct = (drv.connTypes || []).find((c) => c.id === connId) || {};
  $('termLanRow').hidden = !ct.requiresIp;
  $('termIp').required = Boolean(ct.requiresIp);
  if (ct.defaultPort) $('termPort').value = ct.defaultPort;
  $('termSerialRow').hidden = !ct.requiresSerial;
  $('termSerial').required = Boolean(ct.requiresSerial);

  const wrap = $('termVendorFields');
  wrap.innerHTML = '';
  for (const f of drv.fields || []) {
    if (f.connTypes && !f.connTypes.includes(connId)) continue;
    const lbl = document.createElement('label');
    lbl.textContent = f.label;
    const required = (f.requiredFor || []).includes(connId);
    if (f.type === 'select') {
      const sel = document.createElement('select');
      sel.name = `vc_${f.key}`;
      for (const o of f.options || []) {
        const opt = document.createElement('option');
        opt.value = o; opt.textContent = o;
        sel.appendChild(opt);
      }
      if (f.default) sel.value = f.default;
      lbl.appendChild(sel);
    } else {
      const inp = document.createElement('input');
      inp.name = `vc_${f.key}`;
      inp.type = f.type || 'text';
      if (f.placeholder) inp.placeholder = f.placeholder;
      if (f.default != null) inp.value = f.default;
      inp.required = required;
      lbl.appendChild(inp);
    }
    wrap.appendChild(lbl);
  }

  $('termSetupHint').textContent = (drv.setupSteps || []).join(' ');
}

$('termVendor')?.addEventListener('change', syncTerminalForm);
$('termConnType')?.addEventListener('change', syncConnFields);

async function loadDrivers() {
  if (state.status !== 'running') return;
  try {
    const { drivers } = await api('/drivers');
    if (Array.isArray(drivers) && drivers.length) {
      driversCatalog = drivers;
      const sel = $('termVendor');
      const cur = sel.value;
      sel.innerHTML = '';
      for (const d of drivers) {
        const opt = document.createElement('option');
        opt.value = d.vendor; opt.textContent = d.label;
        sel.appendChild(opt);
      }
      if (drivers.some((d) => d.vendor === cur)) sel.value = cur;
      syncTerminalForm();
    }
  } catch { /* older bridge without /api/drivers */ }
}

async function loadTerminals() {
  const list = $('termList');
  if (state.status !== 'running') {
    list.innerHTML = '<p class="muted">Start the bridge to manage terminals.</p>';
    $('glanceTerminals').textContent = '—';
    return;
  }
  ensurePairingWs();
  if (driversCatalog === FALLBACK_DRIVERS) await loadDrivers();
  try {
    const { terminals } = await api('/terminals');
    $('glanceTerminals').textContent = terminals.length;
    list.innerHTML = terminals.length
      ? ''
      : '<p class="muted">No terminals yet. Add one on the left.</p>';
    for (const t of terminals) list.appendChild(termRow(t));
  } catch (e) {
    list.innerHTML = `<p class="muted">Could not load: ${e.message}</p>`;
  }
}

function termRow(t) {
  const el = document.createElement('div');
  el.className = 'term';
  const vendor = (t.vendor || 'pax').toUpperCase();
  const target = t.connType === 'usb'
    ? `USB ${t.serialPath || ''}`
    : t.connType === 'cloud'
      ? `Cloud (${t.vendorConfig?.epi || t.vendorConfig?.deviceId || t.vendorConfig?.tpn || t.vendorConfig?.poiId || 'API'})`
      : `${t.ip || 'LAN'}:${t.port || ''}`;
  const showPair = t.vendor === 'clover' && (t.connType === 'snpd' || t.connType === 'tcp');
  const pairedBadge = showPair && t.vendorConfig?.authToken ? ' · paired' : '';
  el.innerHTML = `
    <div>
      <div><b>${escapeHtml(t.name)}</b> <span class="badge" data-badge>?</span></div>
      <small>${escapeHtml(vendor)} · ${escapeHtml(t.model || '')} · ${escapeHtml(target)}${escapeHtml(pairedBadge)}</small>
    </div>
    <div class="term-actions">
      ${showPair ? '<button class="btn sm" data-pair>Pair</button>' : ''}
      <button class="btn sm" data-test>Test</button>
      <button class="btn sm ghost" data-del>Delete</button>
    </div>`;
  const pairBtn = el.querySelector('[data-pair]');
  if (pairBtn) {
    pairBtn.addEventListener('click', async (ev) => {
      const badge = el.querySelector('[data-badge]');
      ev.target.disabled = true; badge.textContent = 'pairing…'; badge.className = 'badge';
      try {
        await api(`/terminals/${t.id}/pair`, { method: 'POST' });
        badge.textContent = 'paired'; badge.className = 'badge on';
        $('pairingBanner').hidden = true;
        loadTerminals();
      } catch (e) {
        badge.textContent = 'pair failed'; badge.className = 'badge off'; badge.title = e.message;
      } finally { ev.target.disabled = false; }
    });
  }
  el.querySelector('[data-test]').addEventListener('click', async (ev) => {
    const badge = el.querySelector('[data-badge]');
    ev.target.disabled = true; badge.textContent = '…'; badge.className = 'badge';
    try {
      await api(`/terminals/${t.id}/ping`, { method: 'POST' });
      badge.textContent = 'online'; badge.className = 'badge on';
    } catch (e) {
      badge.textContent = 'offline'; badge.className = 'badge off'; badge.title = e.message;
    } finally { ev.target.disabled = false; }
  });
  el.querySelector('[data-del]').addEventListener('click', async () => {
    if (!confirm(`Delete terminal "${t.name}"?`)) return;
    await api(`/terminals/${t.id}`, { method: 'DELETE' });
    loadTerminals();
  });
  return el;
}

$('termForm').addEventListener('submit', async (e) => {
  e.preventDefault();
  const msg = $('termMsg');
  msg.textContent = ''; msg.className = 'form-msg';
  const fd = Object.fromEntries(new FormData(e.target).entries());
  const vendorConfig = {};
  for (const [k, v] of Object.entries(fd)) {
    if (k.startsWith('vc_') && String(v).trim() !== '') {
      const key = k.slice(3);
      vendorConfig[key] = key.toLowerCase().includes('port') ? Number(v) : String(v).trim();
    }
  }
  try {
    await api('/terminals', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        name: fd.name,
        vendor: fd.vendor || 'pax',
        model: fd.model,
        connType: fd.connType || 'tcp',
        ip: fd.ip || '',
        port: Number(fd.port) || 0,
        serialPath: fd.serialPath || '',
        baudRate: Number(fd.baudRate) || 115200,
        vendorConfig,
      }),
    });
    msg.textContent = 'Saved.'; msg.className = 'form-msg ok';
    e.target.reset();
    syncTerminalForm();
    loadTerminals();
  } catch (err) {
    msg.textContent = err.message; msg.className = 'form-msg err';
  }
});
$('btnClearForm').addEventListener('click', () => { $('termForm').reset(); syncTerminalForm(); });
$('btnReloadTerms').addEventListener('click', loadTerminals);

// ---- settings -------------------------------------------------------------
async function loadSettings() {
  const s = await pax.settings.get();
  $('setLogin').checked = s.launchAtLogin;
  $('setAutostart').checked = s.startBridgeOnLaunch;
  $('setTray').checked = s.minimizeToTray;
  $('setShowTray').checked = s.showTrayIcon ?? true;
  $('setAutoUpdate').checked = s.autoUpdate;
  $('setPort').value = s.port ?? state.port;
}
const bindToggle = (id, key) => $(id).addEventListener('change', (e) => pax.settings.set({ [key]: e.target.checked }));
bindToggle('setLogin', 'launchAtLogin');
bindToggle('setAutostart', 'startBridgeOnLaunch');
bindToggle('setTray', 'minimizeToTray');
bindToggle('setShowTray', 'showTrayIcon');
bindToggle('setAutoUpdate', 'autoUpdate');

$('portForm').addEventListener('submit', async (e) => {
  e.preventDefault();
  const msg = $('portMsg');
  const port = Number($('setPort').value);
  msg.textContent = ''; msg.className = 'form-msg';
  try {
    await pax.settings.set({ port });
    msg.textContent = state.status === 'stopped'
      ? 'Saved. It will be used next time you start the bridge.'
      : 'Saved. Restart the bridge to switch to this port.';
    msg.className = 'form-msg ok';
  } catch (err) {
    msg.textContent = err.message || String(err);
    msg.className = 'form-msg err';
  }
});

// ---- logs -----------------------------------------------------------------
const logView = $('logView');
function appendLog(entry) {
  const time = new Date(entry.ts).toLocaleTimeString();
  const cls = entry.stream === 'err' ? 'l-err' : entry.stream === 'sys' ? 'l-sys' : '';
  const line = document.createElement('div');
  line.className = cls;
  line.innerHTML = `<span class="l-time">${time}</span>  ${escapeHtml(entry.line)}`;
  logView.appendChild(line);
  while (logView.childElementCount > 800) logView.removeChild(logView.firstChild);
  logView.scrollTop = logView.scrollHeight;
}
pax.bridge.onLog(appendLog);
$('btnClearLogs').addEventListener('click', () => (logView.innerHTML = ''));

$('btnDownloadLogs').addEventListener('click', async (e) => {
  const btn = e.currentTarget;
  const original = btn.textContent;
  btn.disabled = true; btn.textContent = 'Saving…';
  try {
    // alert() is a no-op in the Tauri webview — the file saved fine but the
    // click looked like it did nothing. Report through the toast instead.
    const res = await pax.logs.download();
    if (res.ok) {
      showToast('Logs saved', res.filePath, {
        kind: 'ok',
        autoHideMs: 12000,
        actions: [
          {
            label: 'Show in folder',
            primary: true,
            onClick: () => pax.app.openExternal(`file://${res.filePath.replace(/\/[^/]+$/, '')}`),
          },
        ],
      });
    } else if (res.reason === 'empty') {
      showToast('No logs yet', 'Start the bridge and process a payment first.', { kind: 'warn', autoHideMs: 5000 });
    } else {
      showToast('Could not save logs', res?.message || 'Unknown error.', { kind: 'err', autoHideMs: 6000 });
    }
  } finally {
    btn.disabled = false; btn.textContent = original;
  }
});

// ---- updates (lightweight toast) ------------------------------------------
const toast = $('updateToast');
let toastTimer = null;
let userAskedForUpdate = false;

function hideToast() {
  if (toastTimer) { clearTimeout(toastTimer); toastTimer = null; }
  toast.classList.remove('is-open');
}

function showToast(title, msg, { actions = [], progress = false, indeterminate = false, kind = 'info', autoHideMs = 0, dismissible = true } = {}) {
  if (toastTimer) { clearTimeout(toastTimer); toastTimer = null; }

  $('toastTitle').textContent = title;
  const text = String(msg ?? '');
  $('toastMsg').textContent = text.length > 300 ? `${text.slice(0, 300)}…` : text;
  $('toastProgress').hidden = !progress;
  $('toastProgress').classList.toggle('is-indeterminate', progress && indeterminate);
  if (progress && indeterminate) $('toastBar').style.width = '100%';
  $('toastClose').hidden = !dismissible;

  toast.classList.remove('is-info', 'is-ok', 'is-warn', 'is-err');
  toast.classList.add(`is-${kind}`, 'is-open');

  const wrap = $('toastActions');
  wrap.innerHTML = '';
  for (const a of actions) {
    const b = document.createElement('button');
    b.type = 'button';
    b.className = `btn sm ${a.primary ? 'primary' : 'ghost'}`;
    b.textContent = a.label;
    b.addEventListener('click', (ev) => {
      ev.preventDefault();
      ev.stopPropagation();
      a.onClick();
    });
    wrap.appendChild(b);
  }

  if (autoHideMs > 0) {
    toastTimer = setTimeout(hideToast, autoHideMs);
  }
}

$('toastClose').addEventListener('click', (ev) => {
  ev.preventDefault();
  ev.stopPropagation();
  hideToast();
});

$('btnUpdate').addEventListener('click', () => {
  userAskedForUpdate = true;
  pax.updates.check();
});

pax.updates.onEvent((e) => {
  switch (e.type) {
    case 'checking':
      if (!userAskedForUpdate) break; // silent background checks
      showToast('Checking for updates…', 'Contacting the release server.', { kind: 'info' });
      break;
    case 'none':
      if (!userAskedForUpdate) break; // don't nag on every launch
      userAskedForUpdate = false;
      showToast('You’re up to date', `Version ${e.current} is the latest.`, {
        kind: 'ok',
        autoHideMs: 3500,
      });
      break;
    case 'available':
      userAskedForUpdate = false;
      showToast(`Update available — v${e.version}`, 'A new version is ready to download.', {
        kind: 'warn',
        actions: [
          { label: 'Later', onClick: hideToast },
          {
            label: 'Download',
            primary: true,
            onClick: () => {
              // Show movement immediately — the first byte can be seconds away.
              showToast(`Downloading v${e.version}…`, 'Starting download…', {
                kind: 'info',
                progress: true,
                indeterminate: true,
              });
              pax.updates.download();
            },
          },
        ],
      });
      break;
    case 'download-start':
      showToast(`Downloading v${e.version}…`, 'Starting download…', {
        kind: 'info',
        progress: true,
        indeterminate: true,
      });
      break;
    case 'progress': {
      const size = e.total ? ` of ${(e.total / 1e6).toFixed(1)} MB` : '';
      showToast('Downloading update…', `${e.percent}%${size} · ${(e.bytesPerSecond / 1e6).toFixed(1)} MB/s`, {
        kind: 'info',
        progress: true,
      });
      $('toastBar').style.width = `${e.percent}%`;
      break;
    }
    case 'downloaded':
      showToast(`Ready to install — v${e.version}`, 'The update will apply on restart.', {
        kind: 'ok',
        actions: [
          { label: 'Later', onClick: hideToast },
          {
            label: 'Restart & install',
            primary: true,
            onClick: () => {
              showToast('Installing update…', 'Please wait — the app will restart on its own.', {
                kind: 'info',
                progress: true,
                indeterminate: true,
                dismissible: false,
              });
              pax.updates.install();
            },
          },
        ],
      });
      break;
    case 'installing':
      showToast('Installing update…', 'Please wait — the app will restart on its own.', {
        kind: 'info',
        progress: true,
        indeterminate: true,
        dismissible: false,
      });
      break;
    case 'restarting':
      showToast('Restarting…', 'Reopening with the new version.', {
        kind: 'ok',
        progress: true,
        indeterminate: true,
        dismissible: false,
      });
      break;
    case 'dev':
      userAskedForUpdate = false;
      showToast('Dev mode', e.message, { kind: 'info', autoHideMs: 4000 });
      break;
    case 'error':
      if (!userAskedForUpdate && !toast.classList.contains('is-open')) break;
      userAskedForUpdate = false;
      showToast('Update error', e.message, { kind: 'err', autoHideMs: 6000 });
      break;
    default:
      break;
  }
});

// ---- utils ----------------------------------------------------------------
function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
}

// ---- boot -----------------------------------------------------------------
(async function boot() {
  await refreshState();
  await loadSettings();
  // hydrate log view with any buffered lines
  (await pax.bridge.logs()).forEach(appendLog);
  // poll terminal count on the status page while running
  setInterval(() => { if (state.status === 'running') loadTerminals(); }, 8000);
})();
