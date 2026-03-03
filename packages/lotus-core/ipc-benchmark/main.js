const { ServoWindow, ipcMain, app } = require('@lotus-gui/core');
const path = require('path');
const os = require('os');
const v8 = require('v8');

// Pre-warm backend
console.log("[Benchmark] Warming up backend...");
app.warmup();

const UI_DIR = path.join(__dirname, 'ui');

console.log("[Benchmark] Launching IPC Benchmark Window...");

const win = new ServoWindow({
    id: 'benchmark-window',
    root: UI_DIR,
    index: 'index.html',
    width: 1000,
    height: 750,
    title: "Lotus IPC Benchmark",
    frameless: true,
    transparent: true,
    visible: true
});

win.on('ready', () => {
    console.log("[Benchmark] Window ready");
});

win.on('load-status', (status) => {
    console.log(`[Benchmark] Load status: ${status}`);
});

// ─── Benchmark Core: Echo Handler ───────────────────────────────────────────
// The renderer sends benchmark-ping and we echo back the same data immediately.
ipcMain.on('benchmark-ping', (data) => {
    ipcMain.send('benchmark-pong', data);
});

// ─── Latency Test: Round-trip timing ────────────────────────────────────────
// Renderer sends a single message and measures the round-trip time precisely.
ipcMain.on('latency-ping', (data) => {
    ipcMain.send('latency-pong', { id: data.id, serverTs: Date.now() });
});

// ─── System Info Test ────────────────────────────────────────────────────────
ipcMain.on('get-system-info', (data) => {
    const memUsage = process.memoryUsage();
    const heapStats = v8.getHeapStatistics();
    ipcMain.send('system-info-response', {
        requestId: data ? data.requestId : null,
        platform: process.platform,
        arch: process.arch,
        nodeVersion: process.version,
        pid: process.pid,
        uptime: Math.round(process.uptime()),
        cpus: os.cpus().length,
        cpuModel: os.cpus()[0]?.model || 'Unknown',
        totalMem: Math.round(os.totalmem() / 1024 / 1024),
        freeMem: Math.round(os.freemem() / 1024 / 1024),
        heapUsed: Math.round(memUsage.heapUsed / 1024 / 1024),
        heapTotal: Math.round(memUsage.heapTotal / 1024 / 1024),
        rss: Math.round(memUsage.rss / 1024 / 1024),
        v8HeapSizeLimit: Math.round(heapStats.heap_size_limit / 1024 / 1024),
        loadAvg: os.loadavg().map(v => v.toFixed(2)),
        hostname: os.hostname(),
        timestamp: Date.now()
    });
});

// ─── Stress Test: Large payload echo ─────────────────────────────────────────
// Used for torture tests -- generates a large response server-side.
ipcMain.on('stress-request', (data) => {
    const size = Math.min(data.size || 1024, 10 * 1024 * 1024); // cap at 10MB
    const responseData = {
        id: data.id,
        serverTs: Date.now(),
        size: size,
        payload: 'X'.repeat(size)
    };
    ipcMain.send('stress-response', responseData);
});

// ─── JSON Echo: Complex object round-trip ────────────────────────────────────
ipcMain.on('json-echo', (data) => {
    ipcMain.send('json-echo-response', data);
});

// ─── Burst Test: Fire many in a row, count them on the way back ──────────────
let burstReceived = 0;
let burstTarget = 0;
let burstStartTs = 0;

ipcMain.on('burst-start', (data) => {
    burstReceived = 0;
    burstTarget = data.count || 100;
    burstStartTs = Date.now();
    console.log(`[Benchmark] Burst test started: expecting ${burstTarget} messages`);
});

ipcMain.on('burst-ping', (data) => {
    burstReceived++;
    ipcMain.send('burst-pong', { id: data.id, seq: burstReceived });
    if (burstReceived === burstTarget) {
        ipcMain.send('burst-complete', {
            received: burstReceived,
            elapsed: Date.now() - burstStartTs
        });
    }
});

// ─── Calculation Test ────────────────────────────────────────────────────────
ipcMain.on('calculate', (data) => {
    const { operation, a, b } = data;
    let result;
    switch (operation) {
        case 'add': result = a + b; break;
        case 'subtract': result = a - b; break;
        case 'multiply': result = a * b; break;
        case 'divide': result = b !== 0 ? a / b : Infinity; break;
        case 'power': result = Math.pow(a, b); break;
        case 'sqrt': result = Math.sqrt(a); break;
        case 'fib': {
            // Iterative fibonacci -- CPU stress test
            let x = 0, y = 1;
            for (let i = 0; i < Math.min(a, 50); i++) { [x, y] = [y, x + y]; }
            result = x;
            break;
        }
        default: result = null;
    }
    ipcMain.send('calculation-response', {
        operation, a, b, result,
        processedBy: 'Node.js Backend',
        timestamp: Date.now()
    });
});

// ─── Window Controls ─────────────────────────────────────────────────────────
let isMaximized = false;
ipcMain.on('window-control', (action) => {
    switch (action) {
        case 'minimize':
            win.minimize();
            break;
        case 'maximize':
            if (isMaximized) {
                win.unmaximize();
                isMaximized = false;
            } else {
                win.maximize();
                isMaximized = true;
            }
            break;
        case 'close':
            win.close();
            setTimeout(() => process.exit(0), 100);
            break;
    }
});

console.log('[Benchmark] All IPC handlers registered and ready.');
