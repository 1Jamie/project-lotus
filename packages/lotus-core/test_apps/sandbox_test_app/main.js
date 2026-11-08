const {
    ServoWindow,
    LayoutBuilder,
    ipcMain,
    app,
    defaultSecureConstellation,
    hostIpcRouter,
    respawnCrashedSurface,
} = require('@lotus-gui/core');
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

    // Pool B watchdog state — declared early so fault-injection / respawn paths can
    // disarm before a successor engine is acquired (avoids stale-clock SIGKILL).
    const AUX_SURFACE_ID = 'sandbox-showcase-window:aux_sandbox';
    let lastPoolBHeartbeat = 0;
    let poolBHeartbeatArmed = false;
    let isWatchdogRecovering = false;
    let showcaseWindowFocused = true;

    function disarmPoolBWatchdog() {
        lastPoolBHeartbeat = 0;
        poolBHeartbeatArmed = false;
        isWatchdogRecovering = false;
    }

    function isPoolBSurface(surfaces) {
        return Array.isArray(surfaces) && surfaces.includes(AUX_SURFACE_ID);
    }

    // Presets drive the layout tree (dock Fixed + split Weights). Geometry for the
    // tape map is derived from the live window size + those inputs — not a hardcoded 1280 grid.
    const TAPE_H = 180;
    const WIN_W = 1280;
    const WIN_H = 800;
    const layoutPresets = {
        default: { trustedW: 300, weights: [0.65, 0.35] },
        wide_workspace: { trustedW: 240, weights: [0.77, 0.23] },
        dual_split: { trustedW: 260, weights: [0.5, 0.5] },
    };

    let poolWeights = layoutPresets.default.weights.slice();
    let trustedWidth = layoutPresets.default.trustedW;

    function windowLogicalSize() {
        const w = Math.max(1, Math.round(activeWindow?._logicalWidth || WIN_W));
        const h = Math.max(1, Math.round(activeWindow?._logicalHeight || WIN_H));
        return { w, h };
    }

    function deriveGeometry() {
        const { w: winW, h: winH } = windowLogicalSize();
        const tapeH = Math.min(TAPE_H, Math.max(1, winH - 1));
        const contentH = Math.max(1, winH - tapeH);
        const trusted = activeWindow?.panes?.get('trusted');
        const tw = Math.max(
            1,
            Math.round(Number(trusted?._layout?.width) || trustedWidth || layoutPresets.default.trustedW)
        );
        trustedWidth = tw;
        const poolW = Math.max(1, winW - tw);
        const [wa, wb] = poolWeights;
        const sum = Math.max(1e-6, (Number(wa) || 0) + (Number(wb) || 0));
        const s2 = Math.max(1, Math.round(poolW * ((Number(wa) || 0) / sum)));
        const s3 = Math.max(1, poolW - s2);
        return {
            trusted: { x: 0, y: 0, w: tw, h: contentH, track: 'T1', name: 'Dock (In-Process)' },
            sandboxed: { x: tw, y: 0, w: s2, h: contentH, track: 'T2', name: 'Pool A (SHM)' },
            aux_sandbox: { x: tw + s2, y: 0, w: s3, h: contentH, track: 'T2', name: 'Pool B (Isolated)' },
            tape: { x: 0, y: contentH, w: winW, h: tapeH, track: 'T1', name: 'Inspector (In-Process)' },
            viewport: { w: winW, h: winH },
            splitRatio: (Number(wa) || 0) / sum,
        };
    }

    let currentGeometry = deriveGeometry();

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

    function publishGeometry(extra = {}) {
        currentGeometry = { ...deriveGeometry(), ...extra };
        broadcastAllPanes('compositor.geometryUpdated', currentGeometry);
        return currentGeometry;
    }

    function applyLayoutPreset(presetKey) {
        const preset = layoutPresets[presetKey] || layoutPresets.default;
        poolWeights = preset.weights.slice();
        trustedWidth = preset.trustedW;
        if (!activeWindow) return publishGeometry();
        const trusted = activeWindow.panes.get('trusted');
        // One Rust recalculate via begin/endLayoutBatch; mutations queue inside.
        activeWindow.batchLayout(() => {
            if (trusted && typeof trusted.setSize === 'function') {
                trusted.setSize(preset.trustedW);
            }
            activeWindow.setSplitWeights('pools', preset.weights);
        });
        return publishGeometry();
    }

    // -------------------------------------------------------------------------
    // 2. Track 1 Compositor & Scene Controls
    // -------------------------------------------------------------------------
    ipcMain.handle('compositor.setLayoutPreset', async ({ preset }) => {
        const start = performance.now();
        const target = applyLayoutPreset(preset);
        const latency = performance.now() - start;
        recordIpcPacket('HOST -> COMPOSITOR', 'compositor', `preset:${preset}`, 'RESIZE', {
            S1: [target.trusted.x, target.trusted.y, target.trusted.w, target.trusted.h],
            S2: [target.sandboxed.x, target.sandboxed.y, target.sandboxed.w, target.sandboxed.h],
            S3: [target.aux_sandbox.x, target.aux_sandbox.y, target.aux_sandbox.w, target.aux_sandbox.h],
        }, latency);
        return {
            success: true,
            geometry: currentGeometry,
            splitRatio: currentGeometry.splitRatio,
        };
    });

    ipcMain.handle('compositor.setSplitPosition', async ({ ratio }) => {
        const r = Math.max(0.2, Math.min(0.8, Number(ratio) || 0.65));
        poolWeights = [r, 1 - r];
        if (activeWindow) {
            activeWindow.setSplitWeights('pools', poolWeights);
        }
        publishGeometry();
        return { success: true, geometry: currentGeometry, splitRatio: r };
    });

    ipcMain.handle('compositor.getGeometry', async () => {
        return publishGeometry();
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

    // Fault Injection: terminate Pool B. Demo signals (SIGSYS/SIGABRT) are labels —
    // those can be blocked/caught inside the engine, so we always SIGKILL to guarantee
    // death; host crash chrome still displays the requested fault name.
    ipcMain.register('sandboxTasks.triggerCrashTest', async (payload) => {
        const auxSurfaceId = AUX_SURFACE_ID;
        const entry = defaultSecureConstellation?._isolatedInstances?.get(auxSurfaceId);
        const instance = entry?.instance;
        const targetPid = instance?.enginePid;
        const supervisorPid = instance?.supervisor?.pid;
        const sig = payload?.signal || 'SIGSYS';
        const pane = activeWindow?.panes?.get('aux_sandbox');
        if (pane) {
            pane._faultInjection = {
                sig,
                reason: sig === 'SIGABRT'
                    ? 'Simulated heap exhaustion / abort'
                    : 'Seccomp syscall violation trap (simulated)',
            };
        }

        // Stop hang detection before the process dies — otherwise a Respawn that
        // lands before the exit handler runs inherits a stale armed clock.
        disarmPoolBWatchdog();

        recordIpcPacket('KERNEL -> T2', 'seccomp', `${sig} -> PID ${targetPid || 'Pool B'}`, 'CRASH', {
            signal: sig,
            targetPid,
            reason: pane?._faultInjection?.reason || 'Fault injection',
        }, 0.02);

        let killed = false;
        if (instance) {
            instance._isTerminated = true;
        }
        if (targetPid) {
            try { process.kill(targetPid, 'SIGKILL'); killed = true; } catch (_) {}
        }
        if (supervisorPid) {
            try { process.kill(supervisorPid, 'SIGKILL'); killed = true; } catch (_) {}
        }
        if (!killed) {
            defaultSecureConstellation.emit('crashed', {
                instanceId: entry?.instanceId || 999,
                code: 137,
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

    // Pool B respawn via framework host crash chrome (dismisses Aw Snap + reacquires engine).
    async function doRespawnAuxEngine(caller = 'HOST SUPERVISOR') {
        const auxSurfaceId = AUX_SURFACE_ID;
        const pane = activeWindow ? activeWindow.panes.get('aux_sandbox') : null;
        if (!pane) return { respawned: false };

        // New engine has no heartbeats yet — do not inherit the previous life's clock.
        disarmPoolBWatchdog();

        const start = performance.now();
        try {
            // Showcase grants may have been wiped by a fail-closed navigation during death.
            const surface = hostIpcRouter?.surfaceTracker?.getSurface(auxSurfaceId);
            if (surface) {
                surface.allowedChannels = new Set(['vault', 'sandboxTasks']);
                surface.trustedOrigins = new Set([sandboxedOrigin]);
            }
            pane._url = auxUrl;

            const engineRes = await respawnCrashedSurface(auxSurfaceId);
            if (!engineRes?.respawned) {
                return { respawned: false, error: engineRes?.error || 'respawn failed' };
            }

            const latency = performance.now() - start;
            recordIpcPacket('HOST -> SUPERVISOR', 'supervisor', `respawnAuxEngine (${caller})`, 'RESPAWN', {
                newPid: engineRes.newPid,
                elapsedMs: Number(latency.toFixed(2)),
            }, latency);

            broadcastAllPanes('system.poolBRespawned', { newPid: engineRes.newPid, latencyMs: latency });
            return { respawned: true, newPid: engineRes.newPid, latencyMs: latency };
        } catch (err) {
            console.error('[Host IPC] Respawn error:', err);
            return { respawned: false, error: err.message };
        }
    }

    // Chrome-button / framework respawn does not go through doRespawnAuxEngine — forward
    // so the Host Dock + tape topology clear their "DEAD / KILLED" chrome.
    ipcMain.on('system.remoteSurfaceRespawned', (info) => {
        if (!info || !String(info.surfaceId || '').includes('aux_sandbox')) return;
        disarmPoolBWatchdog();
        broadcastAllPanes('system.poolBRespawned', {
            newPid: info.newPid,
            latencyMs: 0,
            source: 'crash-chrome',
        });
    });

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
    // Heartbeats are page-driven (setInterval → lotus.invoke). When the host window
    // loses OS focus, Wayland often stops RedrawRequested → remote SHM/GPU drain
    // stalls → sandboxed JS can wedge → heartbeats stop. That is not a hang; do not
    // SIGKILL. Also do not arm until the first successful heartbeat (IPC bring-up).
    //
    // Critical: disarm on every Pool B death / respawn start. If we stay armed across
    // a crash, the next cold-spawn inherits a multi-second stale "elapsed" and the
    // watchdog SIGKILLs the brand-new engine before its first heartbeat can land —
    // which is exactly the "Respawn needs 2–3 clicks" failure mode.
    ipcMain.register('sandboxTasks.heartbeat', async () => {
        lastPoolBHeartbeat = Date.now();
        poolBHeartbeatArmed = true;
        isWatchdogRecovering = false;
        return { acknowledged: true, timestamp: Date.now() };
    }, {
        channel: 'sandboxTasks',
        schema: {
            acknowledged: 'boolean',
            timestamp: 'number',
        },
    });

    // Hang detection: 2s without heartbeat → SIGKILL. Recovery is pane policy
    // (autoRespawn / host crash chrome), not showcase auto-respawn.
    const POOL_B_HANG_MS = 2000;
    const watchdogInterval = setInterval(() => {
        if (!activeWindow) return;
        if (!showcaseWindowFocused) return;
        if (!poolBHeartbeatArmed) return;
        const pane = activeWindow.panes.get('aux_sandbox');
        // Crash chrome / mid-respawn: never judge liveness against a stale clock.
        if (pane?._crashed || pane?._respawning) {
            disarmPoolBWatchdog();
            return;
        }
        const entry = defaultSecureConstellation?._isolatedInstances?.get(AUX_SURFACE_ID);
        const targetPid = entry?.instance?.enginePid;
        const supervisorPid = entry?.instance?.supervisor?.pid;
        if (!targetPid) return; // Already dead — crash chrome / autoRespawn owns recovery

        const elapsed = Date.now() - lastPoolBHeartbeat;
        if (elapsed > POOL_B_HANG_MS && !isWatchdogRecovering) {
            isWatchdogRecovering = true;
            console.warn(`[Host Watchdog] Pool B main thread hang detected (${elapsed}ms without heartbeat)! Killing hung engine...`);

            if (pane) {
                pane._faultInjection = {
                    sig: 'WATCHDOG TIMEOUT',
                    reason: 'Main thread deadlock / infinite loop',
                };
            }

            recordIpcPacket('HOST -> WATCHDOG', 'supervisor', 'WATCHDOG_HANG_TIMEOUT', 'TIMEOUT', {
                targetPid,
                hangDurationMs: elapsed,
                remedy: 'SIGKILL hung container; host crash chrome awaits Respawn',
            }, 0.05);

            // Disarm before kill so a concurrent Respawn cannot be murdered by this clock.
            disarmPoolBWatchdog();

            try { process.kill(targetPid, 'SIGKILL'); } catch (_) {}
            if (supervisorPid) {
                try { process.kill(supervisorPid, 'SIGKILL'); } catch (_) {}
            }

            broadcastAllPanes('system.crashEvent', {
                sig: 'WATCHDOG TIMEOUT',
                reason: 'Main Thread Deadlock / Infinite Loop',
                surfaces: [AUX_SURFACE_ID],
                isolated: true,
            });
        }
    }, 200);

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
            resizable: true,
            min: 180,
        })
        .row('pools', (r) => {
            r.pane('sandboxed', {
                weight: 0.65,
                min: 160,
                resizable: true,
                url: sandboxedUrl,
                sandbox: true,
                autoRespawn: true,
                autoRespawnMaxAttempts: 3,
                allowedChannels: ['vault', 'sandboxTasks'],
                trustedOrigins: [sandboxedOrigin],
            })
             .pane('aux_sandbox', {
                weight: 0.35,
                min: 160,
                resizable: true,
                url: auxUrl,
                sandbox: true,
                isolated: true,
                autoRespawn: false,
                allowedChannels: ['vault', 'sandboxTasks'],
                trustedOrigins: [sandboxedOrigin],
            });
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
        if (isPoolBSurface(event.surfaces)) {
            disarmPoolBWatchdog();
        }
        recordIpcPacket('KERNEL -> T2', 'seccomp', `SIGSYS (${event.sig || 'TERMINATED'})`, 'CRASH', {
            surfaces: event.surfaces,
            isolated: event.isolated,
        }, 0.01);
        broadcastAllPanes('system.crashEvent', event);
    });

    win.once('ready-to-show', () => {
        console.log('[Lotus Sandbox Showcase] First frame painted - showing window.');
        win.show();

        if (process.env.LOTUS_TEST_CRASH_RECOVERY === '1') {
            (async () => {
                console.log('[TEST] Starting multi-cycle crash recovery stress test...');
                await new Promise((r) => setTimeout(r, 1500));
                const auxSurfaceId = AUX_SURFACE_ID;
                let prevPid = defaultSecureConstellation?._isolatedInstances?.get(auxSurfaceId)?.instance?.enginePid;
                console.log('[TEST] Initial Pool B PID:', prevPid);

                for (let cycle = 1; cycle <= 4; cycle++) {
                    console.log(`[TEST] === Cycle ${cycle}: Crashing Pool B (aux_sandbox) ===`);
                    const entry = defaultSecureConstellation?._isolatedInstances?.get(auxSurfaceId);
                    const targetPid = entry?.instance?.enginePid;
                    const supPid = entry?.instance?.supervisor?.pid;
                    console.log(`[TEST] Cycle ${cycle} killing enginePid=${targetPid}, supervisorPid=${supPid}`);
                    if (entry?.instance) entry.instance._isTerminated = true;
                    try { if (targetPid) process.kill(targetPid, 'SIGKILL'); } catch (_) {}
                    try { if (supPid) process.kill(supPid, 'SIGKILL'); } catch (_) {}

                    // Vary delay before respawn: immediate (50ms) vs after crash chrome (600ms)
                    await new Promise((r) => setTimeout(r, cycle % 2 === 1 ? 600 : 50));

                    console.log(`[TEST] Cycle ${cycle}: Requesting respawn...`);
                    const res = cycle % 2 === 1
                        ? await respawnCrashedSurface(auxSurfaceId)
                        : await doRespawnAuxEngine(`TEST_CYCLE_${cycle}`);
                    console.log(`[TEST] Cycle ${cycle} respawn returned:`, res);

                    if (!res?.respawned || !res.newPid || res.newPid === targetPid) {
                        throw new Error(`Cycle ${cycle} failed to respawn a fresh PID: ${JSON.stringify(res)}`);
                    }

                    // Test idempotency right after recovery
                    const idem = await respawnCrashedSurface(auxSurfaceId);
                    console.log(`[TEST] Cycle ${cycle} idempotency check:`, idem);
                    if (!idem?.respawned || !idem.alreadyHealthy || idem.newPid !== res.newPid) {
                        throw new Error(`Cycle ${cycle} idempotency check failed: ${JSON.stringify(idem)}`);
                    }

                    // Wait up to 2.5s for Track-2 heartbeat from the newly respawned Pool B page
                    const hbStart = Date.now();
                    while (!poolBHeartbeatArmed && Date.now() - hbStart < 2500) {
                        await new Promise((r) => setTimeout(r, 100));
                    }
                    console.log(`[TEST] Cycle ${cycle} Pool B heartbeat armed=${poolBHeartbeatArmed} after ${Date.now() - hbStart}ms`);
                    if (!poolBHeartbeatArmed) {
                        throw new Error(`Cycle ${cycle}: Pool B Track-2 IPC heartbeat did not resume after respawn!`);
                    }
                    prevPid = res.newPid;
                }

                console.log('[TEST] All 4 crash/restore cycles verified (fresh PID, fresh frame, idempotency, and live Track-2 IPC heartbeat)!');
                cleanup();
                process.exit(0);
            })().catch((err) => {
                console.error('[TEST] Crash recovery test failed:', err);
                cleanup();
                process.exit(1);
            });
        }

        if (process.env.LOTUS_TEST_RESIZE === '1') {
            (async () => {
                console.log('[TEST] Starting rapid resize stress test...');
                // Wait for WebGL scene in sandboxed.html to start rendering
                await new Promise((r) => setTimeout(r, 1000));

                // Rapidly alternate between presets (wide_workspace is upscaling 640 -> 800)
                const presets = [
                    'wide_workspace',
                    'dual_split',
                    'default',
                    'wide_workspace',
                    'dual_split',
                    'wide_workspace',
                ];
                for (const preset of presets) {
                    console.log(`[TEST] Applying preset: ${preset}`);
                    applyLayoutPreset(preset);
                    await new Promise((r) => setTimeout(r, 150));
                }

                // Rapid continuous slider dragging (weight toggles)
                console.log('[TEST] Running rapid split-ratio drag sweeps...');
                for (let cycle = 0; cycle < 3; cycle++) {
                    for (let r = 20; r <= 80; r += 5) {
                        const ratio = r / 100;
                        poolWeights = [ratio, 1 - ratio];
                        activeWindow.setSplitWeights('pools', poolWeights);
                        publishGeometry();
                        await new Promise((res) => setTimeout(res, 30));
                    }
                    for (let r = 80; r >= 20; r -= 5) {
                        const ratio = r / 100;
                        poolWeights = [ratio, 1 - ratio];
                        activeWindow.setSplitWeights('pools', poolWeights);
                        publishGeometry();
                        await new Promise((res) => setTimeout(res, 30));
                    }
                }

                console.log('[TEST] Rapid resize stress test completed successfully without panicking or crashing!');
                cleanup();
                process.exit(0);
            })().catch((err) => {
                console.error('[TEST] Resize test failed:', err);
                process.exit(1);
            });
        }
    });

    win.on('focus', () => {
        showcaseWindowFocused = true;
        // Give Pool B a grace window after refocus before hang detection.
        if (poolBHeartbeatArmed) {
            lastPoolBHeartbeat = Date.now();
        }
        // Let sandboxed demos resume rAF (Page Visibility alone is unreliable in remotes).
        broadcastAllPanes('app.visibility', { visible: true });
    });
    win.on('blur', () => {
        showcaseWindowFocused = false;
        broadcastAllPanes('app.visibility', { visible: false });
    });

    win.on('resize', () => {
        publishGeometry();
    });

    win.on('load-status', (status, paneId) => {
        console.log(`[Lotus Sandbox Showcase] Pane "${paneId}" load status: ${status}`);
    });

    // Initial topology map from live window + tree inputs.
    publishGeometry();

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
