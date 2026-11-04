const { ServoWindow, LayoutBuilder, ipcMain, app } = require('@lotus-gui/core');
const path = require('path');
const http = require('http');
const fs = require('fs');
const crypto = require('crypto');

console.log('[Secure App] Initializing Lotus Dual-Track & Sandboxed Pane Showcase...');
app.warmup();

const UI_DIR = path.join(__dirname, 'ui');

// -----------------------------------------------------------------------------
// 1. Local HTTP Server for Sandboxed Pane Content
// -----------------------------------------------------------------------------
// Because the sandboxed engine runs in CLONE_NEWNET and connects through the
// host's NetworkBroker proxy, serving sandboxed.html over HTTP exercises the real
// NetworkBroker proxy path, anti-SSRF IP classification, and Track 2 /ipc/handshake
// & /ipc/secure-ws WebSocket tunneling.
const sandboxedServer = http.createServer((req, res) => {
    const parsedUrl = new URL(req.url, `http://${req.headers.host || '127.0.0.1'}`);
    let reqPath = parsedUrl.pathname;
    if (reqPath === '/' || reqPath === '/sandboxed' || reqPath === '/sandboxed.html') {
        const filePath = path.join(UI_DIR, 'sandboxed.html');
        if (fs.existsSync(filePath)) {
            const content = fs.readFileSync(filePath);
            res.writeHead(200, {
                'Content-Type': 'text/html; charset=utf-8',
                'Content-Length': content.length,
                'Cache-Control': 'no-cache, no-store',
                'X-Content-Type-Options': 'nosniff',
            });
            res.end(content);
            return;
        }
    }
    res.writeHead(404, { 'Content-Type': 'text/plain' });
    res.end('Not found');
});

async function startShowcase() {
    await new Promise((resolve) => sandboxedServer.listen(0, '127.0.0.1', resolve));
    const sandboxedPort = sandboxedServer.address().port;
    const sandboxedOrigin = `http://127.0.0.1:${sandboxedPort}`;
    const sandboxedUrl = `${sandboxedOrigin}/sandboxed.html`;

    console.log(`[Secure App] Sandboxed HTTP asset server listening on ${sandboxedUrl}`);

    let activeWindow = null;
    let taskDispatchedCounter = 0;

    // -------------------------------------------------------------------------
    // 2. Track 1 IPC Handlers (Trusted Pane / In-Process Chrome)
    // -------------------------------------------------------------------------
    // Trusted in-process panes run with full Node.js OS access.
    ipcMain.handle('system.getHostMetrics', async () => {
        const mem = process.memoryUsage();
        return {
            pid: process.pid,
            platform: `${process.platform} (${process.arch})`,
            nodeVersion: process.version,
            rssMb: (mem.rss / 1024 / 1024).toFixed(1),
            heapUsedMb: (mem.heapUsed / 1024 / 1024).toFixed(1),
            heapTotalMb: (mem.heapTotal / 1024 / 1024).toFixed(1),
            uptimeSec: Math.floor(process.uptime()),
            timestamp: Date.now(),
        };
    });

    ipcMain.handle('system.readHostFile', async () => {
        const configPath = path.join(__dirname, 'lotus.config.json');
        try {
            const content = fs.readFileSync(configPath, 'utf8');
            const parsed = JSON.parse(content);
            return {
                success: true,
                path: configPath,
                name: parsed.name,
                version: parsed.version,
                description: parsed.description,
            };
        } catch (err) {
            return { success: false, error: err.message };
        }
    });

    // Cross-Pane Task Orchestration: Trusted Pane -> Host -> Sandboxed Pane
    ipcMain.handle('orchestrator.dispatchTask', async (params) => {
        taskDispatchedCounter++;
        const taskId = `task-${taskDispatchedCounter}`;
        const taskPayload = {
            taskId,
            title: params?.title || 'Data Transformation',
            inputData: params?.inputData || 'Lotus confidential data stream',
            dispatchedAt: Date.now(),
        };

        console.log(`[Host Orchestrator] Dispatching ${taskId} from Trusted Pane to Sandboxed Pane...`);

        // Forward to the sandboxed pane over the WebSocket bridge
        if (activeWindow) {
            activeWindow.sendToPaneRenderer('sandboxed', 'task.execute', taskPayload);
        }

        return {
            taskId,
            dispatched: true,
            timestamp: Date.now(),
        };
    });

    ipcMain.handle('orchestrator.broadcastEvent', async (params) => {
        const alert = {
            level: params?.level || 'INFO',
            message: params?.message || 'Host-wide audit and invariant check completed.',
            timestamp: Date.now(),
        };

        if (activeWindow) {
            activeWindow.sendToPaneRenderer('trusted', 'system.securityAlert', alert);
            activeWindow.sendToPaneRenderer('sandboxed', 'system.securityAlert', alert);
        }

        return { broadcasted: true, timestamp: alert.timestamp };
    });

    // -------------------------------------------------------------------------
    // 3. Track 2 IPC Methods (Sandboxed Pane / Capability-Gated)
    // -------------------------------------------------------------------------
    // Channel "vault" is in the surface's allowedChannels set
    ipcMain.register('vault.getSecret', async () => {
        console.log('[Host IPC] Servicing Track 2 "vault.getSecret" request...');
        return {
            secret: 'lotus_sec_' + crypto.randomBytes(16).toString('hex'),
            timestamp: Date.now(),
        };
    }, {
        channel: 'vault',
        schema: {
            secret: 'string',
            timestamp: 'number',
        },
    });

    ipcMain.register('vault.signPayload', async (payload) => {
        const message = payload?.message || '';
        console.log(`[Host IPC] Signing payload for sandboxed pane: "${message.slice(0, 40)}..."`);
        const hmac = crypto.createHmac('sha256', 'lotus-host-master-key');
        hmac.update(message);
        return {
            signature: hmac.digest('hex'),
            algorithm: 'HMAC-SHA256',
        };
    }, {
        channel: 'vault',
        schema: {
            signature: 'string',
            algorithm: 'string',
        },
    });

    // Channel "sandboxTasks" receives results from the Sandboxed Pane and relays them to Trusted Pane
    ipcMain.register('sandboxTasks.submitResult', async (payload) => {
        console.log(`[Host IPC] Received completed task ${payload?.taskId} from Sandboxed Engine!`);

        // Notify the Trusted Pane that the sandboxed worker completed the computation
        if (activeWindow) {
            activeWindow.sendToPaneRenderer('trusted', 'task.completed', {
                taskId: payload?.taskId,
                result: payload?.result,
                hash: payload?.hash,
                completedAt: Date.now(),
            });
        }

        return {
            accepted: true,
            taskId: payload?.taskId,
            receivedAt: Date.now(),
        };
    }, {
        channel: 'sandboxTasks',
        schema: {
            accepted: 'boolean',
            taskId: 'string',
            receivedAt: 'number',
        },
    });

    // Channel "admin" is intentionally NOT granted to the surface to prove capability gating
    ipcMain.register('admin.killProcess', async () => {
        console.log('[Host IPC] CRITICAL VIOLATION: Unauthorized admin.killProcess invoked!');
        return { killed: true };
    }, {
        channel: 'admin',
    });

    // -------------------------------------------------------------------------
    // 4. Multi-Pane Declarative Layout with LayoutBuilder
    // -------------------------------------------------------------------------
    // Left Pane: Trusted In-Process Host Deck (lotus-resource://localhost/trusted.html)
    // Fill Pane: Sandboxed Remote Engine (http://127.0.0.1:<port>/sandboxed.html, sandbox: true)
    const layout = new LayoutBuilder()
        .left('trusted', 580, {
            url: 'lotus-resource://localhost/trusted.html',
            sandbox: false,
        })
        .fill('sandboxed', {
            url: sandboxedUrl,
            sandbox: true,
            allowedChannels: ['vault', 'sandboxTasks'],
            trustedOrigins: [sandboxedOrigin],
        });

    console.log('[Secure App] Spawning ServoWindow with dual-track panes...');
    const win = new ServoWindow({
        id: 'secure-showcase-window',
        root: UI_DIR,
        width: 1220,
        height: 820,
        title: 'Lotus Dual-Track Architecture & Sandboxed Pane Showcase',
        allowLocalhost: true,
        autoResizeMain: false,
        visible: false,
        ...layout.config(),
    });

    activeWindow = win;

    win.once('ready-to-show', () => {
        console.log('[Secure App] First frame painted - showing window.');
        win.show();
    });

    win.on('load-status', (status, paneId) => {
        console.log(`[Secure App] Pane "${paneId}" load status: ${status}`);
    });

    win.on('close', () => {
        console.log('[Secure App] Closing showcase window...');
        sandboxedServer.close();
        process.exit(0);
    });
}

startShowcase().catch((err) => {
    console.error('[Secure App] Fatal initialization error:', err);
    process.exit(1);
});
