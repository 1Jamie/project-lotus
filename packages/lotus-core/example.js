const { ServoWindow, ipcMain, app } = require('./lotus.js');
const path = require('path');

console.log('[Lotus] Initializing runtime...');

// 1. Initialize encrypted VFS if built with --encrypt, then pre-warm Servo
app.initVfs();
app.warmup();

// 2. Create a frameless, transparent window pointing at the test_app UI
const win = new ServoWindow({
    id: 'example-window',
    root: path.join(__dirname, 'test_app', 'ui'),
    index: 'index.html',
    width: 1024,
    height: 768,
    title: 'Lotus Example',
    frameless: true,
    transparent: true,
    visible: false
});

// 3. Reveal the window only once the first frame has painted (zero white flash)
win.once('ready-to-show', () => {
    console.log('[Lotus] First frame ready — showing window.');
    win.show();
});

// 4. Handle promise-based IPC calls from the renderer (`await window.lotus.invoke('ping', ...)`)
ipcMain.handle('ping', async ({ msg }) => {
    return { pong: true, echo: msg, timestamp: Date.now(), pid: process.pid };
});
