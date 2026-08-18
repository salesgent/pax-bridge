// System-tray icon + menu. Lets the merchant leave the bridge running in the
// background and reopen the window from the tray.
const path = require('node:path');
const { Tray, Menu, nativeImage, app } = require('electron');

function trayIcon() {
  // A template image renders crisply in the macOS menu bar (auto light/dark).
  const file = process.platform === 'darwin' ? 'trayTemplate.png' : 'icon.png';
  const img = nativeImage.createFromPath(path.join(__dirname, '..', '..', 'build', file));
  if (process.platform === 'darwin') img.setTemplateImage(true);
  return img.isEmpty() ? nativeImage.createEmpty() : img;
}

function createTray({ onShow, onQuit, getState, onRestart }) {
  const tray = new Tray(trayIcon());
  tray.setToolTip('Salesgent Hardware Bridge');

  const rebuild = () => {
    const s = getState();
    const dot = s.status === 'running' ? '🟢' : s.status === 'error' ? '🔴' : '⚪️';
    tray.setContextMenu(
      Menu.buildFromTemplate([
        { label: `Salesgent Hardware Bridge — ${dot} ${s.status}`, enabled: false },
        { label: `localhost:${s.port}`, enabled: false },
        { type: 'separator' },
        { label: 'Open window', click: onShow },
        { label: 'Restart bridge', click: onRestart },
        { type: 'separator' },
        { label: 'Quit', click: onQuit },
      ]),
    );
  };

  rebuild();
  tray.on('click', onShow);
  return { tray, refresh: rebuild };
}

module.exports = { createTray };
