const { ServoWindow, ipcMain, app } = require('@lotus-gui/core');
const path = require('path');
const crypto = require('crypto');

console.log('[Secure App] Initializing Lotus Secure Showcase...');
app.warmup();

const UI_DIR = path.join(__dirname, 'ui');

// 1. Register Track 2 Secure Methods
// Channel "vault" is in the surface's allowed channels
ipcMain.register('vault.getSecret', async () => {
    console.log('[Host IPC] Servicing Track 2 "vault.getSecret" request...');
    return {
        secret: 'lotus_sec_' + crypto.randomBytes(16).toString('hex'),
        timestamp: Date.now()
    };
}, {
    channel: 'vault',
    schema: {
        secret: 'string',
        timestamp: 'number'
    }
});

ipcMain.register('vault.signMessage', async (payload) => {
    console.log(`[Host IPC] Signing message: "${payload?.message}"`);
    const hmac = crypto.createHmac('sha256', 'lotus-secure-signing-key');
    hmac.update(payload?.message || '');
    return {
        signature: hmac.digest('hex'),
        algorithm: 'HMAC-SHA256'
    };
}, {
    channel: 'vault',
    schema: {
        signature: 'string',
        algorithm: 'string'
    }
});

// Channel "admin" is intentionally NOT granted to the surface, demonstrating security gating
ipcMain.register('admin.purge', async () => {
    console.log('[Host IPC] CRITICAL: Unauthorized admin.purge invoked!');
    return { purged: true };
}, {
    channel: 'admin'
});

// Register Track 2 Outbound Event
ipcMain.registerEvent('vault.securityAlert', {
    schema: {
        level: 'string',
        message: 'string',
        timestamp: 'number'
    }
});

// 2. Launch Servo Window
const win = new ServoWindow({
    id: 'secure-showcase-window',
    root: UI_DIR,
    index: 'index.html',
    width: 1040,
    height: 760,
    title: 'Lotus Process-Isolated Sandbox & Track 2 Showcase',
    secure: true,
    allowedChannels: ['vault', 'telemetry'],
    visible: false
});

win.on('ready', () => {
    console.log('[Secure App] Servo Window ready.');
});

win.once('ready-to-show', () => {
    console.log('[Secure App] First frame rendered - displaying window.');
    win.show();
});

win.on('load-status', (status, paneId) => {
    if (paneId && paneId !== 'main') return;
    console.log(`[Secure App] Load status: ${status}`);
    if (status === 'complete') {
        setTimeout(() => {
            console.log('[Secure App] Emitting outbound host event "vault.securityAlert"...');
            win.sendToRenderer('vault.securityAlert', {
                level: 'INFO',
                message: 'Stage-B seccomp policy verified active in engine process.',
                timestamp: Date.now()
            });
        }, 1500);
    }
});

