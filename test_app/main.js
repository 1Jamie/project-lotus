const { ServoWindow, ipcMain, app } = require('../lotus.js');
const path = require('path');

// Pre-warm backend
console.log("[App] Warming up backend...");
app.warmup();

const UI_DIR = path.join(__dirname, 'ui');

// Hybrid Lotus Mode: No Node.js HTTP Server!
console.log("[App] Launching Servo Window in Hybrid Mode...");

// We point Lotus directly at the UI directory
const win = new ServoWindow({
    root: UI_DIR,
    index: 'index.html',
    width: 1024,
    height: 768,
    title: "Hybrid Lotus App"
});

win.on('ready', () => {
    console.log("[App] ServoWindow reported READY");
});

win.on('load-status', (status) => {
    console.log(`[App] Load status changed to: ${status}`);
    if (status === 'complete') {
        setTimeout(() => {
            console.log("[App] Testing executeScript...");
            win.executeScript(`
                const target = document.getElementById('script-target');
                if (target) {
                    target.style.background = 'rgba(0, 255, 0, 0.3)';
                    target.innerHTML = '✅ <strong>Script Executed Successfully!</strong><br>Main process changed this content.';
                }
            `);
        }, 2000);
    }
});

// Set up IPC handlers
ipcMain.on('get-system-info', (data) => {
    console.log('[ipcMain] Received system info request:', data);

    // Backend processing: gather system information
    const systemInfo = {
        platform: process.platform,
        nodeVersion: process.version,
        architecture: process.arch,
        processId: process.pid,
        uptime: Math.round(process.uptime()),
        memoryUsage: Math.round(process.memoryUsage().heapUsed / 1024 / 1024),
        timestamp: new Date().toISOString(),
        requestId: data.requestId
    };

    // Simulate some processing time
    setTimeout(() => {
        console.log('[ipcMain] Sending system info back to renderer');
        ipcMain.send('system-info-response', systemInfo);
    }, 500);
});

ipcMain.on('calculate', (data) => {
    console.log('[ipcMain] Received calculation request:', data);

    // Backend processing: perform calculation
    const { operation, a, b } = data;
    let result;

    switch (operation) {
        case 'add': result = a + b; break;
        case 'multiply': result = a * b; break;
        case 'power': result = Math.pow(a, b); break;
        default: result = 'Unknown operation';
    }

    ipcMain.send('calculation-response', {
        operation,
        a,
        b,
        result,
        processedBy: 'Node.js Backend',
        timestamp: Date.now()
    });
});

ipcMain.on('open-secondary-window', () => {
    console.log('[ipcMain] Opening secondary window...');
    const win2 = new ServoWindow({
        root: UI_DIR,
        index: 'secondary.html',
        width: 600,
        height: 400,
        title: "Secondary Window"
    });
});

console.log('[App] Servo window initialized');
