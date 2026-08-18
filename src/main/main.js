const fs = require('node:fs');
const path = require('node:path');
const { app, BrowserWindow, ipcMain, shell, Menu, dialog } = require('electron');
const store = require('./store');
const { Bridge } = require('./bridge');
const { createTray } = require('./tray');
const { initUpdater } = require('./updater');

const APP_TITLE = 'Salesgent Hardware Bridge';
// Fixed on purpose — not user-configurable (see registerIpc's settings:set guard).
const BRIDGE_PORT = 5000;

// Single-instance: a second launch just focuses the existing window.
if (!app.requestSingleInstanceLock()) {
  app.quit();
  process.exit(0);
}

let win = null;
let tray = null;
let bridge = null;
let updater = null;
let isQuitting = false;

const send = (channel, payload) => {
  if (win && !win.isDestroyed()) win.webContents.send(channel, payload);
};

function createWindow() {
  win = new BrowserWindow({
    width: 1040,
    height: 720,
    minWidth: 900,
    minHeight: 600,
    show: false,
    backgroundColor: '#0b0f1a',
    title: APP_TITLE,
    icon: path.join(__dirname, '..', '..', 'build', 'icon.png'),
    webPreferences: {
      preload: path.join(__dirname, '..', 'preload', 'preload.js'),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: false,
    },
  });

  win.loadFile(path.join(__dirname, '..', 'renderer', 'index.html'));
  win.once('ready-to-show', () => win.show());

  // Closing the window keeps the bridge alive in the tray (unless quitting).
  win.on('close', (e) => {
    if (!isQuitting && store.read().minimizeToTray) {
      e.preventDefault();
      win.hide();
    }
  });

  // Open external links in the OS browser, never in-app.
  win.webContents.setWindowOpenHandler(({ url }) => {
    shell.openExternal(url);
    return { action: 'deny' };
  });
}

function showWindow() {
  if (!win) createWindow();
  else {
    win.show();
    win.focus();
  }
}

function applyLoginItem(enabled) {
  if (process.platform === 'linux') return; // not supported the same way
  try {
    app.setLoginItemSettings({ openAtLogin: !!enabled, openAsHidden: true });
  } catch (err) {
    // Sandboxed/managed environments may deny this — not fatal.
    console.warn('[login-item] could not update:', err.message);
  }
}

app.on('second-instance', showWindow);

app.whenReady().then(async () => {
  if (process.platform === 'darwin') Menu.setApplicationMenu(Menu.buildFromTemplate(defaultMenu()));

  const settings = store.read();
  applyLoginItem(settings.launchAtLogin);

  bridge = new Bridge({
    onLog: (entry) => send('bridge:log', entry),
    onStatus: (state) => {
      send('bridge:status', state);
      tray?.refresh();
    },
  });

  createWindow();
  tray = createTray({
    onShow: showWindow,
    onQuit: quit,
    onRestart: () => bridge.restart(BRIDGE_PORT),
    getState: () => bridge.getState(),
  });

  updater = initUpdater({ send, getSettings: () => store.read() });

  registerIpc();

  if (settings.startBridgeOnLaunch) bridge.start(BRIDGE_PORT);
});

// Keep running in the tray after the last window closes.
app.on('window-all-closed', () => {
  if (process.platform !== 'darwin') {
    // handled by tray; do nothing so the app stays alive
  }
});

app.on('activate', showWindow);
app.on('before-quit', () => (isQuitting = true));

async function quit() {
  isQuitting = true;
  try {
    await bridge?.stop();
  } catch {
    /* ignore */
  }
  app.quit();
}

function registerIpc() {
  ipcMain.handle('app:info', () => ({
    name: app.getName(),
    version: app.getVersion(),
    platform: process.platform,
    electron: process.versions.electron,
    node: process.versions.node,
    userData: app.getPath('userData'),
    packaged: app.isPackaged,
  }));

  // The bridge port is fixed (BRIDGE_PORT) — never taken from the renderer,
  // so nothing in the UI (or a compromised page) can move it.
  ipcMain.handle('bridge:state', () => bridge.getState());
  ipcMain.handle('bridge:logs', () => bridge.logs);
  ipcMain.handle('bridge:start', () => bridge.start(BRIDGE_PORT));
  ipcMain.handle('bridge:stop', () => bridge.stop());
  ipcMain.handle('bridge:restart', () => bridge.restart(BRIDGE_PORT));

  // Full, unbounded log history (every payment event ever logged) exported to
  // a file the merchant picks — not just the ~500-line in-memory buffer.
  ipcMain.handle('logs:download', async () => {
    const content = bridge.readAllLogs();
    if (!content) return { ok: false, reason: 'empty' };
    const stamp = new Date().toISOString().replace(/[:.]/g, '-');
    const { canceled, filePath } = await dialog.showSaveDialog(win, {
      title: 'Save Salesgent Hardware Bridge logs',
      defaultPath: path.join(app.getPath('desktop'), `salesgent-pax-bridge-logs-${stamp}.log`),
      filters: [{ name: 'Log file', extensions: ['log', 'txt'] }],
    });
    if (canceled || !filePath) return { ok: false, reason: 'cancelled' };
    fs.writeFileSync(filePath, content);
    return { ok: true, filePath };
  });

  ipcMain.handle('settings:get', () => store.read());
  ipcMain.handle('settings:set', (_e, patch) => {
    // Port is not a user setting — strip it even if a caller tries to sneak it in.
    const { port: _ignoredPort, ...safePatch } = patch || {};
    const next = store.write(safePatch);
    if ('launchAtLogin' in safePatch) applyLoginItem(next.launchAtLogin);
    return next;
  });

  ipcMain.handle('update:check', () => updater.check());
  ipcMain.handle('update:download', () => updater.download());
  ipcMain.handle('update:install', () => updater.quitAndInstall());

  ipcMain.handle('open:userData', () => shell.openPath(app.getPath('userData')));
  ipcMain.handle('open:external', (_e, url) => shell.openExternal(url));
  ipcMain.handle('app:quit', quit);
}

function defaultMenu() {
  return [
    { role: 'appMenu' },
    { role: 'editMenu' },
    { role: 'viewMenu' },
    { role: 'windowMenu' },
  ];
}
