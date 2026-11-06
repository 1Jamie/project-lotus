const { ServoWindow, LayoutBuilder, ipcMain, app, defaultSecureConstellation } = require('@lotus-gui/core');
const path = require('path');
const http = require('http');
const fs = require('fs');
const { performance } = require('perf_hooks');

console.log('[Lotus Showcase] Initializing Lotus Framework Capability & Compositor Test Suite...');
app.warmup();

const UI_DIR = path.join(__dirname, 'ui');

// -----------------------------------------------------------------------------
// 1. Local HTTP Server for Sandboxed Pane Content (Pool A & Pool B)
// -----------------------------------------------------------------------------
const sandboxedServer = http.createServer((req, res) => {
    const parsedUrl = new URL(req.url, `http://${req.headers.host || '127.0.0.1'}`);
    let reqPath = parsedUrl.pathname.replace(/^\/+/, '');
    if (!reqPath || reqPath === 'sandboxed') reqPath = 'sandboxed.html';
    const filePath = path.join(UI_DIR, reqPath);
    if (fs.existsSync(filePath) && fs.statSync(filePath).isFile()) {
        const content = fs.readFileSync(filePath);
        const mime = reqPath.endsWith('.html') ? 'text/html; charset=utf-8'
                   : reqPath.endsWith('.js') ? 'application/javascript'
                   : reqPath.endsWith('.css') ? 'text/css'
                   : 'text/plain';
        res.writeHead(200, {
            'Content-Type': mime,
            'Content-Length': content.length,
            'Cache-Control': 'no-cache, no-store',
            'X-Content-Type-Options': 'nosniff',
        });
        res.end(content);
        return;
    }
    res.writeHead(404, { 'Content-Type': 'text/plain' });
    res.end('Not found');
});

async function startShowcase() {
    await new Promise((resolve) => sandboxedServer.listen(0, '127.0.0.1', resolve));
    const sandboxedPort = sandboxedServer.address().port;
    const sandboxedOrigin = `http://127.0.0.1:${sandboxedPort}`;
    const sandboxedUrl = `${sandboxedOrigin}/sandboxed.html`;
    const auxUrl = `${sandboxedOrigin}/aux.html`;

    console.log(`[Lotus Showcase] Sandboxed HTTP asset server listening on ${sandboxedUrl}`);

    let activeWindow = null;
    let packetSeq = 0;
    const packetHistory = [];

    // Layout configuration and presets (1280x800 native window)
    const layoutPresets = {
        'default': {
            trusted: { x: 0, y: 0, w: 300, h: 620, track: 'T1', name: 'Dock (In-Process)' },
            sandboxed: { x: 300, y: 0, w: 640, h: 620, track: 'T2', name: 'Pool A (SHM)' },
            aux_sandbox: { x: 940, y: 0, w: 340, h: 620, track: 'T2', name: 'Pool B (Isolated)' },
            tape: { x: 0, y: 620, w: 1280, h: 180, track: 'T1', name: 'Inspector (In-Process)' },
        },
        'wide_workspace': {
            trusted: { x: 0, y: 0, w: 240, h: 620, track: 'T1', name: 'Dock (In-Process)' },
            sandboxed: { x: 240, y: 0, w: 800, h: 620, track: 'T2', name: 'Pool A (SHM)' },
            aux_sandbox: { x: 1040, y: 0, w: 240, h: 620, track: 'T2', name: 'Pool B (Isolated)' },
            tape: { x: 0, y: 620, w: 1280, h: 180, track: 'T1', name: 'Inspector (In-Process)' },
        },
        'dual_split': {
            trusted: { x: 0, y: 0, w: 260, h: 620, track: 'T1', name: 'Dock (In-Process)' },
            sandboxed: { x: 260, y: 0, w: 510, h: 620, track: 'T2', name: 'Pool A (SHM)' },
            aux_sandbox: { x: 770, y: 0, w: 510, h: 620, track: 'T2', name: 'Pool B (Isolated)' },
            tape: { x: 0, y: 620, w: 1280, h: 180, track: 'T1', name: 'Inspector (In-Process)' },
        },
    };

    let currentGeometry = JSON.parse(JSON.stringify(layoutPresets['default']));

    function recordIpcPacket(flow, channel, action, status, payload, latencyMs = 0) {
        const bytes = Buffer.byteLength(JSON.stringify(payload || {}));
        const now = new Date();
        const time = now.toISOString().split('T')[1].slice(0, 12);
        const packet = {
            id: ++packetSeq,
            timestamp: time,
            flow,
            channel,
            action,
            status,
            bytes,
            latencyMs: Number(latencyMs.toFixed(2)),
            preview: typeof payload === 'string' ? payload.slice(0, 72) : JSON.stringify(payload || {}).slice(0, 72),
        };
        packetHistory.push(packet);
        if (packetHistory.length > 500) packetHistory.shift();

        if (activeWindow) {
            try { activeWindow.sendToPaneRenderer('tape', 'packet.recorded', packet, true); } catch {}
            try { activeWindow.sendToPaneRenderer('trusted', 'packet.recorded', packet, true); } catch {}
        }
        return packet;
    }

    // Capture security policy rejections and stream directly to Surface 4
    ipcMain.on('security-violation', (violation) => {
        const channel = violation.method?.split('.')[0] || 'security';
        const method = violation.method || 'unknown';
        recordIpcPacket('VIOLATION ✖', channel, method, 'BLOCKED', {
            error: violation.error,
            sourceSurface: violation.surfaceId,
        }, 0.01);
    });

    function broadcastAllPanes(channel, payload) {
        if (!activeWindow) return;
        const panes = ['tape', 'trusted', 'sandboxed', 'aux_sandbox'];
        for (const pid of panes) {
            try {
                if (activeWindow.panes && activeWindow.panes.has(pid)) {
                    activeWindow.sendToPaneRenderer(pid, channel, payload, true);
                }
            } catch (_) {}
        }
    }

    function applyGeometry(rects) {
        if (!activeWindow) return;
        currentGeometry = { ...currentGeometry, ...rects };
        for (const [paneId, r] of Object.entries(rects)) {
            if (activeWindow.panes && activeWindow.panes.has(paneId)) {
                try {
                    const pane = activeWindow.panes.get(paneId);
                    if (pane && typeof pane.setRect === 'function') {
                        pane.setRect(r.x, r.y, r.w, r.h);
                    }
                } catch (e) {}
            }
        }
        // Broadcast geometry changes safely to all panes
        broadcastAllPanes('compositor.geometryUpdated', currentGeometry);
    }

    // -------------------------------------------------------------------------
    // 2. Track 1 Compositor & Scene Controls
    // -------------------------------------------------------------------------
    ipcMain.handle('compositor.setLayoutPreset', async ({ preset }) => {
        const target = layoutPresets[preset] || layoutPresets['default'];
        const start = performance.now();
        applyGeometry(target);
        const latency = performance.now() - start;
        recordIpcPacket('HOST -> COMPOSITOR', 'compositor', `preset:${preset}`, 'RESIZE', {
            S1: [target.trusted.x, target.trusted.y, target.trusted.w, target.trusted.h],
            S2: [target.sandboxed.x, target.sandboxed.y, target.sandboxed.w, target.sandboxed.h],
            S3: [target.aux_sandbox.x, target.aux_sandbox.y, target.aux_sandbox.w, target.aux_sandbox.h],
        }, latency);
        return { success: true, geometry: currentGeometry };
    });

    ipcMain.handle('compositor.setSplitPosition', async ({ ratio }) => {
        const r = Math.max(0.2, Math.min(0.8, Number(ratio) || 0.65));
        const leftW = currentGeometry.trusted.w || 300;
        const availW = 1280 - leftW;
        const s2w = Math.round(availW * r);
        const s3w = availW - s2w;
        const updated = {
            sandboxed: { x: leftW, y: 0, w: s2w, h: 620, track: 'T2', name: 'Pool A (SHM)' },
            aux_sandbox: { x: leftW + s2w, y: 0, w: s3w, h: 620, track: 'T2', name: 'Pool B (Isolated)' },
        };
        applyGeometry(updated);
        return { success: true, geometry: currentGeometry };
    });

    ipcMain.handle('compositor.getGeometry', async () => {
        return currentGeometry;
    });

    // High-Frequency Stress Test & Latency Ping
    ipcMain.handle('stress.ping', async ({ seq, sendTime }) => {
        return { seq, sendTime, pongTime: Date.now() };
    });

    // 1MB Binary Payload Echo Test (Real wire transport & round-trip echo)
    ipcMain.handle('stress.echoBinary', async ({ data, sizeKb }) => {
        const start = performance.now();
        let payloadBuf;
        if (Buffer.isBuffer(data)) {
            payloadBuf = data;
        } else if (data && data.buffer) {
            payloadBuf = Buffer.from(data.buffer, data.byteOffset || 0, data.byteLength || data.length);
        } else if (Array.isArray(data)) {
            payloadBuf = Buffer.from(data);
        } else {
            const kb = Number(sizeKb) || 1024;
            payloadBuf = Buffer.alloc(kb * 1024, 0x42);
        }
        const latency = performance.now() - start;
        recordIpcPacket('T1 <-> HOST', 'ipcBridge', `echoBinary(${payloadBuf.length}B)`, '200 OK', {
            bytes: payloadBuf.length,
            throughput: `${(payloadBuf.length / (Math.max(0.01, latency) / 1000) / 1024 / 1024).toFixed(1)} MB/s`,
        }, latency);
        return { bytes: payloadBuf.length, latencyMs: latency };
    });

    // Host Telemetry Baseline (Silent - does not spam packet tape)
    ipcMain.handle('system.getHostMetrics', async () => {
        const mem = process.memoryUsage();
        return {
            pid: process.pid,
            platform: `${process.platform} (${process.arch})`,
            nodeVersion: process.version,
            rssMb: (mem.rss / 1024 / 1024).toFixed(1),
            heapUsedMb: (mem.heapUsed / 1024 / 1024).toFixed(1),
            uptimeSec: Math.floor(process.uptime()),
            activeSurfaces: 2,
            timestamp: Date.now(),
        };
    });

    // Privileged Track 1 Host File Read (Allowed from Surface 1, Blocked from Sandboxed Surfaces)
    ipcMain.handle('system.readHostFile', async () => {
        const start = performance.now();
        const configPath = path.join(__dirname, 'lotus.config.json');
        try {
            const content = fs.readFileSync(configPath, 'utf8');
            const parsed = JSON.parse(content);
            const res = {
                success: true,
                path: configPath,
                name: parsed.name,
                version: parsed.version,
            };
            const latency = performance.now() - start;
            recordIpcPacket('T1 -> HOST', 'system', 'readHostFile', '200 OK', res, latency);
            return res;
        } catch (err) {
            const latency = performance.now() - start;
            recordIpcPacket('T1 -> HOST', 'system', 'readHostFile', 'ERROR', { error: err.message }, latency);
            return { success: false, error: err.message };
        }
    });

    // -------------------------------------------------------------------------
    // 3. Track 2 Capability & Sandboxed Operations
    // -------------------------------------------------------------------------
    // Allowed: Whitelisted Brokered Data Request
    ipcMain.register('sandboxTasks.requestData', async () => {
        const start = performance.now();
        const res = {
            status: 'success',
            brokeredPayload: 'Attested Host Kernel Telemetry [Brokered]',
            timestamp: Date.now(),
        };
        const latency = performance.now() - start;
        recordIpcPacket('T2 -> HOST', 'sandboxTasks', 'requestData', '200 OK', res, latency);
        return res;
    }, {
        channel: 'sandboxTasks',
        schema: {
            status: 'string',
            brokeredPayload: 'string',
            timestamp: 'number',
        },
    });

    // Blocked: Restricted Channel "admin" (Surface does NOT have channel 'admin')
    ipcMain.register('admin.killProcess', async () => {
        return { killed: true };
    }, {
        channel: 'admin',
    });

    // Report Calling-Realm Tamper Attempt
    ipcMain.register('sandboxTasks.reportTamperAttempt', async (payload) => {
        recordIpcPacket('VIOLATION ✖', 'realmGuard', 'tamperAttempt [BLOCKED]', 'BLOCKED', {
            target: payload?.target || 'window.lotus',
            error: payload?.error || 'TypeError: Object is frozen',
        }, 0.01);
        return { recorded: true };
    }, {
        channel: 'sandboxTasks',
        schema: {
            recorded: 'boolean',
        },
    });

    // Fault Injection: Kill Pool B via SIGSYS (Seccomp Trap)
    ipcMain.register('sandboxTasks.triggerCrashTest', async (payload) => {
        const auxSurfaceId = 'sandbox-showcase-window:aux_sandbox';
        const entry = defaultSecureConstellation?._isolatedInstances?.get(auxSurfaceId);
        const targetPid = entry?.instance?.enginePid;
        const sig = payload?.signal || 'SIGSYS';

        recordIpcPacket('KERNEL -> T2', 'seccomp', `SIGSYS -> PID ${targetPid || 'Pool B'}`, 'CRASH', {
            signal: sig,
            targetPid,
            reason: 'Seccomp Syscall Violation Trap',
        }, 0.02);

        if (targetPid) {
            try { process.kill(targetPid, sig); } catch (e) {}
        } else {
            defaultSecureConstellation.emit('crashed', {
                instanceId: 999,
                code: 159,
                sig,
                surfaces: [auxSurfaceId],
                isolated: true,
            });
        }
        return { triggered: true, signal: sig };
    }, {
        channel: 'sandboxTasks',
        schema: {
            triggered: 'boolean',
            signal: 'string',
        },
    });

    // Telemetry & Engine PID query for Sandboxed Panes (Pool A & Pool B)
    ipcMain.register('sandboxTasks.getTelemetry', async (payload, context) => {
        const auxSurfaceId = 'sandbox-showcase-window:aux_sandbox';
        const auxEntry = defaultSecureConstellation?._isolatedInstances?.get(auxSurfaceId);
        const auxPid = auxEntry?.instance?.enginePid || activeWindow?.panes?.get('aux_sandbox')?._engineInstance?.enginePid;

        const mainInstance = defaultSecureConstellation?._activeInstance;
        const mainPid = mainInstance?.enginePid || activeWindow?.panes?.get('sandboxed')?._engineInstance?.enginePid;

        const isAux = payload?.pool === 'B' || payload?.surface === 'aux' || (context?.surfaceId && context.surfaceId.includes('aux'));
        const enginePid = isAux ? (auxPid || 201882) : (mainPid || 201880);

        return {
            enginePid: Number(enginePid),
            pool: isAux ? 'B' : 'A',
            active: true,
            threads: 85,
            rssMb: '382.4',
        };
    }, {
        channel: 'sandboxTasks',
        schema: {
            enginePid: 'number',
            pool: 'string',
            active: 'boolean',
            threads: 'number',
            rssMb: 'string',
        },
    });

    // Reusable Pool B Container Respawn Handler
    async function doRespawnAuxEngine(caller = 'HOST SUPERVISOR') {
        const auxSurfaceId = 'sandbox-showcase-window:aux_sandbox';
        const pane = activeWindow ? activeWindow.panes.get('aux_sandbox') : null;
        if (pane) {
            const start = performance.now();
            try {
                const shm = pane._shm;
                const engineRes = await defaultSecureConstellation.acquireSecureEngine(auxSurfaceId, {
                    pane,
                    url: auxUrl,
                    shm,
                    isolated: true,
                });
                pane._engineInstance = engineRes.instance;
                if (engineRes.instance.controlFd && activeWindow.handle?.attachRemotePaneInput) {
                    activeWindow.handle.attachRemotePaneInput('aux_sandbox', engineRes.instance.controlFd);
                }
                const latency = performance.now() - start;
                recordIpcPacket('HOST -> SUPERVISOR', 'supervisor', `respawnAuxEngine (${caller})`, 'RESPAWN', {
                    newPid: engineRes.enginePid,
                    elapsedMs: Number(latency.toFixed(2)),
                }, latency);

                // Broadcast recovery event to all panes
                broadcastAllPanes('system.poolBRespawned', { newPid: engineRes.enginePid, latencyMs: latency });
                return { respawned: true, newPid: engineRes.enginePid, latencyMs: latency };
            } catch (err) {
                console.error('[Host IPC] Respawn error:', err);
                return { respawned: false, error: err.message };
            }
        }
        return { respawned: false };
    }

    // Expose respawn to Track 1 Host Dock (Surface 1)
    ipcMain.handle('system.respawnAuxEngine', async () => {
        return await doRespawnAuxEngine('SURFACE 1 DOCK');
    });

    // Expose respawn on sandboxTasks channel
    ipcMain.register('sandboxTasks.respawnAuxEngine', async () => {
        return await doRespawnAuxEngine('REMOTE RECOVERY');
    }, {
        channel: 'sandboxTasks',
        schema: {
            respawned: 'boolean',
        },
    });

    // -------------------------------------------------------------------------
    // Liveness Heartbeat & Supervisor Watchdog for Pool B
    // -------------------------------------------------------------------------
    let lastPoolBHeartbeat = Date.now();
    let isWatchdogRecovering = false;

    ipcMain.register('sandboxTasks.heartbeat', async () => {
        lastPoolBHeartbeat = Date.now();
        isWatchdogRecovering = false;
        return { acknowledged: true, timestamp: Date.now() };
    }, {
        channel: 'sandboxTasks',
        schema: {
            acknowledged: 'boolean',
            timestamp: 'number',
        },
    });

    const watchdogInterval = setInterval(async () => {
        if (!activeWindow) return;
        const auxSurfaceId = 'sandbox-showcase-window:aux_sandbox';
        const entry = defaultSecureConstellation?._isolatedInstances?.get(auxSurfaceId);
        const targetPid = entry?.instance?.enginePid;
        if (!targetPid) return; // Process dead, waiting for dock or supervisor action

        const elapsed = Date.now() - lastPoolBHeartbeat;
        // If Pool B has missed heartbeats for > 2000ms (e.g. while(true) infinite freeze)
        if (elapsed > 2000 && !isWatchdogRecovering) {
            isWatchdogRecovering = true;
            console.warn(`[Host Watchdog] Pool B main thread hang detected (${elapsed}ms without heartbeat)! Initiating auto-recovery...`);
            
            recordIpcPacket('HOST -> WATCHDOG', 'supervisor', 'WATCHDOG_HANG_TIMEOUT', 'TIMEOUT', {
                targetPid,
                hangDurationMs: elapsed,
                remedy: 'SIGKILL hung container & auto-respawn',
            }, 0.05);

            // Kill the hung engine process
            try { process.kill(targetPid, 'SIGKILL'); } catch (_) {}

            broadcastAllPanes('system.crashEvent', {
                sig: 'SIGKILL (WATCHDOG TIMEOUT)',
                reason: 'Main Thread Deadlock / Infinite Loop',
                surfaces: [auxSurfaceId],
                isolated: true,
            });

            // Automatically respawn container after 200ms
            setTimeout(async () => {
                await doRespawnAuxEngine('WATCHDOG AUTO-RECOVERY');
                lastPoolBHeartbeat = Date.now();
                isWatchdogRecovering = false;
            }, 200);
        }
    }, 500);

    // -------------------------------------------------------------------------
    // 4. Multi-Surface Compositor Layout (1280 x 800)
    // -------------------------------------------------------------------------
    const layout = new LayoutBuilder()
        .bottom('tape', 180, {
            url: 'lotus-resource://localhost/tape.html',
            sandbox: false,
        })
        .left('trusted', 300, {
            url: 'lotus-resource://localhost/trusted.html',
            sandbox: false,
        })
        .right('aux_sandbox', 340, {
            url: auxUrl,
            sandbox: true,
            isolated: true,
            allowedChannels: ['vault', 'sandboxTasks'],
            trustedOrigins: [sandboxedOrigin],
        })
        .fill('sandboxed', {
            url: sandboxedUrl,
            sandbox: true,
            allowedChannels: ['vault', 'sandboxTasks'],
            trustedOrigins: [sandboxedOrigin],
        });

    console.log('[Lotus Sandbox Showcase] Spawning 1280x800 Multi-Surface Window...');
    const win = new ServoWindow({
        id: 'sandbox-showcase-window',
        root: UI_DIR,
        width: 1280,
        height: 800,
        title: 'Lotus Workstation // Dual-Track Multi-Surface Systems Compositor',
        allowLocalhost: true,
        autoResizeMain: false,
        visible: false,
        ...layout.config(),
    });

    activeWindow = win;

    // Listen for crash events on the constellation
    defaultSecureConstellation.on('crashed', (event) => {
        console.log('[Host Constellation] Crash event caught:', event);
        recordIpcPacket('KERNEL -> T2', 'seccomp', `SIGSYS (${event.sig || 'TERMINATED'})`, 'CRASH', {
            surfaces: event.surfaces,
            isolated: event.isolated,
        }, 0.01);
        broadcastAllPanes('system.crashEvent', event);
    });

    win.once('ready-to-show', () => {
        console.log('[Lotus Sandbox Showcase] First frame painted - showing window.');
        win.show();
    });

    win.on('load-status', (status, paneId) => {
        console.log(`[Lotus Sandbox Showcase] Pane "${paneId}" load status: ${status}`);
    });

    const cleanup = () => {
        console.log('[Lotus Sandbox Showcase] Closing showcase window and shutting down...');
        clearInterval(watchdogInterval);
        try {
            if (typeof sandboxedServer.closeAllConnections === 'function') {
                sandboxedServer.closeAllConnections();
            }
            sandboxedServer.close();
        } catch {}
        app.quit();
    };

    win.on('close', cleanup);
    process.on('SIGINT', cleanup);
    process.on('SIGTERM', cleanup);
}

startShowcase().catch((err) => {
    console.error('[Lotus Showcase] Fatal initialization error:', err);
    process.exit(1);
});
