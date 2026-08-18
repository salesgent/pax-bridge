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

async function loadTerminals() {
  const list = $('termList');
  if (state.status !== 'running') {
    list.innerHTML = '<p class="muted">Start the bridge to manage terminals.</p>';
    $('glanceTerminals').textContent = '—';
    return;
  }
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
  el.innerHTML = `
    <div>
      <div><b>${escapeHtml(t.name)}</b> <span class="badge" data-badge>?</span></div>
      <small>${escapeHtml(t.model || '')} · ${escapeHtml(t.ip || 'LAN')}:${t.port || ''}</small>
    </div>
    <div class="term-actions">
      <button class="btn sm" data-test>Test</button>
      <button class="btn sm ghost" data-del>Delete</button>
    </div>`;
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
  try {
    await api('/terminals', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        name: fd.name, model: fd.model, connType: 'tcp',
        ip: fd.ip, port: Number(fd.port) || 10009,
      }),
    });
    msg.textContent = 'Saved.'; msg.className = 'form-msg ok';
    e.target.reset();
    loadTerminals();
  } catch (err) {
    msg.textContent = err.message; msg.className = 'form-msg err';
  }
});
$('btnClearForm').addEventListener('click', () => $('termForm').reset());
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
