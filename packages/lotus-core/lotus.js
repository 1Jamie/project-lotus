const { spawn } = require('child_process');
const path = require('path');
const fs = require('fs');

// Ensure LOTUS_RESOURCES_PATH points to the packaged resources directory
if (!process.env.LOTUS_RESOURCES_PATH) {
    const resourcesPath = path.join(__dirname, 'resources');
    if (fs.existsSync(resourcesPath)) {
        process.env.LOTUS_RESOURCES_PATH = resourcesPath;
    }
}

// Auto-fix for Linux TLS allocation issue (ERR_DLOPEN_FAILED)
// This must run BEFORE loading the native module in index.js
if (process.platform === 'linux') {
    const requiredTunable = 'glibc.rtld.optional_static_tls=16384';
    const currentTunables = process.env.GLIBC_TUNABLES || '';
    const alreadyFixed = process.env.LOTUS_TLS_FIXED === '1';

    if (!alreadyFixed && !currentTunables.includes('optional_static_tls=')) {
        console.log(`[LOTUS] Detected Linux. Respawning with ${requiredTunable} to prevent TLS allocation issues...`);
        const { spawnSync } = require('child_process');
        const childArgs = process.execArgv.concat(process.argv.slice(1));
        const result = spawnSync(process.execPath, childArgs, {
            stdio: 'inherit',
            env: {
                ...process.env,
                GLIBC_TUNABLES: currentTunables ? `${currentTunables}:${requiredTunable}` : requiredTunable,
                LOTUS_TLS_FIXED: '1',
            }
        });

        process.exit(result.status ?? 0);
        return;
    }
}

// Windows ANGLE DLL discovery
// This MUST run BEFORE loading the native module (index.js).
// Native modules with hardware acceleration on Windows need ANGLE Dlls (libEGL.dll, libGLESv2.dll).
// By appending our packaged windows/ directory to PATH, the OS loader can find them.
if (process.platform === 'win32') {
    const path = require('path');
    const fs = require('fs');
    const angleDllDir = path.join(__dirname, 'windows');
    if (fs.existsSync(angleDllDir)) {
        // Find the correct casing for the PATH env var
        const pathKey = Object.keys(process.env).find(k => k.toLowerCase() === 'path') || 'PATH';
        // Prepend our directory so it takes precedence over system DLLs if any
        process.env[pathKey] = `${angleDllDir}${path.delimiter}${process.env[pathKey] || ''}`;
    }
}


const {
    App,
    createWindow,
    allocateRemotePaneShm,
    resetRemotePaneShm,
    createControlSocketPair,
    createGpuSocketPair,
    ensureWebglBroker,
    registerGpuWebglBroker,
    sendFdsOverSocket,
    registerSurfacePolicy,
    dispatchToSurface
} = require('./index.js');
const { SecureConstellationManager } = require('./secure_constellation.js');
const { isEngineInstanceAlive } = require('./launcher.js');
const EventEmitter = require('events');
const crypto = require('node:crypto');

// LOTUS_NATIVE_PROFILE=release → use target/release binaries (perf testing).
const nativeProfile = process.env.LOTUS_NATIVE_PROFILE === 'release' ? 'release' : 'debug';
const defaultSecureConstellation = new SecureConstellationManager({
    engineBin: path.resolve(__dirname, `../lotus-engine/target/${nativeProfile}/lotus-engine`),
    supervisorBin: path.resolve(__dirname, `../lotus-supervisor/target/${nativeProfile}/lotus-supervisor`),
});
let msgpackr;
try {
    msgpackr = require('msgpackr');
} catch (e) {
    console.warn('msgpackr not found, IPC will fail if binary');
}

const hostIpcPolicy = require('./host_ipc_policy.js');
const hostIpcRouter = new hostIpcPolicy.HostIpcPolicyRouter();
const { normalizeOriginIpc: normalizeOrigin } = require('./origin_normalize.js');
const { registerHostRegistry, resetHostRegistry } = require('./lotus-host-registry.js');

// Publish host IPC handles for sandbox/policy modules (avoids require cycles).
registerHostRegistry({
    hostIpcRouter,
    hostIpcPolicy,
    getIpcPort: () => ipcServerPort,
});

class IpcMain extends EventEmitter {
    constructor() {
        super();
    }

    /** Broadcast a message to ALL open windows. */
    send(channel, data) {
        for (const win of windows.values()) {
            win.sendToRenderer(channel, data);
        }
    }

    /**
     * Send a message to a single window by its ID.
     * Used internally by handle() to route invoke() replies to the
     * originating window only, avoiding unnecessary broadcasts.
     */
    sendTo(clientId, channel, data) {
        if (!clientId) return;
        
        // Handle both windowId and windowId:paneId formats
        const [windowId, paneId] = clientId.split(':');
        const win = windows.get(windowId);
        if (!win) return;
        
        if (paneId) {
            win.sendToPaneRenderer(paneId, channel, data);
        } else {
            win.sendToRenderer(channel, data);
        }
    }

    /**
     * Register a request/reply handler for window.lotus.invoke() calls.
     * The handler receives the payload (with _replyId stripped) and may
     * return a value or a Promise. The resolved value is automatically
     * sent back to the originating renderer window only (not broadcast).
     * Thrown / rejected errors are forwarded as { _error: message }.
     */
    handle(channel, handler) {
        // EventEmitter passes extra args -- second arg is the windowId (or windowId:paneId) 
        // threaded through ipcMain.emit() by the batch routing code below.
        this.on(channel, async (data, fromClientId) => {
            const replyId = data && data._replyId;
            if (!replyId) return; // not an invoke() call; ignore
            const payload = Object.assign({}, data);
            delete payload._replyId;

            // Security Boundary: Sandboxed surfaces (Track 2) cannot invoke privileged Track 1 host handlers!
            if (fromClientId) {
                let surface = hostIpcRouter.surfaceTracker.getSurface(fromClientId);
                if (!surface && !fromClientId.includes(':')) {
                    surface = hostIpcRouter.surfaceTracker.getSurface(`${fromClientId}:main`);
                }
                if (surface && surface.secure) {
                    const errMessage = `E_UNAUTHORIZED: Sandboxed surface "${fromClientId}" cannot invoke privileged host handler "${channel}"`;
                    const replyEnvelope = {
                        type: 'reply',
                        id: replyId,
                        error: errMessage
                    };
                    const jsonBuf = Buffer.from(JSON.stringify(replyEnvelope));
                    if (typeof dispatchToSurface === 'function') {
                        try {
                            dispatchToSurface(surface.surfaceId, jsonBuf);
                        } catch (_) {}
                    }
                    this.emit('security-violation', {
                        surfaceId: fromClientId,
                        method: channel,
                        error: errMessage,
                        payload
                    });
                    this.sendTo(fromClientId, replyId, { _error: errMessage });
                    return;
                }
            }

            try {
                const result = await handler(payload);
                // Reply to the originating client (window or pane) if possible.
                if (fromClientId) {
                    this.sendTo(fromClientId, replyId, result);
                } else {
                    this.send(replyId, result); // fallback for legacy paths
                }
            } catch (err) {
                const errPayload = { _error: err?.message ?? String(err) };
                if (fromClientId) {
                    this.sendTo(fromClientId, replyId, errPayload);
                } else {
                    this.send(replyId, errPayload);
                }
            }
        });
    }

    register(method, handler, options) {
        const unregisterFromRouter = hostIpcRouter.registerSecureMethod(method, handler, options);

        const onMethodCall = async (data, fromClientId) => {
            const replyId = data && data._replyId;
            if (!replyId) return;

            const payload = Object.assign({}, data);
            delete payload._replyId;

            try {
                const targetClientId = fromClientId || 'main-window:main';
                let surface = hostIpcRouter.surfaceTracker.getSurface(targetClientId);
                if (!surface && !targetClientId.includes(':')) {
                    surface = hostIpcRouter.surfaceTracker.getSurface(`${targetClientId}:main`);
                }
                if (!surface) {
                    const err = new Error(`E_SURFACE_NOT_FOUND: Surface "${targetClientId}" is not registered`);
                    err.code = 'E_SURFACE_NOT_FOUND';
                    throw err;
                }

                // Execute Track 2 inbound validation
                const envelope = {
                    method,
                    documentId: surface.activeDocumentId,
                    payload
                };
                const wireBytes = JSON.stringify(payload).length;
                const regEntry = hostIpcRouter.validateTrack2Inbound(surface, envelope, wireBytes);
                const reqDocId = surface.activeDocumentId;
                const reqGenId = regEntry.generationId;

                // Execute handler
                const rawResult = await regEntry.handler(payload);

                // Execute Track 2 post-await validation
                if (!hostIpcRouter.validateTrack2PostAwait(surface, regEntry, reqDocId, reqGenId)) {
                    return; // Dropped post-await
                }

                // Two-phase output projection (supports both `returns` and `schema`)
                const outputSchema = options ? (options.returns || options.schema) : undefined;
                const safeResult = hostIpcPolicy.validateAndProjectOutput(outputSchema, rawResult);

                // Reply to originating client
                if (surface && surface.secure) {
                    const replyEnvelope = {
                        type: 'reply',
                        id: replyId,
                        result: safeResult
                    };
                    const jsonBuf = Buffer.from(JSON.stringify(replyEnvelope));
                    if (typeof dispatchToSurface === 'function') {
                        try {
                            dispatchToSurface(surface.surfaceId, jsonBuf);
                        } catch (_) {}
                    }
                    if (fromClientId) {
                        this.sendTo(fromClientId, replyId, safeResult);
                    }
                } else if (fromClientId) {
                    this.sendTo(fromClientId, replyId, safeResult);
                } else {
                    this.send(replyId, safeResult);
                }
            } catch (err) {
                const errMessage = err?.message ?? String(err);
                if (errMessage.includes('E_CHANNEL_FORBIDDEN') || errMessage.includes('E_SURFACE_SECURE_MISMATCH') || errMessage.includes('E_IPC_INELIGIBLE')) {
                    this.emit('security-violation', {
                        surfaceId: fromClientId || 'main-window:main',
                        method,
                        error: errMessage,
                        payload
                    });
                }
                const errPayload = { _error: errMessage };
                const targetClientId = fromClientId || 'main-window:main';
                let surface = hostIpcRouter.surfaceTracker.getSurface(targetClientId);
                if (!surface && !targetClientId.includes(':')) {
                    surface = hostIpcRouter.surfaceTracker.getSurface(`${targetClientId}:main`);
                }
                if (surface && surface.secure) {
                    const replyEnvelope = {
                        type: 'reply',
                        id: replyId,
                        error: err?.message ?? String(err)
                    };
                    const jsonBuf = Buffer.from(JSON.stringify(replyEnvelope));
                    if (typeof dispatchToSurface === 'function') {
                        try {
                            dispatchToSurface(surface.surfaceId, jsonBuf);
                        } catch (_) {}
                    }
                }
                if (fromClientId) {
                    this.sendTo(fromClientId, replyId, errPayload);
                } else {
                    this.send(replyId, errPayload);
                }
            }
        };

        this.on(method, onMethodCall);

        return () => {
            unregisterFromRouter();
            this.off(method, onMethodCall);
        };
    }

    registerEvent(eventName, options) {
        return hostIpcRouter.registerSecureEvent(eventName, options);
    }

    unregister(method) {
        this.removeAllListeners(method);
        return hostIpcRouter.unregisterSecureMethod(method);
    }
}

const isProfiling = process.argv.includes('--profile');

const MSG_TYPE_CONTROL = 0x01;
const MSG_TYPE_DATA = 0x02;
const MSG_TYPE_CHUNK = 0x03;

/**
 * Handles reassembly of multi-part (chunked) IPC messages.
 * Prevents main thread blocking during large transfers (e.g. 10MB+)
 * by allowing the event loop to interleave other tasks between chunks.
 */
class MessageAssembler {
    constructor(onComplete) {
        this.assemblies = new Map();
        this.onComplete = onComplete;
        this.CLEANUP_INTERVAL = 10000; // 10s
        
        const timer = setInterval(() => this.cleanup(), this.CLEANUP_INTERVAL);
        if (timer && typeof timer.unref === 'function') {
            timer.unref();
        }
    }

    handleChunk(clientId, data, unpacker) {
        if (data.length < 9) return;
        
        // Protocol: [Type(1)][MsgID(4)][Total(2)][Index(2)][Payload(N)]
        const msgId = data.readUInt32BE(1);
        const total = data.readUInt16BE(5);
        const index = data.readUInt16BE(7);
        const payload = data.subarray(9);
        
        const key = `${clientId}:${msgId}`;
        let assembly = this.assemblies.get(key);
        
        if (!assembly) {
            assembly = {
                chunks: new Array(total),
                received: 0,
                total: total,
                lastActivity: Date.now()
            };
            this.assemblies.set(key, assembly);
        }
        
        if (!assembly.chunks[index]) {
            assembly.chunks[index] = payload;
            assembly.received++;
            assembly.lastActivity = Date.now();
        }
        
        if (assembly.received === assembly.total) {
            this.assemblies.delete(key);
            const fullBuffer = Buffer.concat(assembly.chunks);
            this.onComplete(clientId, fullBuffer, unpacker);
        }
    }

    cleanup() {
        const now = Date.now();
        for (const [key, assembly] of this.assemblies) {
            if (now - assembly.lastActivity > this.CLEANUP_INTERVAL) {
                this.assemblies.delete(key);
            }
        }
    }
}
const startTime = Date.now();

if (isProfiling) {
    console.log(`[PROFILE] Node.js process started at ${new Date(startTime).toISOString()}`);
}

const ipcMain = new IpcMain();
let globalApp = null;
let ipcServerPort = 0;
// Refresh registry getIpcPort now that the binding exists (idempotent).
registerHostRegistry({
    getIpcPort: () => ipcServerPort,
});
let systemTheme = 'light';
let vfsInitialized = false;
const windows = new Map();
const eventQueue = new Map(); // windowId -> Array of pending events
const globalPackers = new Map();
const globalUnpackers = new Map();

/**
 * Host-level robust theme detection for Linux using desktop settings.
 * Performed in Node Host to avoid subprocess execution inside the sandboxed engine.
 */
function detectLinuxThemeNode() {
    if (process.platform !== 'linux') return null;
    try {
        const { execFileSync } = require('node:child_process');
        try {
            const out = execFileSync('gsettings', ['get', 'org.gnome.desktop.interface', 'color-scheme'], {
                encoding: 'utf8',
                stdio: ['ignore', 'pipe', 'ignore'],
                timeout: 500
            }).toLowerCase();
            if (out.includes('prefer-dark')) return 'dark';
            if (out.includes('prefer-light') || out.includes('default')) return 'light';
        } catch {}
        try {
            const out = execFileSync('gsettings', ['get', 'org.gnome.desktop.interface', 'gtk-theme'], {
                encoding: 'utf8',
                stdio: ['ignore', 'pipe', 'ignore'],
                timeout: 500
            }).toLowerCase();
            if (out.includes('-dark') || out.includes(':dark')) return 'dark';
        } catch {}
    } catch {}
    return null;
}

/**
 * Validates platform security gating for requested isolation parameters.
 * Fails closed with E_SECURITY_ISOLATION_UNSUPPORTED_PLATFORM when secure: true
 * (separateProcess: true) is requested without verified OS sandbox support.
 */
function validatePlatformSecurityIsolation(options = {}) {
    if (!options || typeof options !== 'object') return;
    const isSecure = options.secure === true || options.sandbox === true;
    const separateProcess = options.separateProcess !== false; // defaults to true when sandbox/secure: true
    if (!isSecure || !separateProcess) {
        return;
    }

    if (process.platform !== 'linux') {
        const err = new Error(
            `Process-isolated secure engine is not supported on platform "${process.platform}". ` +
            `Opt into in-process hosting via { separateProcess: false } or run on a supported platform.`
        );
        err.code = 'E_SECURITY_ISOLATION_UNSUPPORTED_PLATFORM';
        throw err;
    }

    // Flatpak sandboxes block nested CLONE_NEWUSER / CLONE_NEWNET creation
    try {
        const fs = require('node:fs');
        if (process.env.FLATPAK_ID || fs.existsSync('/.flatpak-info')) {
            const err = new Error(
                'Process-isolated secure engine cannot spawn nested user/network namespaces inside a Flatpak sandbox.'
            );
            err.code = 'E_SECURITY_ISOLATION_UNSUPPORTED_PLATFORM';
            throw err;
        }
        if (fs.existsSync('/proc/sys/kernel/unprivileged_userns_clone')) {
            const val = fs.readFileSync('/proc/sys/kernel/unprivileged_userns_clone', 'utf8').trim();
            if (val === '0') {
                const err = new Error(
                    'Process-isolated secure engine requires unprivileged user namespaces, ' +
                    'but /proc/sys/kernel/unprivileged_userns_clone is 0.'
                );
                err.code = 'E_SECURITY_ISOLATION_UNSUPPORTED_PLATFORM';
                throw err;
            }
        }
        if (fs.existsSync('/proc/sys/kernel/apparmor_restrict_unprivileged_userns')) {
            const val = fs.readFileSync('/proc/sys/kernel/apparmor_restrict_unprivileged_userns', 'utf8').trim();
            if (val === '1') {
                const err = new Error(
                    'Process-isolated secure engine requires unprivileged user namespaces, ' +
                    'but kernel.apparmor_restrict_unprivileged_userns is active.'
                );
                err.code = 'E_SECURITY_ISOLATION_UNSUPPORTED_PLATFORM';
                throw err;
            }
        }
    } catch (e) {
        if (e.code === 'E_SECURITY_ISOLATION_UNSUPPORTED_PLATFORM') throw e;
    }
}

ipcMain.on('__lotus_select_toggle', (payload, clientId) => {
    if (!clientId) return;
    const windowId = clientId.split(':')[0];
    const win = windows.get(windowId);
    if (win && win.handle && payload) {
        win.handle.updateSelectControl(payload.id || '', payload.selectedIndices || []);
    }
});

ipcMain.on('__lotus_select_choice', (payload, clientId) => {
    if (!clientId) return;
    const windowId = clientId.split(':')[0];
    const win = windows.get(windowId);
    if (win && win.handle && payload) {
        win.handle.submitSelectControl(payload.id || '', payload.selectedIndices || []);
        win._hideSelectOverlay();
    }
});

ipcMain.on('__lotus_select_dismiss', (payload, clientId) => {
    if (!clientId) return;
    const windowId = clientId.split(':')[0];
    const win = windows.get(windowId);
    if (win && win.handle && payload) {
        win.handle.dismissSelectControl(payload.id || '');
        win._hideSelectOverlay();
    }
});

ipcMain.on('__lotus_select_resize', (payload, clientId) => {
    if (!clientId) return;
    const windowId = clientId.split(':')[0];
    const win = windows.get(windowId);
    if (win && payload && payload.height) {
        win._resizeSelectOverlay(payload.height, payload.id);
    }
});

ipcMain.on('__lotus_select_ready', (_payload, clientId) => {
    if (!clientId) return;
    const windowId = clientId.split(':')[0];
    const win = windows.get(windowId);
    if (win) {
        win._selectOverlayReady = true;
        if (win._pendingSelectPayload) {
            win.sendToPaneRenderer('__lotus_select_overlay', 'populate-select', win._pendingSelectPayload, true);
            win._pendingSelectPayload = null;
        }
    }
});

function crashChromePaneId(paneId) {
    return `__lotus_crash_${paneId}`;
}

function parseSurfaceId(surfaceId) {
    if (!surfaceId || typeof surfaceId !== 'string') return null;
    const idx = surfaceId.indexOf(':');
    if (idx <= 0 || idx >= surfaceId.length - 1) return null;
    return {
        windowId: surfaceId.slice(0, idx),
        paneId: surfaceId.slice(idx + 1),
    };
}

const CRASH_CHROME_Z_INDEX = 2_000_000;

function ensureCrashChromePane(win, pane) {
    const crashId = crashChromePaneId(pane.id);
    let crash = win.panes.get(crashId);
    if (crash) return crash;

    const layout = pane._layout || {};
    // Absolute high-z overlay — never participates in dock layout. The remote keeps its
    // dock slot (and vsync) underneath while chrome covers the slot visually.
    // paneId in the query lets Respawn arm even if the first crash-info push races load.
    crash = win.createPane(crashId, {
        url: `lotus-resource://localhost/surface-crash.html?paneId=${encodeURIComponent(pane.id)}`,
        x: layout.x || 0,
        y: layout.y || 0,
        width: layout.width || 0,
        height: layout.height || 0,
        zIndex: CRASH_CHROME_Z_INDEX,
        anchor: Anchor.None,
        dockOrder: 0,
        visible: false,
        sandbox: false,
        secure: false,
    });
    return crash;
}

function buildCrashInfoPayload(pane, surfaceId) {
    const crashed = pane?._crashed || {};
    return {
        paneId: pane.id,
        surfaceId: surfaceId || pane.surfaceId || null,
        code: crashed.code ?? null,
        sig: crashed.sig || null,
        reason: crashed.reason || null,
        isolated: crashed.isolated === true,
        autoRespawnFailures: crashed.autoRespawnFailures || 0,
    };
}

function pushCrashInfoToChrome(win, crash, pane, surfaceId) {
    if (!win || !crash || !pane?._crashed) return;
    const info = buildCrashInfoPayload(pane, surfaceId);
    // Latch like select-overlay `_pendingSelectPayload`: crash-info sent before the
    // chrome page's WS is up is recovered on `__lotus_crash_ready`.
    crash._pendingCrashInfo = info;
    win.sendToPaneRenderer(crash.id, 'crash-info', info, true);
}

function coverRemoteWithCrashChrome(win, pane, crash) {
    if (!win?.handle || !pane || !crash) return;
    if (typeof win.handle.coverRemoteWithCrashChrome === 'function') {
        win.handle.coverRemoteWithCrashChrome(pane.id, crash.id);
        crash.visible = true;
        pane.visible = true;
        return;
    }
    // Fallback without native cover helper: keep remote in the compositor and stack chrome.
    win.handle.setPaneVisible(pane.id, true);
    crash.visible = true;
    win.handle.setPaneVisible(crash.id, true);
}

const AUTO_RESPAWN_HEALTHY_MS = 5000;
const AUTO_RESPAWN_BACKOFF_MS = 250;

function clearAutoRespawnHealthyTimer(pane) {
    if (pane?._autoRespawnHealthyTimer) {
        clearTimeout(pane._autoRespawnHealthyTimer);
        pane._autoRespawnHealthyTimer = null;
    }
}

function resetAutoRespawnStreak(pane) {
    if (!pane) return;
    clearAutoRespawnHealthyTimer(pane);
    pane._autoRespawnFailures = 0;
}

function armAutoRespawnHealthyReset(pane) {
    if (!pane) return;
    clearAutoRespawnHealthyTimer(pane);
    pane._autoRespawnHealthyTimer = setTimeout(() => {
        pane._autoRespawnHealthyTimer = null;
        pane._autoRespawnFailures = 0;
    }, AUTO_RESPAWN_HEALTHY_MS);
    if (typeof pane._autoRespawnHealthyTimer.unref === 'function') {
        pane._autoRespawnHealthyTimer.unref();
    }
}

function stepPlaneDim(physPx) {
    const STEP = 512;
    const MAX = 4096;
    const grown = Math.ceil(physPx * 1.5);
    return Math.min(MAX, Math.max(STEP, Math.ceil(grown / STEP) * STEP));
}

function sanitizePaneShm(pane) {
    if (!pane?._shm || typeof resetRemotePaneShm !== 'function') return;
    // If layout grew past the sealed plane (e.g. after native realloc or HiDPI
    // maximize), mint a fresh stepped plane so CreateSurface on respawn fits.
    try {
        const layoutW = Math.max(1, Math.round(pane._layout?.width || pane._shm.width || 800));
        const layoutH = Math.max(1, Math.round(pane._layout?.height || pane._shm.height || 600));
        // Prefer physical ceiling already stored on the plane; else step from CSS layout.
        const needW = stepPlaneDim(layoutW);
        const needH = stepPlaneDim(layoutH);
        const curW = pane._shm.maxWidth || pane._shm.width || 0;
        const curH = pane._shm.maxHeight || pane._shm.height || 0;
        if (
            (needW > curW || needH > curH) &&
            typeof allocateRemotePaneShm === 'function'
        ) {
            const fresh = allocateRemotePaneShm(
                `respawn_${pane.id}_${Date.now()}`,
                Math.max(needW, curW),
                Math.max(needH, curH)
            );
            fresh.width = layoutW;
            fresh.height = layoutH;
            fresh.maxWidth = Math.max(needW, curW);
            fresh.maxHeight = Math.max(needH, curH);
            pane._shm = fresh;
            return;
        }
    } catch (_) {}
    const pixelRwFd = pane._shm.pixelRwFd ?? pane._shm.rwFd ?? -1;
    const ackRwFd = pane._shm.ackRwFd ?? -1;
    try {
        resetRemotePaneShm(pixelRwFd, ackRwFd);
    } catch (_) {}
}

function showHostCrashChrome(surfaceId, crashInfo = {}) {
    const parsed = parseSurfaceId(surfaceId);
    if (!parsed) return;
    const win = windows.get(parsed.windowId);
    if (!win) return;
    const pane = win.panes.get(parsed.paneId);
    if (!pane || !pane.isRemote) return;

    // Fault-injection demos stash a human label (SIGSYS/SIGABRT/watchdog) because the
    // process is always terminated with SIGKILL (blockable signals are unreliable).
    const fault = pane._faultInjection || null;
    pane._faultInjection = null;
    if (fault?.sig && (!crashInfo.sig || crashInfo.sig === 'SIGKILL')) {
        crashInfo.sig = fault.sig;
    }
    if (fault?.reason && !crashInfo.reason) {
        crashInfo.reason = fault.reason;
    }

    pane._crashed = {
        code: crashInfo.code ?? null,
        sig: fault?.sig || crashInfo.sig || null,
        reason: fault?.reason || crashInfo.reason || null,
        isolated: crashInfo.isolated === true,
        instanceId: crashInfo.instanceId,
        autoRespawnFailures: pane._autoRespawnFailures || 0,
    };
    pane._engineInstance = null;
    pane._respawning = false;

    // Sanitize SHM ring buffer immediately so dead-engine frames cannot be consumed.
    sanitizePaneShm(pane);

    const crash = ensureCrashChromePane(win, pane);
    // Keep the remote docked+visible under opaque host chrome so a later respawn already
    // has a compositor slot / vsync path — never swap to a frozen SHM flash.
    coverRemoteWithCrashChrome(win, pane, crash);
    pushCrashInfoToChrome(win, crash, pane, surfaceId);
    // Retry while the chrome page brings up its WS — first paint often races this push.
    if (crash._crashInfoRetryTimer) {
        clearInterval(crash._crashInfoRetryTimer);
    }
    let tries = 0;
    crash._crashInfoRetryTimer = setInterval(() => {
        if (!pane._crashed || !crash._pendingCrashInfo) {
            clearInterval(crash._crashInfoRetryTimer);
            crash._crashInfoRetryTimer = null;
            return;
        }
        win.sendToPaneRenderer(crash.id, 'crash-info', crash._pendingCrashInfo, true);
        if (++tries >= 15) {
            clearInterval(crash._crashInfoRetryTimer);
            crash._crashInfoRetryTimer = null;
        }
    }, 100);
}

const _respawnInFlight = new Map();
const RESPAWN_FIRST_FRAME_MS = 20000;

/**
 * Arm before acquireSecureEngine. Ready requires BOTH:
 * 1. Verified navigation commit (`load-status === 'complete'`), AND
 * 2. Verified post-load host GL texture upload (`remote-frame-composited`).
 */
function waitForRespawnReady(pane, win, timeoutMs) {
    return new Promise((resolve) => {
        let settled = false;
        let loadComplete = false;
        let compositedAfterLoad = false;
        let latestCompositedSeq = 0;
        let postLoadFallbackTimer = null;

        const finish = (ok, reason) => {
            if (settled) return;
            settled = true;
            clearTimeout(timer);
            if (postLoadFallbackTimer) clearTimeout(postLoadFallbackTimer);
            pane.removeListener('remote-frame-composited', onComposited);
            pane.removeListener('load-status', onLoad);
            resolve({ ok, reason, pubSeq: latestCompositedSeq });
        };

        const onComposited = (pubSeq) => {
            const seq = Number(pubSeq) || 0;
            if (seq > latestCompositedSeq) {
                latestCompositedSeq = seq;
            } else if (latestCompositedSeq === 0) {
                latestCompositedSeq = 1;
            }
            if (loadComplete) {
                compositedAfterLoad = true;
                finish(true, 'verified-load-and-composited');
            }
        };

        const onLoad = (status) => {
            if (status === 'failed') {
                finish(false, 'load-failed');
                return;
            }
            if (status === 'complete') {
                loadComplete = true;
                if (win?.handle && typeof win.handle.requestRedraw === 'function') {
                    win.handle.requestRedraw();
                }
                if (compositedAfterLoad || latestCompositedSeq >= 2) {
                    finish(true, 'verified-load-and-composited');
                } else if (!postLoadFallbackTimer) {
                    postLoadFallbackTimer = setTimeout(() => {
                        postLoadFallbackTimer = null;
                        if (win?.handle && typeof win.handle.requestRedraw === 'function') {
                            win.handle.requestRedraw();
                        }
                        if (latestCompositedSeq >= 1) {
                            finish(true, 'verified-load-and-initial-composited');
                        }
                    }, 120);
                }
            }
        };

        const timer = setTimeout(() => finish(false, 'timeout'), timeoutMs);
        pane.on('remote-frame-composited', onComposited);
        pane.on('load-status', onLoad);
    });
}

function restoreSurfaceGrants(pane) {
    if (!pane?.surfaceId || !hostIpcRouter?.surfaceTracker) return;
    const surface = hostIpcRouter.surfaceTracker.getSurface(pane.surfaceId);
    if (!surface) return;
    const channels = Array.isArray(pane._allowedChannels) ? pane._allowedChannels : [];
    const origins = Array.isArray(pane._trustedOrigins) ? pane._trustedOrigins : [];
    surface.allowedChannels = new Set(channels);
    surface.trustedOrigins = new Set(origins.map((o) => {
        try {
            return normalizeOrigin(o);
        } catch (_) {
            return o;
        }
    }).filter((o) => o && o !== 'null')); // opaque "null" never grants trust
    // Dying-generation failNavigation leaves navigationState=failed and empty grants.
    // Re-arm so CreateSurface can bake a coherent bridge for the successor.
    if (surface.navigationState === 'failed') {
        surface.navigationState = 'provisional';
        surface.ipcEligible = false;
    }
    if (typeof hostIpcPolicy.syncSurfacePolicyToNative === 'function') {
        hostIpcPolicy.syncSurfacePolicyToNative(surface);
    }
}

function revealRemoteAfterRespawn(win, pane, crash) {
    pane._crashed = null;
    pane._respawning = false;
    // Remote was already visible under chrome (vsync path). Only lift the cover.
    if (crash) {
        if (crash._crashInfoRetryTimer) {
            clearInterval(crash._crashInfoRetryTimer);
            crash._crashInfoRetryTimer = null;
        }
        crash._pendingCrashInfo = null;
        crash.visible = false;
        win.handle.setPaneVisible(crash.id, false);
    }
    pane.visible = true;
    win.handle.setPaneVisible(pane.id, true);
    if (typeof win.handle.clearRemoteCrashCover === 'function') {
        win.handle.clearRemoteCrashCover(pane.id);
    }
}

async function respawnCrashedSurface(surfaceId) {
    const parsed = parseSurfaceId(surfaceId);
    if (!parsed) {
        return { respawned: false, error: 'Invalid surfaceId' };
    }
    if (_respawnInFlight.has(surfaceId)) {
        return _respawnInFlight.get(surfaceId);
    }

    const run = (async () => {
        const win = windows.get(parsed.windowId);
        if (!win) return { respawned: false, error: 'Window not found' };
        const pane = win.panes.get(parsed.paneId);
        if (!pane || !pane.isRemote) return { respawned: false, error: 'Remote pane not found' };
        if (!pane._shm) return { respawned: false, error: 'Remote pane SHM missing' };

        // Verified idempotency: check actual OS process liveness (not just JS flags)
        // so a recently SIGKILLed engine whose exit event hasn't fired is still respawned.
        if (!pane._crashed && !pane._respawning && isEngineInstanceAlive(pane._engineInstance)) {
            return {
                respawned: true,
                alreadyHealthy: true,
                newPid: pane._engineInstance.enginePid,
                surfaceId,
                paneId: pane.id,
            };
        }

        const crashId = crashChromePaneId(pane.id);
        let crash = win.panes.get(crashId);
        const url = pane._url || pane._pendingUrl || 'about:blank';
        // Manual / chrome path covers with Aw Snap. Silent autoRespawn must not flash chrome.
        const useCrashChrome = Boolean(pane._crashed) || Boolean(crash?.visible);

        try {
            pane._respawning = true;

            // Step 1: Retire prior isolated instance and verify its OS process is reaped
            // before sanitizing SHM, so the dying engine cannot write a stale frame.
            if (pane.isIsolated === true && typeof defaultSecureConstellation._retireIsolatedInstance === 'function') {
                await defaultSecureConstellation._retireIsolatedInstance(surfaceId);
            }

            // Step 2: Synchronously zero SHM producer & consumer headers on the host.
            sanitizePaneShm(pane);

            // Step 3: Cover and arm the host compositor BEFORE spawning the successor engine.
            if (useCrashChrome) {
                if (!crash) {
                    crash = ensureCrashChromePane(win, pane);
                }
                coverRemoteWithCrashChrome(win, pane, crash);
            } else {
                pane.visible = true;
                win.handle.setPaneVisible(pane.id, true);
            }
            if (typeof win.handle.armCrashCoverFreshFrame === 'function') {
                win.handle.armCrashCoverFreshFrame(pane.id);
            }

            // Step 4: Restore surface grants and begin listening for verified load + frame.
            restoreSurfaceGrants(pane);
            pane._pendingNavId = null;
            pane._pendingUrl = url;

            const readyWait = waitForRespawnReady(pane, win, RESPAWN_FIRST_FRAME_MS);

            // Step 5: Spawn successor engine and send CreateSurface.
            const acquireOpts = {
                pane,
                url,
                shm: pane._shm,
                isolated: pane.isIsolated === true,
            };
            let engineRes;
            try {
                engineRes = await defaultSecureConstellation.acquireSecureEngine(surfaceId, acquireOpts);
            } catch (firstErr) {
                // Transient spawn race vs. dying supervisor teardown — one retry.
                await new Promise((r) => setTimeout(r, 150));
                sanitizePaneShm(pane);
                restoreSurfaceGrants(pane);
                pane._pendingNavId = null;
                pane._pendingUrl = url;
                try {
                    engineRes = await defaultSecureConstellation.acquireSecureEngine(surfaceId, acquireOpts);
                } catch (retryErr) {
                    throw new Error(
                        (retryErr && retryErr.message) ||
                        (firstErr && firstErr.message) ||
                        String(retryErr || firstErr)
                    );
                }
            }

            // Step 6: Attach successor controlFd for input/vsync/resize lockstep.
            pane._engineInstance = engineRes.instance;
            pane._engineInstanceId = engineRes.instanceId;
            if (engineRes.instance.controlFd && win.handle?.attachRemotePaneInput) {
                win.handle.attachRemotePaneInput(pane.id, engineRes.instance.controlFd);
            }
            if (typeof win.handle.requestRedraw === 'function') {
                win.handle.requestRedraw();
            }

            // Step 7: Wait for verified navigation commit AND post-load GL texture upload.
            const ready = await readyWait;
            if (!ready.ok) {
                pane._respawning = false;
                pane._engineInstance = null;
                // Drop the half-ready successor so a later Respawn starts clean.
                try {
                    await defaultSecureConstellation.releaseSecureEngine(surfaceId);
                } catch (_) {}
                sanitizePaneShm(pane);
                if (useCrashChrome) {
                    coverRemoteWithCrashChrome(win, pane, crash);
                }
                return {
                    respawned: false,
                    error: ready.reason === 'load-failed'
                        ? 'Respawn load failed'
                        : 'Respawn timed out waiting for first frame',
                    newPid: engineRes.enginePid,
                };
            }

            // Step 8: Reveal verified surface and broadcast canonical lifecycle recovery.
            revealRemoteAfterRespawn(win, pane, useCrashChrome ? crash : null);
            resetAutoRespawnStreak(pane);
            armAutoRespawnHealthyReset(pane);

            const respawnedEvt = {
                surfaceId,
                paneId: pane.id,
                newPid: engineRes.enginePid,
                isolated: pane.isIsolated === true,
                pubSeq: ready.pubSeq || 1,
            };
            pane.emit('respawned', respawnedEvt);
            ipcMain.emit('system.remoteSurfaceRespawned', respawnedEvt);
            ipcMain.send('system.remoteSurfaceRespawned', respawnedEvt);

            return {
                respawned: true,
                newPid: engineRes.enginePid,
                surfaceId,
                paneId: pane.id,
            };
        } catch (err) {
            pane._respawning = false;
            return { respawned: false, error: err?.message || String(err) };
        }
    })();

    _respawnInFlight.set(surfaceId, run);
    try {
        return await run;
    } finally {
        _respawnInFlight.delete(surfaceId);
    }
}

async function handleRemoteSurfaceCrash(surfaceId, crashInfo = {}) {
    const parsed = parseSurfaceId(surfaceId);
    if (!parsed) return;
    const win = windows.get(parsed.windowId);
    if (!win) return;
    const pane = win.panes.get(parsed.paneId);
    if (!pane || !pane.isRemote) return;

    // Ignore late crash notifications if a respawn is already in flight or if a
    // newer engine generation is already attached to this pane.
    if (_respawnInFlight.has(surfaceId) || pane._respawning) {
        return;
    }
    if (
        crashInfo.instanceId != null &&
        pane._engineInstanceId != null &&
        crashInfo.instanceId !== pane._engineInstanceId &&
        isEngineInstanceAlive(pane._engineInstance)
    ) {
        return;
    }

    clearAutoRespawnHealthyTimer(pane);
    pane._engineInstance = null;

    const maxAttempts = pane.autoRespawnMaxAttempts > 0 ? pane.autoRespawnMaxAttempts : 3;
    if (pane.autoRespawn === true) {
        pane._autoRespawnFailures = (pane._autoRespawnFailures || 0) + 1;
        if (pane._autoRespawnFailures <= maxAttempts) {
            // Silent heal: no crash chrome. Remote stays compositor-visible for vsync.
            pane._crashed = null;
            if (AUTO_RESPAWN_BACKOFF_MS > 0) {
                await new Promise((r) => setTimeout(r, AUTO_RESPAWN_BACKOFF_MS));
            }
            const result = await respawnCrashedSurface(surfaceId);
            if (result?.respawned) {
                return;
            }
            // Fall through to chrome when the heal attempt itself fails.
        }
    }

    showHostCrashChrome(surfaceId, crashInfo);
}

defaultSecureConstellation.on('crashed', (event) => {
    const surfaces = Array.isArray(event?.surfaces) ? event.surfaces : [];
    for (const surfaceId of surfaces) {
        handleRemoteSurfaceCrash(surfaceId, event).catch((err) => {
            console.error('[Lotus] handleRemoteSurfaceCrash error:', err);
            showHostCrashChrome(surfaceId, event);
        });
    }
});

ipcMain.on('__lotus_crash_respawn', async (payload, clientId) => {
    const paneId = payload?.paneId;
    if (!paneId || !clientId) return;
    const windowId = clientId.split(':')[0];
    const win = windows.get(windowId);
    const pane = win?.panes.get(paneId);
    const surfaceId = pane?.surfaceId || `${windowId}:${paneId}`;
    const result = await respawnCrashedSurface(surfaceId);
    if (win) {
        const crashId = crashChromePaneId(paneId);
        // Only ack the crash chrome while it is still the visible slot. After a
        // successful reveal the chrome pane is hidden; a late result is harmless.
        win.sendToPaneRenderer(crashId, 'crash-respawn-result', result, true);
    }
});

ipcMain.on('__lotus_crash_ready', (_payload, clientId) => {
    if (!clientId) return;
    const [windowId, crashPaneId] = clientId.split(':');
    if (!windowId || !crashPaneId || !crashPaneId.startsWith('__lotus_crash_')) return;
    const paneId = crashPaneId.slice('__lotus_crash_'.length);
    const win = windows.get(windowId);
    const pane = win?.panes.get(paneId);
    const crash = win?.panes.get(crashPaneId);
    if (!win || !pane?._crashed || !crash) return;
    // Prefer the latched payload (mirrors `__lotus_select_ready` + `_pendingSelectPayload`).
    const info = crash._pendingCrashInfo || buildCrashInfoPayload(pane, pane.surfaceId);
    crash._pendingCrashInfo = info;
    win.sendToPaneRenderer(crashPaneId, 'crash-info', info, true);
});

function ensureApp() {
    if (!globalApp) {
        if (isProfiling) console.log("[PROFILE] Initializing Lotus App...");
        
        // Read package.json to get app identifier
        let appIdentifier = 'lotus'; // default fallback
        try {
            const fs = require('fs');
            const path = require('path');
            const pkgPath = path.join(process.cwd(), 'package.json');
            if (fs.existsSync(pkgPath)) {
                const pkg = JSON.parse(fs.readFileSync(pkgPath, 'utf8'));
                // Use 'name' field from package.json (e.g., "servo-node" -> "servo-node")
                // Or use a custom 'appId' field if provided
                appIdentifier = pkg.appId || pkg.name || 'lotus';
            }
        } catch (e) {
            console.warn('[Lotus] Could not read package.json, using default app identifier');
        }

        // Read msgpackr source from node_modules
        let msgpackrSource = '// msgpackr not found';
        try {
            const path = require('path');
            const fs = require('fs');
            const execDirMsgpackr = path.join(path.dirname(process.execPath), 'msgpackr-renderer.js');
            const usrLibMsgpackr = path.join('/usr/lib', path.basename(process.execPath), 'msgpackr-renderer.js');
            const appLibMsgpackr = path.join('/app/lib', path.basename(process.execPath), 'msgpackr-renderer.js');

            if (fs.existsSync(execDirMsgpackr)) {
                msgpackrSource = fs.readFileSync(execDirMsgpackr, 'utf8');
            } else if (fs.existsSync(usrLibMsgpackr)) {
                msgpackrSource = fs.readFileSync(usrLibMsgpackr, 'utf8');
            } else if (fs.existsSync(appLibMsgpackr)) {
                msgpackrSource = fs.readFileSync(appLibMsgpackr, 'utf8');
            } else {
                let msgpackrPath;
                try {
                    // Try to resolve it via subpath (might fail due to 'exports' in newer node)
                    msgpackrPath = require.resolve('msgpackr/dist/index.min.js');
                } catch (e) {
                    // Fallback: use the main entry point to find the directory
                    // msgpackr exports its node version in dist/node.cjs
                    const mainPath = require.resolve('msgpackr');
                    msgpackrPath = path.join(path.dirname(mainPath), 'index.min.js');
                }

                if (fs.existsSync(msgpackrPath)) {
                    msgpackrSource = fs.readFileSync(msgpackrPath, 'utf8');
                } else {
                    console.warn('[Lotus] msgpackr minified source not found at:', msgpackrPath);
                }
            }
        } catch (e) {
            console.warn('[Lotus] Could not load msgpackr source for renderer:', e.message);
        }

        const getUnpacker = (clientId) => {
            if (!globalUnpackers.has(clientId)) {
                globalUnpackers.set(clientId, new msgpackr.Unpackr({ useRecords: false }));
            }
            return globalUnpackers.get(clientId);
        };

        const resetClientDictionaries = (clientId) => {
            const [windowId] = clientId.split(':');
            if (isProfiling) console.log(`[PROFILE] Resetting msgpackr dictionaries for window ${windowId} and its panes`);
            
            // Purge ALL dictionaries associated with this window (main and panes)
            for (const key of globalPackers.keys()) {
                if (key === windowId || key.startsWith(`${windowId}:`)) {
                    globalPackers.delete(key);
                }
            }
            for (const key of globalUnpackers.keys()) {
                if (key === windowId || key.startsWith(`${windowId}:`)) {
                    globalUnpackers.delete(key);
                }
            }
        };

        const assembler = new MessageAssembler((clientId, fullBuffer, unpacker) => {
            try {
                const msg = unpacker.unpack(fullBuffer);
                const [windowId, rawPaneId] = clientId.split(':');
                const targetPaneId = rawPaneId || (msg && msg.pane_id) || 'main';
                handleProcessedMsg(windowId, targetPaneId, msg);
            } catch (e) {
                console.error(`[lotus] Failed to unpack reassembled message for ${clientId}:`, e);
            }
        });

        const handleProcessedMsg = (windowId, paneId, msg) => {
            const actualPaneId = (msg && msg.pane_id) || paneId || 'main';
            const clientId = `${windowId}:${actualPaneId}`;

            // Re-attach window_id and pane_id for compatibility with existing emitters/handlers
            if (msg && typeof msg === 'object') {
                msg.window_id = windowId;
                msg.pane_id = actualPaneId;
            }

            // Handle app-ready event
            if (msg.event === 'app-ready') {
                if (isProfiling) console.log(`[PROFILE] App/IPC Ready event received.`);
                ipcServerPort = msg.ipc_port || ipcServerPort;
                ipcMain.emit('ready', msg);
                return;
            }

            // All other events should have a window_id
            const win = windows.get(windowId);
            
            // If the window isn't in our map yet, or if there's already a queue being drained,
            // we MUST queue the event to ensure it's not processed before the application 
            // has a chance to attach its listeners (e.g. win.on('ready')).
            if (windowId && (!win || eventQueue.has(windowId))) {
                if (!eventQueue.has(windowId)) eventQueue.set(windowId, []);
                eventQueue.get(windowId).push(msg);
                return;
            }

            // Intercept Renderer-initiated 'ready' events sent via batched array.
            // These signal DOM readiness and require a msgpackr dictionary reset.
            if (Array.isArray(msg) && msg.length > 0) {
                msg.forEach(m => {
                    if (Array.isArray(m) && m[0] === 'ready') {
                        resetClientDictionaries(clientId);
                        if (win) {
                            win.emit('ready', m[1]);
                            win.emit('dom-ready', m[1]);
                        }
                    }
                });
            }

            processEvent(win, msg);
        };

        if (process.platform === 'linux' && !process.env.LOTUS_SYSTEM_THEME) {
            const detected = detectLinuxThemeNode();
            if (detected) {
                process.env.LOTUS_SYSTEM_THEME = detected;
                systemTheme = detected;
                if (detected === 'dark' && !process.env.GTK_THEME) {
                    process.env.GTK_THEME = 'Adwaita:dark';
                }
            }
        }

        function handleInboundInvoke(clientId, json) {
            if (!json || json.type !== 'invoke') return;
            const targetClientId = clientId || 'main-window:main';
            let surface = hostIpcRouter.surfaceTracker.getSurface(targetClientId);
            if (!surface && !targetClientId.includes(':')) {
                surface = hostIpcRouter.surfaceTracker.getSurface(`${targetClientId}:main`);
            }

            if (surface && surface.secure) {
                if (!hostIpcRouter.track2Methods.has(json.method)) {
                    const errMessage = `E_METHOD_NOT_FOUND: Method "${json.method}" is not authorized for sandboxed surface`;
                    const replyEnvelope = {
                        type: 'reply',
                        id: json.id,
                        error: errMessage
                    };
                    const jsonBuf = Buffer.from(JSON.stringify(replyEnvelope));
                    if (typeof dispatchToSurface === 'function') {
                        try { dispatchToSurface(surface.surfaceId, jsonBuf); } catch (_) {}
                    }
                    ipcMain.emit('security-violation', {
                        surfaceId: targetClientId,
                        method: json.method,
                        error: errMessage,
                        payload: json.payload
                    });
                    return;
                }
            }

            const payload = Object.assign({}, json.payload || {});
            payload._replyId = json.id;
            payload._documentId = json.documentId;
            ipcMain.emit(json.method, payload, clientId);
        }

        globalApp = new App((clientId, messages) => {
            if (!msgpackr) return;
            const unpacker = getUnpacker(clientId);
            const [windowId, rawPaneId] = clientId.split(':');
            
            for (const data of messages) {
                try {
                    const type = data[0];
                    if (type === MSG_TYPE_DATA) {
                        try {
                            const msg = unpacker.unpack(data.subarray(1));
                            const targetPaneId = rawPaneId || (msg && msg.pane_id) || 'main';
                            handleProcessedMsg(windowId, targetPaneId, msg);
                        } catch (unpackErr) {
                            if (data[1] === 0x7B) {
                                const json = JSON.parse(data.subarray(1).toString('utf8'));
                                handleInboundInvoke(clientId, json);
                            } else {
                                throw unpackErr;
                            }
                        }
                    } else if (type === MSG_TYPE_CHUNK) {
                        assembler.handleChunk(clientId, data, unpacker);
                    } else if (type === 0x7B) { // '{' character - Track 2 JSON frame
                        try {
                            const json = JSON.parse(data.toString('utf8'));
                            handleInboundInvoke(clientId, json);
                        } catch (e) {
                            console.error(`[lotus] Failed to process Track 2 JSON frame for ${clientId}:`, e);
                        }
                    }
                } catch (e) {
                    console.error(`[lotus] Failed to process event for ${clientId}:`, e);
                }
            }
        }, isProfiling, appIdentifier, msgpackrSource);
    }
    return globalApp;
}

function processEvent(win, msg) {
    const windowId = msg.window_id;
    const paneId = msg.pane_id;

    if (msg.event === 'app-ready') {
        if (msg.system_theme) {
            systemTheme = msg.system_theme;
        }
        return;
    }

    if (msg.event === 'theme-changed') {
        if (msg.theme) {
            systemTheme = msg.theme;
            for (const w of windows.values()) {
                if (w.selectMenuOptions && (w.selectMenuOptions.theme === 'auto' || !w.selectMenuOptions.theme)) {
                    w.setSelectMenuTheme('auto');
                }
            }
        }
        return;
    }

    if (msg.event === '_internal-created') {
        if (isProfiling) {
            const time = Date.now() - startTime;
            console.log(`[PROFILE] Window ${windowId} NATIVE CREATED event received by JS after ${time}ms`);
        }
        if (win) {
            if (typeof msg.maximized === 'boolean') {
                win.maximized = msg.maximized;
            }
            // Emit a synthetic resize with the ACTUAL window dimensions immediately after
            // the window handle is created. This corrects stale pane rects when restoreState
            // restores the window to a different size than the hardcoded initial values.
            if (msg.logicalWidth && msg.logicalHeight) {
                win._logicalWidth = msg.logicalWidth;
                win._logicalHeight = msg.logicalHeight;
                if (msg.width && msg.logicalWidth > 0) {
                    win._scaleFactor = msg.width / msg.logicalWidth;
                }
                const resizePayload = {
                    width: msg.width,
                    height: msg.height,
                    logicalWidth: msg.logicalWidth,
                    logicalHeight: msg.logicalHeight,
                    maximized: win.maximized
                };
                win.emit('resize', resizePayload);
                win.emit('resized', resizePayload);
            }
        }
        return;
    }

    if (msg.event === 'select-element-open') {
        if (win) {
            win._handleSelectOpen(msg);
        }
        return;
    }

    if (msg.event === 'select-element-close') {
        if (win) {
            win._handleSelectClose(msg);
        }
        return;
    }

    if (msg.event === 'load-status') {
        const pane = win ? win.panes.get(paneId) : null;
        const surfaceId = `${msg.window_id}:${paneId}`;

        if (win && pane) {
            if (msg.status === 'started') {
                const targetUrl = msg.url || pane._pendingUrl || 'about:blank';
                // Q3: Page-initiated navigation carries target URL from engine.
                // If no navigation was pending or a new target URL arrived, start new provisional navigation.
                if (!pane._pendingNavId || (msg.url && msg.url !== pane._pendingUrl)) {
                    const navId = `nav-${crypto.randomBytes(8).toString('hex')}`;
                    pane._pendingNavId = navId;
                    pane._pendingUrl = targetUrl;
                    hostIpcRouter.surfaceTracker.startNavigation(surfaceId, navId, targetUrl);
                }
            } else if (msg.status === 'complete') {
                const finalUrl = msg.url;
                const navId = pane._pendingNavId;
                pane._pendingNavId = null;
                pane._pendingUrl = null;

                try {
                    // Q1: Commit only from the engine's reported finalUrl; fail closed if missing.
                    if (!finalUrl) {
                        const err = new Error('E_NAVIGATION_NO_URL: Engine reported no committed URL on complete');
                        err.code = 'E_NAVIGATION_NO_URL';
                        throw err;
                    }
                    hostIpcRouter.surfaceTracker.commitNavigation(surfaceId, navId, finalUrl);
                } catch (e) {
                    // Q2 (Attack 2): On any commit failure, fail closed!
                    // Revoke channels, revoke trusted origins, drop eligibility, rotate documentId
                    const surface = hostIpcRouter.surfaceTracker.getSurface(surfaceId);
                    if (surface) {
                        surface.navigationState = 'failed';
                        surface.ipcEligible = false;
                        surface.allowedChannels = new Set();
                        surface.trustedOrigins = new Set();
                        surface.activeDocumentId = `doc-failed-${crypto.randomBytes(8).toString('hex')}`;
                        surface.bootstrapSecret = crypto.randomBytes(32).toString('hex');
                    }
                }
            } else if (msg.status === 'failed') {
                pane._pendingNavId = null;
                pane._pendingUrl = null;
                hostIpcRouter.surfaceTracker.failNavigation(surfaceId);
            }
        }

        if (win) {
            // Do not emit public window-level load-status for internal framework panes (__lotus_select_overlay, etc.)
            if (!paneId || !paneId.startsWith('__lotus_')) {
                win.emit('load-status', msg.status, paneId);
            }
            if (pane) pane.emit('load-status', msg.status);
        }
        return;
    }

    if (msg.event === 'title-changed') {
        if (win) {
            if (!paneId || !paneId.startsWith('__lotus_')) {
                win.emit('title-changed', msg.title, paneId);
            }
            const pane = win.panes.get(paneId);
            if (pane) pane.emit('title-changed', msg.title);
        }
        return;
    }

    if (msg.event === 'ready-to-show') {
        if (win) {
            win.emit('ready-to-show');
            win.emit('frame-ready'); // backward-compat alias
        }
        return;
    }

    if (msg.event === 'remote-frame-composited') {
        if (win && paneId) {
            const pane = win.panes.get(paneId);
            if (pane) pane.emit('remote-frame-composited', msg.pub_seq);
        }
        return;
    }

    if (msg.event === 'window-closed') {
        if (win) {
            for (const [p, paneObj] of win.panes.entries()) {
                const surfaceId = `${windowId}:${p}`;
                if (paneObj && paneObj.isRemote) {
                    defaultSecureConstellation.releaseSecureEngine(surfaceId);
                }
                hostIpcRouter.surfaceTracker.removeSurface(surfaceId);
            }
            windows.delete(windowId);
            win.emit('close');
            win.emit('closed');
        }
        if (windows.size === 0) {
            defaultSecureConstellation.terminateAll().catch(() => {});
            if (globalApp && typeof globalApp.quit === 'function') {
                globalApp.quit();
            }
        }
        return;
    }

    if (msg.event === 'moved') {
        if (win) win.emit('moved', { x: msg.x, y: msg.y });
        return;
    }

    if (msg.event === 'focused') {
        if (win) win.emit('focus');
        return;
    }

    if (msg.event === 'unfocused') {
        if (win) win.emit('blur');
        return;
    }

    if (msg.event === 'file-hover') {
        if (win) win.emit('file-hover', { path: msg.path });
        return;
    }

    if (msg.event === 'file-hover-cancelled') {
        if (win) win.emit('file-hover-cancelled');
        return;
    }

    if (msg.event === 'file-drop') {
        if (win) win.emit('file-drop', { path: msg.path });
        return;
    }

    if (msg.event === 'resized') {
        if (win) {
            if (typeof msg.maximized === 'boolean') {
                win.maximized = msg.maximized;
            }
            if (msg.logicalWidth && msg.logicalHeight) {
                win._logicalWidth = msg.logicalWidth;
                win._logicalHeight = msg.logicalHeight;
                if (msg.width && msg.logicalWidth > 0) {
                    win._scaleFactor = msg.width / msg.logicalWidth;
                }
            }
            const payload = {
                width: msg.width,
                height: msg.height,
                logicalWidth: msg.logicalWidth,
                logicalHeight: msg.logicalHeight,
                maximized: win.maximized
            };
            
            // Resize throttle: emit immediately (leading edge), then at most once
            // per THROTTLE_MS during active resizing, plus a final flush when resizing stops.
            const THROTTLE_MS = 25;
            const now = Date.now();
            
            win._pendingResize = payload;
            clearTimeout(win._resizeFlushTimer);
            
            if (now - (win._lastResizeEmitTime || 0) >= THROTTLE_MS) {
                win._lastResizeEmitTime = now;
                win.emit('resize', payload);
                win.emit('resized', payload); // backward-compat alias
                win._pendingResize = null;
            }
            
            win._resizeFlushTimer = setTimeout(() => {
                if (win._pendingResize) {
                    win._lastResizeEmitTime = Date.now();
                    win.emit('resize', win._pendingResize);
                    win.emit('resized', win._pendingResize);
                    win._pendingResize = null;
                }
            }, THROTTLE_MS);
        }
        return;
    }

    // Legacy IPC Message routing (plain array)
    if (Array.isArray(msg)) {
        // Check if it's a batch message (array of [channel, data])
        msg.forEach(m => {
            if (Array.isArray(m) && m.length === 2) {
                if (m[0] === 'lotus:set-drag-regions') {
                    if (win) {
                        win.updateDragRegions(m[1]);
                    }
                } else {
                    // Pass the full clientId (windowId:paneId) to allow targeted replies
                    // in IpcMain.handle() if this is an invoke() call.
                    const clientId = msg.pane_id ? `${msg.window_id}:${msg.pane_id}` : msg.window_id;
                    ipcMain.emit(m[0], m[1], clientId);
                }
            }
        });
        return;
    }
}

const Anchor = {
    None: 0,
    Fill: 1,
    Left: 2,
    Right: 3,
    Top: 4,
    Bottom: 5
};

const _napiWarnedOnce = new Set();

function _callNapiOptional(handle, method, ...args) {
    const fn = handle && handle[method];
    if (typeof fn === 'function') {
        fn.apply(handle, args);
        return true;
    }
    if (!_napiWarnedOnce.has(method)) {
        _napiWarnedOnce.add(method);
        console.warn(`[Lotus] Native ${method} is not available yet; layout mutation was a no-op.`);
    }
    return false;
}

function _resolveLayoutCallbackArgs(optsOrFn, maybeFn) {
    if (typeof optsOrFn === 'function') {
        return { containerOpts: {}, fn: optsOrFn };
    }
    if (typeof optsOrFn === 'object' && optsOrFn !== null) {
        return {
            containerOpts: optsOrFn,
            fn: typeof maybeFn === 'function' ? maybeFn : () => {},
        };
    }
    return { containerOpts: {}, fn: typeof maybeFn === 'function' ? maybeFn : () => {} };
}

function _splitChildFieldsFromOpts(opts, direction) {
    const o = opts || {};
    let size;
    if (o.size !== undefined) {
        size = { kind: 'fixed', value: Number(o.size) };
    } else if (o.weight !== undefined) {
        size = { kind: 'weight', value: Number(o.weight) };
    } else if (direction === 'horizontal' && o.width !== undefined) {
        size = { kind: 'fixed', value: Number(o.width) };
    } else if (direction === 'vertical' && o.height !== undefined) {
        size = { kind: 'fixed', value: Number(o.height) };
    } else {
        size = { kind: 'weight', value: 1 };
    }
    return {
        size,
        min: o.min !== undefined ? Number(o.min) : 1,
        max: o.max !== undefined ? Number(o.max) : null,
        resizable: Boolean(o.resizable),
        visible: o.visible !== false,
        detached: Boolean(o.detached),
    };
}

function _paneOptsFromSizingOpts(opts) {
    if (!opts || typeof opts !== 'object') return {};
    const {
        size, weight, min, max, resizable, detached, width, height,
        ...rest
    } = opts;
    return rest;
}

class LayoutScope {
    constructor(builder, direction, splitId, containerOpts) {
        this._builder = builder;
        this._direction = direction;
        this._splitId = splitId;
        this._containerOpts = containerOpts || {};
        this._children = [];
    }

    pane(id, sizeOrOpts, maybeOpts) {
        let sizingOpts;
        if (typeof sizeOrOpts === 'number') {
            sizingOpts = { size: sizeOrOpts, ...(maybeOpts || {}) };
        } else if (typeof sizeOrOpts === 'object' && sizeOrOpts !== null) {
            sizingOpts = { ...(maybeOpts || {}), ...sizeOrOpts };
        } else {
            sizingOpts = maybeOpts || {};
        }
        const child = _splitChildFieldsFromOpts(sizingOpts, this._direction);
        this._builder._registerTreeLeaf(id, _paneOptsFromSizingOpts(sizingOpts));
        this._children.push({
            ...child,
            node: { type: 'leaf', paneId: id },
        });
        return this;
    }

    row(id, optsOrFn, maybeFn) {
        return this._nestedContainer('horizontal', id, optsOrFn, maybeFn);
    }

    col(id, optsOrFn, maybeFn) {
        return this._nestedContainer('vertical', id, optsOrFn, maybeFn);
    }

    split(id, opts, fn) {
        const direction = opts && opts.direction === 'vertical' ? 'vertical' : 'horizontal';
        const callback = typeof fn === 'function' ? fn : () => {};
        const inner = new LayoutScope(this._builder, direction, id, opts || {});
        callback(inner);
        const splitNode = {
            type: 'split',
            id,
            direction,
            children: inner._children,
        };
        const child = _splitChildFieldsFromOpts(opts || {}, this._direction);
        this._children.push({ ...child, node: splitNode });
        return this;
    }

    _nestedContainer(direction, id, optsOrFn, maybeFn) {
        const { containerOpts, fn } = _resolveLayoutCallbackArgs(optsOrFn, maybeFn);
        const inner = new LayoutScope(this._builder, direction, id, containerOpts);
        fn(inner);
        const splitNode = {
            type: 'split',
            id,
            direction,
            children: inner._children,
        };
        const child = _splitChildFieldsFromOpts(containerOpts, this._direction);
        this._children.push({ ...child, node: splitNode });
        return this;
    }
}

class LayoutBuilder {
    constructor() {
        this._panes = [];
        this._currentDockOrder = 0;
        this._fillTree = null;
        this._fillUsesSplit = false;
    }

    _registerTreeLeaf(id, options = {}) {
        this._panes.push({
            ...options,
            id,
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            anchor: Anchor.None,
            visible: options.visible !== false,
            zIndex: options.zIndex || 0,
        });
    }

    _setRootSplit(direction, id, optsOrFn, maybeFn) {
        const { containerOpts, fn } = _resolveLayoutCallbackArgs(optsOrFn, maybeFn);
        if (this._fillTree && this._fillUsesSplit) {
            console.warn('[Lotus] LayoutBuilder: replacing previous Fill-slot split');
        }
        const scope = new LayoutScope(this, direction, id, containerOpts);
        fn(scope);
        this._fillTree = {
            type: 'split',
            id,
            direction,
            children: scope._children,
        };
        this._fillUsesSplit = true;
        return this;
    }

    row(id, optsOrFn, maybeFn) {
        return this._setRootSplit('horizontal', id, optsOrFn, maybeFn);
    }

    col(id, optsOrFn, maybeFn) {
        return this._setRootSplit('vertical', id, optsOrFn, maybeFn);
    }

    split(id, opts, fn) {
        const direction = opts && opts.direction === 'vertical' ? 'vertical' : 'horizontal';
        const callback = typeof fn === 'function' ? fn : () => {};
        if (this._fillTree && this._fillUsesSplit) {
            console.warn('[Lotus] LayoutBuilder: replacing previous Fill-slot split');
        }
        const scope = new LayoutScope(this, direction, id, opts || {});
        callback(scope);
        this._fillTree = {
            type: 'split',
            id,
            direction,
            children: scope._children,
        };
        this._fillUsesSplit = true;
        return this;
    }

    _buildLayoutTree() {
        const dockChildren = [];
        const docked = this._panes
            .filter(p => p.anchor !== undefined && p.anchor !== Anchor.None)
            .slice()
            .sort((a, b) => (a.dockOrder || 0) - (b.dockOrder || 0));

        let fillDocked = false;
        for (const p of docked) {
            if (p.anchor === Anchor.Fill) {
                fillDocked = true;
                if (this._fillUsesSplit && this._fillTree) {
                    dockChildren.push({
                        anchor: Anchor.Fill,
                        node: this._fillTree,
                        size: { kind: 'weight', value: 1 },
                        min: p.min !== undefined ? Number(p.min) : 1,
                        max: p.max !== undefined ? Number(p.max) : null,
                        resizable: Boolean(p.resizable),
                        visible: p.visible !== false,
                        detached: false,
                    });
                    continue;
                }
            }

            let fixedVal = 0;
            if (p.anchor === Anchor.Left || p.anchor === Anchor.Right) {
                fixedVal = Math.max(1, Number(p.width) || 0);
            } else if (p.anchor === Anchor.Top || p.anchor === Anchor.Bottom) {
                fixedVal = Math.max(1, Number(p.height) || 0);
            }

            const node = p.anchor === Anchor.Fill
                ? { type: 'leaf', paneId: p.id }
                : { type: 'leaf', paneId: p.id };

            dockChildren.push({
                anchor: p.anchor,
                node,
                size: p.anchor === Anchor.Fill
                    ? { kind: 'weight', value: 1 }
                    : { kind: 'fixed', value: fixedVal },
                min: p.min !== undefined ? Number(p.min) : 1,
                max: p.max !== undefined ? Number(p.max) : null,
                resizable: Boolean(p.resizable),
                visible: p.visible !== false,
                detached: false,
            });
        }

        if (this._fillUsesSplit && this._fillTree && !fillDocked) {
            dockChildren.push({
                anchor: Anchor.Fill,
                node: this._fillTree,
                size: { kind: 'weight', value: 1 },
                min: 1,
                max: null,
                resizable: false,
                visible: true,
                detached: false,
            });
        }

        return { type: 'dockRoot', children: dockChildren };
    }

    left(id, width, options = {}) {
        this._panes.push({
            ...options,
            id,
            width,
            x: 0, y: 0, height: 0, // Placeholder
            anchor: Anchor.Left,
            dockOrder: this._currentDockOrder++
        });
        return this;
    }

    right(id, width, options = {}) {
        this._panes.push({
            ...options,
            id,
            width,
            x: 0, y: 0, height: 0, // Placeholder
            anchor: Anchor.Right,
            dockOrder: this._currentDockOrder++
        });
        return this;
    }

    top(id, height, options = {}) {
        this._panes.push({
            ...options,
            id,
            height,
            x: 0, y: 0, width: 0, // Placeholder
            anchor: Anchor.Top,
            dockOrder: this._currentDockOrder++
        });
        return this;
    }

    bottom(id, height, options = {}) {
        this._panes.push({
            ...options,
            id,
            height,
            x: 0, y: 0, width: 0, // Placeholder
            anchor: Anchor.Bottom,
            dockOrder: this._currentDockOrder++
        });
        return this;
    }

    fill(id, options = {}) {
        this._panes.push({
            ...options,
            id,
            x: 0, y: 0, width: 0, height: 0, // Placeholder
            anchor: Anchor.Fill,
            dockOrder: this._currentDockOrder++
        });
        return this;
    }

    absolute(id, x, y, width, height, options = {}) {
        this._panes.push({
            ...options,
            id,
            x,
            y,
            width,
            height,
            anchor: Anchor.None,
            dockOrder: this._currentDockOrder++
        });
        return this;
    }

    config() {
        return {
            panes: this._panes,
            layout: this._buildLayoutTree(),
        };
    }
}

class Pane extends EventEmitter {
    constructor(window, id, visible = true) {
        super();
        this.window = window;
        this.id = id;
        this.surfaceId = `${window.id}:${id}`;
        this.visible = Boolean(visible);
        this._pendingNavId = null;
        this._pendingUrl = null;
        this.isRemote = false;
        this.isIsolated = false;
        this._engineInstance = null;
        this._shm = null;
        this._url = 'about:blank';
        this._layout = {
            anchor: Anchor.None,
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            zIndex: 0,
            dockOrder: 0,
        };
        this._crashed = null;
        this.autoRespawn = false;
        this.autoRespawnMaxAttempts = 3;
        this._autoRespawnFailures = 0;
        this._autoRespawnHealthyTimer = null;
    }

    loadUrl(url) {
        this._url = url;
        const navId = `nav-${crypto.randomBytes(8).toString('hex')}`;
        this._pendingNavId = navId;
        this._pendingUrl = url;
        hostIpcRouter.surfaceTracker.startNavigation(this.surfaceId, navId, url);
        if (this.isRemote && this._engineInstance) {
            try {
                this._engineInstance.controlStream.write(JSON.stringify({
                    type: 'Navigate',
                    surface_id: this.surfaceId,
                    navigation_id: 1,
                    url,
                }) + '\n');
            } catch {}
        } else {
            this.window.handle.loadUrlInPane(this.id, url);
        }
    }

    sendInput(event) {
        if (this.isRemote && this._engineInstance) {
            try {
                this._engineInstance.controlStream.write(JSON.stringify({
                    type: 'Input',
                    event,
                }) + '\n');
            } catch {}
        }
    }

    executeScript(script) {
        this.window.handle.executeScriptInPane(this.id, script);
    }

    setRect(x, y, width, height) {
        if (this._layout) {
            this._layout.x = x;
            this._layout.y = y;
            this._layout.width = width;
            this._layout.height = height;
            // Rust SetPaneRect permanently undocks the pane; keep JS in sync.
            this._layout.anchor = Anchor.None;
        }
        this.window.handle.setPaneRect(this.id, x, y, width, height);
    }

    get zIndex() {
        return this._layout ? this._layout.zIndex : 0;
    }

    set zIndex(val) {
        const z = Number(val) || 0;
        if (this._layout) {
            this._layout.zIndex = z;
        }
        if (this.window && this.window.handle && typeof this.window.handle.setPaneZIndex === 'function') {
            this.window.handle.setPaneZIndex(this.id, z);
        }
    }

    /**
     * Update dock thickness without clearing the pane's anchor.
     * Left/Right use width; Top/Bottom use height; Fill/None are no-ops.
     * Accepts a single scalar for the consumed axis: pane.setSize(240).
     */
    setSize(width, height) {
        const anchor = this._layout ? this._layout.anchor : Anchor.None;
        if (anchor === Anchor.Fill || anchor === Anchor.None) {
            return;
        }

        const w = Number(width);
        const h = height === undefined ? w : Number(height);
        const safeW = Number.isFinite(w) ? w : 0;
        const safeH = Number.isFinite(h) ? h : 0;

        if (this._layout) {
            if (anchor === Anchor.Left || anchor === Anchor.Right) {
                this._layout.width = Math.max(1, safeW);
            } else if (anchor === Anchor.Top || anchor === Anchor.Bottom) {
                this._layout.height = Math.max(1, safeH);
            }
        }

        const apply = () => {
            if (typeof this.window.handle.setPaneSize === 'function') {
                this.window.handle.setPaneSize(this.id, safeW, safeH);
            }
        };
        if (this.window._layoutBatchDepth > 0) {
            this.window._queueLayoutMutation(apply);
        } else {
            apply();
        }
    }

    setWeight(weight) {
        const w = Number(weight);
        const safeW = Number.isFinite(w) ? Math.max(0, w) : 0;
        const apply = () => _callNapiOptional(this.window.handle, 'setPaneWeight', this.id, safeW);
        if (this.window._layoutBatchDepth > 0) {
            this.window._queueLayoutMutation(apply);
        } else {
            apply();
        }
    }

    remove() {
        this.window.removePane(this.id);
    }

    setVisible(visible) {
        const want = Boolean(visible);
        this.visible = want;
        // Crash / respawn cover mode: remote stays compositor-visible (vsync) under chrome.
        if (this.isRemote && (this._crashed || this._respawning)) {
            const crash = this.window.panes.get(crashChromePaneId(this.id));
            if (want && crash) {
                coverRemoteWithCrashChrome(this.window, this, crash);
            } else {
                this.window.handle.setPaneVisible(this.id, false);
                if (crash) {
                    crash.visible = false;
                    crash.window.handle.setPaneVisible(crash.id, false);
                }
            }
            return;
        }
        this.window.handle.setPaneVisible(this.id, this.visible);
    }

    /** Skip hit-testing so clicks fall through (atrium crosshair / game-mode overlay-only). */
    setPassthrough(passthrough) {
        this.pointerPassthrough = !!passthrough;
        if (this.window?.handle && typeof this.window.handle.setPanePassthrough === 'function') {
            this.window.handle.setPanePassthrough(this.id, this.pointerPassthrough);
        }
    }

    isVisible() {
        return this.visible;
    }

    toggleVisible() {
        this.setVisible(!this.visible);
        return this.visible;
    }

    focus() {
        this.window.handle.focusPane(this.id);
    }

    updateDragRegions(rects) {
        this.window.handle.updatePaneDragRegions(this.id, JSON.stringify(rects));
    }
}

class ServoWindow extends EventEmitter {
    constructor(options = {}) {
        super();
        ensureApp();

        if (typeof options === 'string') {
            options = { initialUrl: options };
        }

        // Normalize Electron-style 'frame: false' to Lotus-style 'frameless: true'.
        // The underlying Rust option is 'frameless'; 'frame' is silently ignored otherwise.
        if (options.frame !== undefined && options.frameless === undefined) {
            options = { ...options, frameless: !options.frame };
            delete options.frame;
        }

        const defaultOptions = {
            width: 1024,
            height: 768,
            maximized: false,
            fullscreen: false,
            title: "Lotus",
            resizable: true,
            frameless: false,
            alwaysOnTop: false,
            restoreState: true,
            root: undefined,
            index: 'index.html',
            transparent: false,
            cornerRadius: undefined,
            visible: true,
            autoResizeMain: true,
            panes: []
        };

        const finalOptions = { ...defaultOptions, ...options };

        if (finalOptions.root) {
            // Validate root path
            const path = require('path');
            if (!path.isAbsolute(finalOptions.root)) {
                console.warn(`[Lotus] Warning: 'root' path should be absolute: ${finalOptions.root}`);
                finalOptions.root = path.resolve(finalOptions.root);
            }

            // Force POSIX-style paths
            finalOptions.root = finalOptions.root.replace(/\\/g, '/');

            // If using root, initialUrl is automatically set to our custom protocol
            finalOptions.initialUrl = `lotus-resource://localhost/${finalOptions.index}`;
        }
        
        // If the user provided panes, ensure they are in the format Rust expects
        if (finalOptions.panes && Array.isArray(finalOptions.panes) && finalOptions.panes.length > 0) {
            finalOptions.panes = finalOptions.panes.map(p => ({
                ...p,
                id: p.id,
                url: p.url || 'about:blank',
                x: p.x || 0,
                y: p.y || 0,
                width: p.width || 0,
                height: p.height || 0,
                zIndex: p.zIndex || 0,
                visible: p.visible !== false,
                anchor: p.anchor !== undefined ? p.anchor : Anchor.None,
                dockOrder: p.dockOrder || 0
            }));
        } else {
            // UNIFIED PIPELINE: A single window is just a multi-pane layout with 1 Fill pane ('main').
            finalOptions.panes = [{
                id: 'main',
                url: finalOptions.initialUrl || 'about:blank',
                x: 0,
                y: 0,
                width: finalOptions.width || 0,
                height: finalOptions.height || 0,
                zIndex: 0,
                visible: true,
                anchor: finalOptions.autoResizeMain ? Anchor.Fill : Anchor.None,
                dockOrder: 0,
                sandbox: finalOptions.sandbox,
                secure: finalOptions.secure,
                isolated: finalOptions.isolated,
                allowedChannels: finalOptions.allowedChannels,
                trustedOrigins: finalOptions.trustedOrigins,
            }];
        }

        // Parse & normalize scrollbars options
        let scrollbarsEnabled = true;
        let scrollbarTheme = 'auto';
        let scrollbarAutoHide = true;
        let scrollbarWidth = 8;

        if (finalOptions.scrollbars === false) {
            scrollbarsEnabled = false;
        } else if (typeof finalOptions.scrollbars === 'object' && finalOptions.scrollbars !== null) {
            if (finalOptions.scrollbars.enabled === false) {
                scrollbarsEnabled = false;
            }
            if (finalOptions.scrollbars.theme) scrollbarTheme = String(finalOptions.scrollbars.theme);
            if (finalOptions.scrollbars.autoHide !== undefined) scrollbarAutoHide = Boolean(finalOptions.scrollbars.autoHide);
            if (finalOptions.scrollbars.width !== undefined) scrollbarWidth = Math.max(2, parseInt(finalOptions.scrollbars.width, 10) || 8);
        }

        finalOptions.scrollbarsEnabled = scrollbarsEnabled;
        finalOptions.scrollbarTheme = scrollbarTheme;
        finalOptions.scrollbarAutoHide = scrollbarAutoHide;
        finalOptions.scrollbarWidth = scrollbarWidth;

        if (typeof globalThis !== 'undefined' && globalThis.__LOTUS_ENCRYPTED_BUILD__ && !vfsInitialized) {
            app.initVfs({ required: true });
        }

        validatePlatformSecurityIsolation(finalOptions);
        if (Array.isArray(finalOptions.panes)) {
            for (const p of finalOptions.panes) {
                if (p && typeof p === 'object') {
                    validatePlatformSecurityIsolation(p);
                }
            }
        }

        if (finalOptions.allowLocalhost !== undefined) {
            defaultSecureConstellation.allowLocalhost = Boolean(finalOptions.allowLocalhost);
        }

        const inProcessPanes = [];
        const sandboxedPanes = [];
        if (Array.isArray(finalOptions.panes)) {
            for (const p of finalOptions.panes) {
                const isSecure = !p.id.startsWith('__lotus_') && (p.sandbox === true || p.secure === true || (p.sandbox !== false && p.secure !== false && (finalOptions.sandbox === true || finalOptions.secure === true)));
                if (isSecure) {
                    sandboxedPanes.push(p);
                } else {
                    inProcessPanes.push(p);
                }
            }
        }

        // Step 1: If any non-isolated sandboxed pane will be used, immediately pre-warm
        // the sandboxed engine into a ready wait-state in parallel while the host window
        // and in-process panes are constructed.
        // Isolated panes intentionally do not keep a hot spare — a full lotus-engine is
        // hundreds of MB; crash recovery force-kills and cold-spawns instead.
        if (sandboxedPanes.length > 0) {
            const hasSharedSandboxedPane = sandboxedPanes.some(
                p => !(p.isolated === true || (p.isolated !== false && finalOptions.isolated === true))
            );
            if (hasSharedSandboxedPane && typeof defaultSecureConstellation.prewarm === 'function') {
                defaultSecureConstellation.prewarm();
            }
        }

        let rustOptions = (sandboxedPanes.length > 0 && inProcessPanes.length > 0)
            ? { ...finalOptions, panes: inProcessPanes }
            : { ...finalOptions };
        if (rustOptions.layout && typeof rustOptions.layout === 'object') {
            rustOptions = { ...rustOptions, layout: JSON.stringify(rustOptions.layout) };
        }

        this.handle = createWindow(rustOptions);
        this.id = this.handle.getId();
        
        // Initialize the event queue for this window immediately.
        // This ensures that all events arriving during the constructor are queued
        // until the setImmediate drain at the end, allowing the caller to attach 
        // listeners before any events fire.
        if (!eventQueue.has(this.id)) eventQueue.set(this.id, []);

        this.panes = new Map();
        this._batchQueue = [];
        this._batchBytes = 0;
        this._paneBatchQueues = new Map();
        this._paneBatchBytes = new Map();
        this._batchTimer = null;
        this._layoutBatchDepth = 0;
        this._layoutBatchQueue = [];
        this._layout = finalOptions.layout || null;

        
        // Register all panes created atomically with the window
        if (sandboxedPanes.length > 0 && inProcessPanes.length > 0) {
            inProcessPanes.forEach(p => {
                this.panes.set(p.id, new Pane(this, p.id, p.visible !== false));
            });
        } else if (finalOptions.panes) {
            finalOptions.panes.forEach(p => {
                this.panes.set(p.id, new Pane(this, p.id, p.visible !== false));
            });
        }

        if (!this.panes.has('main')) {
            const mainPane = new Pane(this, 'main', true);
            mainPane._layout = {
                anchor: Anchor.Fill,
                x: 0,
                y: 0,
                width: finalOptions.width || 0,
                height: finalOptions.height || 0,
                zIndex: 0,
                dockOrder: 0,
            };
            this.panes.set('main', mainPane);
        }

        this._logicalWidth = finalOptions.width;
        this._logicalHeight = finalOptions.height;
        this.maximized = Boolean(finalOptions.maximized);
        this._selectOverlayReady = false;
        this._pendingSelectPayload = null;

        let initialSelectTheme = 'auto';
        if (finalOptions.selectMenu && finalOptions.selectMenu.theme !== undefined) {
            initialSelectTheme = finalOptions.selectMenu.theme;
        } else if (globalApp && typeof globalApp.getSystemTheme === 'function') {
            systemTheme = globalApp.getSystemTheme();
        }

        this.selectMenuOptions = Object.assign({
            enabled: true,
            url: 'lotus-resource://localhost/select-menu.html',
            css: '',
            theme: initialSelectTheme
        }, finalOptions.selectMenu || {});

        // Pre-warm the system select overlay pane if enabled
        if (this.selectMenuOptions.enabled !== false) {
            this.createPane('__lotus_select_overlay', {
                url: this.selectMenuOptions.url,
                x: 0,
                y: 0,
                width: 200,
                height: 100,
                zIndex: 2147483647,
                visible: false
            });
        }

        this._allowedChannels = finalOptions.allowedChannels || ['vault', 'telemetry'];
        this._trustedOrigins = finalOptions.trustedOrigins || [];
        this._finalOptions = finalOptions;

        // Register each pane as a distinct surface in HostSurfaceTracker
        const panesToTrack = (sandboxedPanes.length > 0 && inProcessPanes.length > 0)
            ? inProcessPanes
            : (finalOptions.panes || []);

        panesToTrack.forEach(p => {
            const surfaceId = `${this.id}:${p.id}`;
            const paneObj = this.panes.get(p.id);
            const isSecure = !p.id.startsWith('__lotus_') && (p.sandbox === true || p.secure === true || (p.sandbox !== false && p.secure !== false && (finalOptions.sandbox === true || finalOptions.secure === true)));
            const isIsolated = isSecure && (p.isolated === true || (p.isolated !== false && finalOptions.isolated === true));
            const paneChannels = Array.from(p.allowedChannels || this._allowedChannels || []);
            const paneOrigins = Array.from(p.trustedOrigins || this._trustedOrigins || []);
            if (paneObj) {
                paneObj.surfaceId = surfaceId;
                paneObj.isIsolated = isIsolated;
                paneObj._allowedChannels = paneChannels;
                paneObj._trustedOrigins = paneOrigins;
                if (p.url) paneObj._url = p.url;
                // Seed layout from declarative options so setSize can see the real anchor.
                // (createPane does this for late-added panes; constructor-registered ones need it too.)
                paneObj._layout = {
                    anchor: p.anchor !== undefined ? p.anchor : Anchor.None,
                    x: p.x || 0,
                    y: p.y || 0,
                    width: p.width || 0,
                    height: p.height || 0,
                    zIndex: p.zIndex || 0,
                    dockOrder: p.dockOrder || 0,
                };
            }

            hostIpcRouter.surfaceTracker.createSurface(surfaceId, {
                secure: isSecure,
                allowedChannels: paneChannels,
                trustedOrigins: paneOrigins
            });

            const initNavId = `nav-init-${crypto.randomBytes(8).toString('hex')}`;
            if (paneObj) {
                paneObj._pendingNavId = initNavId;
                paneObj._pendingUrl = p.url || 'about:blank';
            }
            hostIpcRouter.surfaceTracker.startNavigation(surfaceId, initNavId, p.url || 'about:blank');
        });

        // Spawn and dock any declarative sandboxed panes
        if (sandboxedPanes.length > 0 && inProcessPanes.length > 0) {
            for (const sp of sandboxedPanes) {
                this.createPane(sp.id, sp);
            }
        }

        windows.set(this.id, this);

        // Drain any events that arrived while we were in the constructor.
        // Deferring this to the next tick ensures the constructor has returned
        // and the application has had a chance to attach its listeners (e.g. win.on('ready')).
        if (eventQueue.has(this.id)) {
            setImmediate(() => {
                const pending = eventQueue.get(this.id);
                if (pending) {
                    if (isProfiling) console.log(`[PROFILE] Draining ${pending.length} pending events for window ${this.id}`);
                    pending.forEach(msg => processEvent(this, msg));
                }
                eventQueue.delete(this.id);
            });
        }
    }

    /** Backward compatibility: loadUrl targets the 'main' pane */
    loadUrl(url) {
        const main = this.panes.get('main');
        if (main) main.loadUrl(url);
    }

    /** Create a new named pane */
    createPane(id, options = {}) {
        validatePlatformSecurityIsolation(options);
        if (this.panes.has(id)) {
            throw new Error(`Pane with id '${id}' already exists`);
        }
        const { url = 'about:blank', x = 0, y = 0, width = 0, height = 0, zIndex = 0, anchor = 0, dockOrder = 0, visible = true, pointerPassthrough = false } = options;
        const isInternalPane = id.startsWith('__lotus_');
        const isSecure = !isInternalPane && (options.sandbox === true || options.secure === true || (options.sandbox !== false && options.secure !== false && this._finalOptions && (this._finalOptions.sandbox === true || this._finalOptions.secure === true)));
        const isIsolated = isSecure && (options.isolated === true || (options.isolated !== false && this._finalOptions && this._finalOptions.isolated === true));
        const surfaceId = `${this.id}:${id}`;

        const pane = new Pane(this, id, visible);
        pane.surfaceId = surfaceId;
        pane.isRemote = isSecure;
        pane.isIsolated = isIsolated;
        pane.autoRespawn = options.autoRespawn === true;
        pane.autoRespawnMaxAttempts = Number.isFinite(options.autoRespawnMaxAttempts)
            ? Math.max(1, Math.floor(options.autoRespawnMaxAttempts))
            : 3;
        pane._autoRespawnFailures = 0;
        pane._url = url;
        pane._allowedChannels = Array.from(
            options.allowedChannels || this._allowedChannels || ['vault', 'telemetry']
        );
        pane._trustedOrigins = Array.from(
            options.trustedOrigins || this._trustedOrigins || []
        );
        pane._layout = {
            anchor,
            x,
            y,
            width,
            height,
            zIndex,
            dockOrder,
        };

        hostIpcRouter.surfaceTracker.createSurface(surfaceId, {
            secure: isSecure,
            allowedChannels: pane._allowedChannels,
            trustedOrigins: pane._trustedOrigins
        });

        const initNavId = `nav-init-${crypto.randomBytes(8).toString('hex')}`;
        pane._pendingNavId = initNavId;
        pane._pendingUrl = url;
        hostIpcRouter.surfaceTracker.startNavigation(surfaceId, initNavId, url);

        if (isSecure && typeof allocateRemotePaneShm === 'function') {
            let targetWidth = width;
            let targetHeight = height;
            if ((!targetWidth || !targetHeight) && anchor !== Anchor.None) {
                let availW = this._logicalWidth || 800;
                let availH = this._logicalHeight || 600;
                if (this.panes) {
                    for (const existingPane of this.panes.values()) {
                        if (existingPane.id === id) continue;
                        const pOpt = this._finalOptions?.panes?.find(p => p.id === existingPane.id);
                        if (!pOpt) continue;
                        if (pOpt.anchor === Anchor.Left || pOpt.anchor === Anchor.Right) {
                            availW -= (pOpt.width || 0);
                        } else if (pOpt.anchor === Anchor.Top || pOpt.anchor === Anchor.Bottom) {
                            availH -= (pOpt.height || 0);
                        }
                    }
                }
                if (!targetWidth) targetWidth = Math.max(1, availW);
                if (!targetHeight) targetHeight = Math.max(1, availH);
            }
            const shmWidth = Math.max(1, targetWidth || this._logicalWidth || 800);
            const shmHeight = Math.max(1, targetHeight || this._logicalHeight || 600);
            // Physical-pixel SHM ceiling with stepped headroom (Phase 3.4).
            // Sealed memfds cannot grow — size for device pixels + step so common
            // dock/maximize growth fits without a 4K×3 (~95 MiB) floor. Never use
            // the old Math.max(..., 3840/2160) CSS-pixel floor.
            const scale = Math.max(
                1,
                Number(this._scaleFactor) ||
                    (typeof this.handle?.scaleFactor === 'number' ? this.handle.scaleFactor : 1) ||
                    1
            );
            const maxShmWidth = stepPlaneDim(Math.ceil(shmWidth * scale));
            const maxShmHeight = stepPlaneDim(Math.ceil(shmHeight * scale));
            const shm = allocateRemotePaneShm(surfaceId, maxShmWidth, maxShmHeight);
            shm.width = shmWidth;
            shm.height = shmHeight;
            shm.maxWidth = maxShmWidth;
            shm.maxHeight = maxShmHeight;
            pane._shm = shm;

            const pixelRoFd = shm.pixelRoFd ?? shm.roFd;
            const ackRwFd = shm.ackRwFd ?? -1;
            this.handle.createRemotePane(id, pixelRoFd, shm.totalSize, ackRwFd, x, y, width, height, zIndex, anchor, dockOrder, visible, maxShmWidth, maxShmHeight);

            defaultSecureConstellation.acquireSecureEngine(surfaceId, {
                pane,
                url,
                shm,
                isolated: isIsolated,
            }).then(engineRes => {
                pane._engineInstance = engineRes.instance;
                if (engineRes.instance.controlFd && this.handle && typeof this.handle.attachRemotePaneInput === 'function') {
                    try {
                        this.handle.attachRemotePaneInput(id, engineRes.instance.controlFd);
                    } catch (e) {
                        console.error('[createPane] attachRemotePaneInput error:', e);
                    }
                }
            }).catch(err => {
                pane.emit('error', err);
            });
        } else {
            this.handle.createPane(id, url, x, y, width, height, zIndex, anchor, dockOrder, visible);
        }

        this.panes.set(id, pane);
        if (pointerPassthrough) {
            pane.setPassthrough(true);
        }

        return pane;
    }

    /**
     * Create an external app pane (nested zero-copy Wayland + Xwayland game surface).
     * @param {string} id Unique pane identifier
     * @param {object} options External app options: command, args, anchor, zIndex, width, height, etc.
     */
    createExternalAppPane(id, options = {}) {
        if (!this.handle) throw new Error("Window not created yet");
        if (this.panes.has(id)) {
            throw new Error(`Pane '${id}' already exists on window '${this.id}'`);
        }

        const visible = options.visible !== undefined ? options.visible : true;
        const pane = new Pane(this, id, visible);
        pane.anchor = options.anchor !== undefined ? options.anchor : Anchor.Fill;
        pane.zIndex = options.zIndex !== undefined ? options.zIndex : 0;
        pane.dockOrder = options.dockOrder !== undefined ? options.dockOrder : 0;
        pane.isExternal = true;

        const command = options.command || 'vkcube';
        const args = Array.isArray(options.args) ? options.args : [];
        const x = options.x || 0;
        const y = options.y || 0;
        const width = options.width || this._logicalWidth || 800;
        const height = options.height || this._logicalHeight || 600;

        const dbus = options.dbus || {};
        const dbusNotifications =
            options.dbusNotifications || dbus.notifications || 'off';
        const dbusNotificationApps = Array.isArray(options.dbusNotificationApps)
            ? options.dbusNotificationApps
            : Array.isArray(dbus.notificationApps)
              ? dbus.notificationApps
              : [];

        if (typeof this.handle.createExternalAppPane === 'function') {
            this.handle.createExternalAppPane(
                id,
                command,
                args,
                x,
                y,
                width,
                height,
                pane.zIndex,
                pane.anchor,
                pane.dockOrder,
                visible,
                dbusNotifications,
                dbusNotificationApps
            );
        } else {
            console.warn('[lotus-atrium] createExternalAppPane not supported on this platform');
        }

        this.panes.set(id, pane);
        if (options.pointerPassthrough) {
            pane.setPassthrough(true);
        }
        return pane;
    }

    /**
     * Set input mode: true = Game mode (pass-through to Wayland/X11 app), false = HUD mode (interact with webview).
     * Nest-ready check runs on the winit loop so it is safe to call immediately after createExternalAppPane.
     * @param {boolean} enabled
     */
    setGameMode(enabled) {
        if (this.handle && typeof this.handle.setGameMode === 'function') {
            this.handle.setGameMode(!!enabled);
        }
    }

    /**
     * Register a window-level host hotkey (beats game/pane focus).
     * @param {object} opts
     * @param {string} opts.id Stable action id (emitted as nest:hotkey)
     * @param {string} opts.key W3C KeyboardEvent.code (e.g. "F12", "KeyD")
     * @param {object} [opts.modifiers] { ctrl?, alt?, shift?, super? }
     */
    registerHostHotkey(opts = {}) {
        if (!this.handle || typeof this.handle.registerHostHotkey !== 'function') {
            return;
        }
        const id = opts.id;
        const key = opts.key;
        if (!id || !key) {
            console.warn('[lotus] registerHostHotkey requires { id, key }');
            return;
        }
        const m = opts.modifiers || {};
        let mask = 0;
        if (m.ctrl || m.control) mask |= 1;
        if (m.alt) mask |= 2;
        if (m.shift) mask |= 4;
        if (m.super || m.meta) mask |= 8;
        this.handle.registerHostHotkey(String(id), String(key), mask);
    }

    unregisterHostHotkey(id) {
        if (this.handle && typeof this.handle.unregisterHostHotkey === 'function') {
            this.handle.unregisterHostHotkey(String(id));
        }
    }

    clearHostHotkeys() {
        if (this.handle && typeof this.handle.clearHostHotkeys === 'function') {
            this.handle.clearHostHotkeys();
        }
    }

    /**
     * Borderless fullscreen for the lotus host window itself.
     * @param {boolean} enabled
     */
    setFullscreen(enabled) {
        if (this.handle && typeof this.handle.setFullscreen === 'function') {
            this.handle.setFullscreen(!!enabled);
        }
    }

    /**
     * Convenience alias for accessing the primary 'main' pane.
     */
    get pane() {
        return this.panes.get('main');
    }

    addPane(id, options = {}) {
        return this.createPane(id, options);
    }

    removePane(id) {
        if (id === 'main') {
            throw new Error("Cannot remove the 'main' pane");
        }
        if (this.panes.has(id)) {
            const pane = this.panes.get(id);
            const surfaceId = `${this.id}:${id}`;
            if (pane && pane.isRemote) {
                defaultSecureConstellation.releaseSecureEngine(surfaceId);
            }
            hostIpcRouter.surfaceTracker.removeSurface(surfaceId);
            this.handle.removePane(id);
            this.panes.delete(id);
        }
    }

    getPane(id) {
        return this.panes.get(id);
    }

    _queueLayoutMutation(fn) {
        if (typeof fn === 'function') {
            this._layoutBatchQueue.push(fn);
        }
    }

    _flushLayoutBatch() {
        // Queued mutations (setPaneSize / setSplitWeights / …) already hit NAPI.
        // beginLayoutBatch/endLayoutBatch bracket the flush so Rust recalculates once.
        const queue = this._layoutBatchQueue || [];
        this._layoutBatchQueue = [];
        for (const fn of queue) {
            try {
                fn();
            } catch (e) {
                console.error('[Lotus] layout batch mutation failed:', e);
            }
        }
    }

    batchLayout(fn) {
        this._layoutBatchDepth++;
        if (this._layoutBatchDepth === 1) {
            _callNapiOptional(this.handle, 'beginLayoutBatch');
        }
        try {
            if (typeof fn === 'function') {
                fn();
            }
        } finally {
            this._layoutBatchDepth = Math.max(0, this._layoutBatchDepth - 1);
            if (this._layoutBatchDepth === 0) {
                try {
                    this._flushLayoutBatch();
                } finally {
                    _callNapiOptional(this.handle, 'endLayoutBatch');
                }
            }
        }
    }

    setSplitWeights(splitId, mapOrArray) {
        const payloadJson = JSON.stringify(mapOrArray ?? {});
        const apply = () => _callNapiOptional(this.handle, 'setSplitWeights', splitId, payloadJson);
        if (this._layoutBatchDepth > 0) {
            this._queueLayoutMutation(apply);
        } else {
            apply();
        }
    }

    setSplitSizes(splitId, mapOrArray) {
        const payloadJson = JSON.stringify(mapOrArray ?? {});
        const apply = () => _callNapiOptional(this.handle, 'setSplitSizes', splitId, payloadJson);
        if (this._layoutBatchDepth > 0) {
            this._queueLayoutMutation(apply);
        } else {
            apply();
        }
    }

    _resolveSelectTheme(theme) {
        let t = theme;
        if (t === undefined || t === 'auto' || !t) {
            if (globalApp && typeof globalApp.getSystemTheme === 'function') {
                systemTheme = globalApp.getSystemTheme();
            }
            return systemTheme || 'light';
        }
        if (typeof t === 'object') {
            let base = t.base;
            if (!base || base === 'auto') {
                if (globalApp && typeof globalApp.getSystemTheme === 'function') {
                    systemTheme = globalApp.getSystemTheme();
                }
                base = systemTheme || 'light';
            }
            return { ...t, base };
        }
        return t;
    }

    setSelectMenuTheme(theme) {
        this.selectMenuOptions.theme = theme;
        if (this.panes.has('__lotus_select_overlay')) {
            this.sendToPaneRenderer('__lotus_select_overlay', 'update-theme', {
                theme: this._resolveSelectTheme(this.selectMenuOptions.theme),
                customCss: this.selectMenuOptions.css
            }, true);
        }
    }

    setSelectMenuCss(css) {
        this.selectMenuOptions.css = css || '';
        if (this.panes.has('__lotus_select_overlay')) {
            this.sendToPaneRenderer('__lotus_select_overlay', 'update-theme', {
                theme: this.selectMenuOptions.theme,
                customCss: this.selectMenuOptions.css
            }, true);
        }
    }

    _handleSelectOpen(msg) {
        let defaultPrevented = false;
        const selectEvent = {
            preventDefault: () => { defaultPrevented = true; },
            get defaultPrevented() { return defaultPrevented; },
            id: msg.id,
            paneId: msg.pane_id,
            rect: msg.rect,
            options: msg.options,
            selectedOptions: msg.selectedOptions,
            allowMultiple: msg.allowMultiple,
            submit: (indices) => {
                this.handle.submitSelectControl(msg.id, indices || []);
                this._hideSelectOverlay();
            },
            update: (indices) => {
                this.handle.updateSelectControl(msg.id, indices || []);
            },
            dismiss: () => {
                this.handle.dismissSelectControl(msg.id);
                this._hideSelectOverlay();
            }
        };

        this.emit('select-menu', selectEvent);

        if (defaultPrevented || this.selectMenuOptions.enabled === false) {
            return;
        }

        // Parse configured max-height if specified in theme
        let configuredMaxHeight = 280;
        const theme = this.selectMenuOptions.theme;
        if (theme && typeof theme === 'object' && theme.maxHeight) {
            const parsed = parseInt(theme.maxHeight, 10);
            if (!isNaN(parsed) && parsed > 0) {
                configuredMaxHeight = parsed + 10;
            }
        }

        const targetWidth = Math.max(160, Math.round(msg.rect.width));
        const winHeight = this._logicalHeight || 768;
        const winWidth = this._logicalWidth || 1024;

        // Estimate content height accounting for line wrapping
        // Available width for option text: targetWidth minus horizontal padding/borders/icon
        const availableTextWidth = Math.max(60, targetWidth - 58);
        const charsPerLine = Math.max(8, Math.floor(availableTextWidth / 7.5));
        const baseItemHeight = 28;
        const extraLineHeight = 18;

        let estimatedContentHeight = 0;
        for (const item of (msg.options || [])) {
            if (item.type === 'optgroup') {
                estimatedContentHeight += 24;
                for (const opt of (item.options || [])) {
                    const labelLen = (opt.label || '').length;
                    const lines = Math.max(1, Math.ceil(labelLen / charsPerLine));
                    estimatedContentHeight += baseItemHeight + (lines - 1) * extraLineHeight;
                }
            } else {
                const labelLen = (item.label || '').length;
                const lines = Math.max(1, Math.ceil(labelLen / charsPerLine));
                estimatedContentHeight += baseItemHeight + (lines - 1) * extraLineHeight;
            }
        }
        const estimatedHeight = Math.min(configuredMaxHeight, Math.max(36, estimatedContentHeight + 18));

        let targetX = Math.round(msg.rect.x);
        let targetY = Math.round(msg.rect.y + msg.rect.height);

        let flippedUpwards = false;
        // Check clearance below
        if (targetY + estimatedHeight > winHeight) {
            const roomAbove = msg.rect.y;
            const roomBelow = winHeight - (msg.rect.y + msg.rect.height);
            if (roomAbove > roomBelow) {
                // Flip upwards
                targetY = Math.max(0, Math.round(msg.rect.y - estimatedHeight));
                flippedUpwards = true;
            }
        }

        if (targetX + targetWidth > winWidth) {
            targetX = Math.max(0, winWidth - targetWidth);
        }

        this._activeSelectContext = {
            id: msg.id,
            rect: msg.rect,
            configuredMaxHeight,
            targetWidth,
            targetX,
            flippedUpwards
        };

        const overlay = this.panes.get('__lotus_select_overlay');
        if (overlay) {
            overlay.setRect(targetX, targetY, targetWidth, estimatedHeight);
            overlay.setVisible(true);
            overlay.focus();
        }

        this._pendingSelectPayload = {
            id: msg.id,
            options: msg.options,
            selectedOptions: msg.selectedOptions,
            allowMultiple: msg.allowMultiple,
            theme: this._resolveSelectTheme(this.selectMenuOptions.theme),
            customCss: this.selectMenuOptions.css
        };

        this.sendToPaneRenderer('__lotus_select_overlay', 'populate-select', this._pendingSelectPayload, true);
    }

    _resizeSelectOverlay(measuredHeight, promptId) {
        if (!this._activeSelectContext) return;
        if (promptId && this._activeSelectContext.id !== promptId) return;

        const ctx = this._activeSelectContext;
        const overlay = this.panes.get('__lotus_select_overlay');
        if (!overlay) return;

        const winHeight = this._logicalHeight || 768;
        const winWidth = this._logicalWidth || 1024;
        const maxHeight = ctx.configuredMaxHeight || 280;

        const finalHeight = Math.min(maxHeight, Math.max(36, Math.round(measuredHeight)));

        let targetX = ctx.targetX;
        let targetY = Math.round(ctx.rect.y + ctx.rect.height);
        const targetWidth = ctx.targetWidth;

        const roomBelow = winHeight - (ctx.rect.y + ctx.rect.height);
        const roomAbove = ctx.rect.y;

        if (ctx.flippedUpwards || (targetY + finalHeight > winHeight && roomAbove > roomBelow)) {
            targetY = Math.max(0, Math.round(ctx.rect.y - finalHeight));
            ctx.flippedUpwards = true;
        } else {
            ctx.flippedUpwards = false;
        }

        if (targetX + targetWidth > winWidth) {
            targetX = Math.max(0, winWidth - targetWidth);
        }

        overlay.setRect(targetX, targetY, targetWidth, finalHeight);
    }

    _handleSelectClose(msg) {
        this._hideSelectOverlay();
    }

    _hideSelectOverlay() {
        this._pendingSelectPayload = null;
        this._activeSelectContext = null;
        const overlay = this.panes.get('__lotus_select_overlay');
        if (overlay) {
            overlay.setVisible(false);
        }
    }

    _flushBatches() {
        if (!msgpackr) return;
        
        // Flush main window batch
        if (this._batchQueue.length > 0) {
            const packer = this._getPacker('main');
            const packed = packer.pack(this._batchQueue);
            this._sendRawToRenderer(packed);
            this._batchQueue = [];
            this._batchBytes = 0;
        }
        
        // Flush pane batches
        for (const [paneId, queue] of this._paneBatchQueues.entries()) {
            if (queue.length > 0) {
                const packer = this._getPacker(paneId);
                const packed = packer.pack(queue);
                this._sendRawToPaneRenderer(paneId, packed);
            }
        }
        this._paneBatchQueues.clear();
        this._paneBatchBytes.clear();
        this._batchTimer = null;
    }

    _sendRawToRenderer(packed) {
        const CHUNK_THRESHOLD = 1024 * 1024; // 1MB
        if (packed.length > CHUNK_THRESHOLD) {
            this._chunkAndSend(packed, (chunk) => this.handle.sendToRenderer(chunk));
        } else {
            const header = Buffer.from([MSG_TYPE_DATA]);
            this.handle.sendToRenderer(Buffer.concat([header, packed]));
        }
    }

    _sendRawToPaneRenderer(paneId, packed) {
        const CHUNK_THRESHOLD = 1024 * 1024; // 1MB
        if (packed.length > CHUNK_THRESHOLD) {
            this._chunkAndSend(packed, (chunk) => this.handle.sendToPaneRenderer(paneId, chunk));
        } else {
            const header = Buffer.from([MSG_TYPE_DATA]);
            this.handle.sendToPaneRenderer(paneId, Buffer.concat([header, packed]));
        }
    }

    _getPacker(paneId) {
        const clientId = paneId ? `${this.id}:${paneId}` : this.id;
        // In lotus.js scope, we have access to clientPackers map via closure if we move it up,
        // or we can attach it to the exports/global.
        // Actually, let's just use the globalApp instance's closure if we can,
        // but here it's cleaner to just maintain them in a global map in this module.
        if (!globalPackers.has(clientId)) {
            globalPackers.set(clientId, new msgpackr.Packr({ useRecords: false }));
        }
        return globalPackers.get(clientId);
    }

    /**
     * Splits a large payload into chunks and sends them interleaved with the event loop.
     * Prevents the "Large Message DoS" by yielding back to the loop between chunks.
     */
    _chunkAndSend(packed, sendFn) {
        const CHUNK_SIZE = 128 * 1024; // 128KB chunks
        const total = Math.ceil(packed.length / CHUNK_SIZE);
        const msgId = (Math.random() * 0xFFFFFFFF) >>> 0;
        
        let index = 0;
        const sendNext = () => {
            if (index >= total) return;
            
            const start = index * CHUNK_SIZE;
            const end = Math.min(start + CHUNK_SIZE, packed.length);
            const chunkPayload = packed.subarray(start, end);
            
            const header = Buffer.allocUnsafe(9);
            header[0] = MSG_TYPE_CHUNK;
            header.writeUInt32BE(msgId, 1);
            header.writeUInt16BE(total, 5);
            header.writeUInt16BE(index, 7);
            
            const final = Buffer.concat([header, chunkPayload]);
            sendFn(final);
            
            index++;
            if (index < total) {
                // Strictly use setImmediate in Node.js to ensure we yield back to 
                // the main event loop (handling I/O and other messages).
                setImmediate(sendNext);
            }
        };
        
        sendNext();
    }

    sendToRenderer(channel, data, immediate = false) {
        const mainPane = this.panes.get('main');
        if (mainPane && mainPane.isRemote) {
            const jsonMsg = JSON.stringify({
                type: 'event',
                eventName: channel,
                payload: data,
            });
            this.handle.sendToRenderer(Buffer.from(jsonMsg));
            return;
        }

        // If this window has remote Track-2 sub-panes, route per-pane so Track-1 panes
        // receive MessagePack and Track-2 panes receive JSON envelopes (preventing raw
        // binary MessagePack broadcasts from hitting Track-2 JSON WebSocket listeners).
        let hasRemoteSubPane = false;
        for (const p of this.panes.values()) {
            if (p.isRemote && p.id !== 'main') {
                hasRemoteSubPane = true;
                break;
            }
        }
        if (hasRemoteSubPane) {
            for (const p of this.panes.values()) {
                if (!p.id.startsWith('__lotus_')) {
                    this.sendToPaneRenderer(p.id, channel, data, immediate);
                }
            }
            return;
        }

        if (!msgpackr) {
            console.error('[Lotus] msgpackr not loaded, cannot sendToRenderer');
            return;
        }
        
        this._batchQueue.push([channel, data]);
        const dataSize = (typeof data === 'string' ? data.length * 2 : (data?.byteLength ?? data?.length ?? 0));
        this._batchBytes += dataSize;
        
        // Mirror the frontend batch limit (800) AND byte limit (1MB) to prevent accidental 
        // "Infinity Batches" from triggering the chunker when we could have just sent smaller clean packets.
        // Also bypass the batcher entirely for "Whales" (> 256KB) to prevent heap pressure.
        if (immediate || channel === 'resize' || channel === 'resized' || 
            this._batchQueue.length >= 800 || this._batchBytes >= 1024 * 1024 || dataSize > 256 * 1024) {
            this._flushBatches();
        } else if (!this._batchTimer) {
            this._batchTimer = setImmediate(() => this._flushBatches());
        }
    }

    sendToPaneRenderer(paneId, channel, data, immediate = false) {
        const pane = this.panes.get(paneId);
        if (pane && pane.isRemote) {
            const jsonMsg = JSON.stringify({
                type: 'event',
                eventName: channel,
                payload: data,
            });
            this.handle.sendToPaneRenderer(paneId, Buffer.from(jsonMsg));
            return;
        }

        if (!msgpackr) {
            console.error('[Lotus] msgpackr not loaded, cannot sendToPaneRenderer');
            return;
        }
        
        if (!this._paneBatchQueues.has(paneId)) {
            this._paneBatchQueues.set(paneId, []);
            this._paneBatchBytes.set(paneId, 0);
        }
        const queue = this._paneBatchQueues.get(paneId);
        queue.push([channel, data]);
        
        const dataSize = (typeof data === 'string' ? data.length * 2 : (data?.byteLength ?? data?.length ?? 0));
        const currentBytes = this._paneBatchBytes.get(paneId) + dataSize;
        this._paneBatchBytes.set(paneId, currentBytes);
        
        if (immediate || channel === 'resize' || channel === 'resized' || 
            queue.length >= 800 || currentBytes >= 1024 * 1024 || dataSize > 256 * 1024) {
            this._flushBatches();
        } else if (!this._batchTimer) {
            this._batchTimer = setImmediate(() => this._flushBatches());
        }
    }

    /** Backward compatibility: executeScript targets the 'main' pane */
    executeScript(script) {
        const main = this.panes.get('main');
        if (main) main.executeScript(script);
    }

    close() {
        for (const pane of this.panes.values()) {
            if (pane && pane.isRemote) {
                defaultSecureConstellation.releaseSecureEngine(pane.surfaceId);
            }
        }
        this.handle.close();
    }

    setTitle(title) {
        this.handle.setTitle(title);
    }

    setSize(width, height) {
        this.handle.resize(width, height);
    }

    setMinSize(width, height) {
        this.handle.setMinSize(width, height);
    }

    setMaxSize(width, height) {
        this.handle.setMaxSize(width, height);
    }

    setPosition(x, y) {
        this.handle.setPosition(x, y);
    }

    updateDragRegions(rects) {
        if (!this.handle) return;
        
        // Handle both object-with-paneId and legacy array-only formats
        const paneId = rects.paneId;
        const rectsJson = JSON.stringify(rects);
        
        if (paneId && this.handle.updatePaneDragRegions) {
            this.handle.updatePaneDragRegions(paneId, rectsJson);
        } else if (this.handle.updateDragRegions) {
            this.handle.updateDragRegions(rectsJson);
        }
    }

    show() {
        this.handle.show();
    }

    hide() {
        this.handle.hide();
    }

    minimize() {
        if (this.handle && this.handle.minimize) {
            this.handle.minimize();
        }
    }

    unminimize() {
        if (this.handle && this.handle.unminimize) {
            this.handle.unminimize();
        }
    }

    maximize() {
        this.maximized = true;
        if (this.handle && this.handle.maximize) {
            this.handle.maximize();
        }
    }

    unmaximize() {
        this.maximized = false;
        if (this.handle && this.handle.unmaximize) {
            this.handle.unmaximize();
        }
    }

    isMaximized() {
        return Boolean(this.maximized);
    }

    toggleMaximize() {
        if (this.isMaximized()) {
            this.unmaximize();
        } else {
            this.maximize();
        }
        return this.maximized;
    }

    focus() {
        if (this.handle && this.handle.focus) {
            this.handle.focus();
        }
    }
}

module.exports = {
    ServoWindow,
    LayoutBuilder,
    Anchor,
    ipcMain,
    respawnCrashedSurface,
    showHostCrashChrome,
    app: {
        quit: () => {
            defaultSecureConstellation.terminateAll().catch(() => {});
            if (globalApp && typeof globalApp.quit === 'function') {
                globalApp.quit();
            }
            const fallbackTimer = setTimeout(() => {
                if (globalApp && typeof globalApp.exit === 'function') {
                    globalApp.exit(0);
                } else {
                    process.exit(0);
                }
            }, 1000);
            if (fallbackTimer && typeof fallbackTimer.unref === 'function') {
                fallbackTimer.unref();
            }
        },
        exit: (code = 0) => {
            defaultSecureConstellation.terminateAll().catch(() => {});
            if (globalApp && typeof globalApp.exit === 'function') {
                globalApp.exit(code);
            } else {
                process.exit(code);
            }
        },
        warmup: ensureApp,
        getSystemTheme: () => {
            if (process.platform === 'linux') {
                const detected = detectLinuxThemeNode();
                if (detected) {
                    systemTheme = detected;
                    return systemTheme;
                }
            }
            if (globalApp && typeof globalApp.getSystemTheme === 'function') {
                systemTheme = globalApp.getSystemTheme();
            }
            return systemTheme;
        },
        initVfs: (options = {}) => {
            ensureApp();
            const required = !!(options?.required || (typeof globalThis !== 'undefined' && globalThis.__LOTUS_ENCRYPTED_BUILD__));
            if (globalApp) {
                const initialized = globalApp.initVfs();
                if (initialized) {
                    vfsInitialized = true;
                } else if (required) {
                    throw new Error(
                        'Fatal: Encrypted VFS initialization is required for this build, ' +
                        'but VFS sections/resources were not found in the executable.'
                    );
                }
                return !!initialized;
            }
            if (required) {
                throw new Error(
                    'Fatal: Encrypted VFS initialization is required for this build, ' +
                    'but native application core is unavailable.'
                );
            }
            return false;
        },
        isVfsInitialized: () => vfsInitialized,
        validatePlatformSecurityIsolation,
        detectLinuxThemeNode,
        spawnSandboxedEngine: require('./launcher.js').spawnSandboxedEngine,
        networkBroker: require('./network_broker.js'),
        NetworkBroker: require('./network_broker.js').NetworkBroker,
        isAllowedPublicIp: require('./network_broker.js').isAllowedPublicIp,
        secureConstellation: require('./secure_constellation.js'),
        originDerivation: require('./origin_derivation.js'),
        hostIpcPolicy,
        hostIpcRouter,
        defaultSecureConstellation,
        allocateRemotePaneShm,
        resetRemotePaneShm,
        createControlSocketPair,
        createGpuSocketPair,
        ensureWebglBroker,
        registerGpuWebglBroker,
        sendFdsOverSocket,
        registerSurfacePolicy,
        register_surface_policy: registerSurfacePolicy,
        dispatchToSurface,
        dispatch_to_surface: dispatchToSurface,
        getIpcPort: () => ipcServerPort,
    },
    validatePlatformSecurityIsolation,
    detectLinuxThemeNode,
    spawnSandboxedEngine: require('./launcher.js').spawnSandboxedEngine,
    launcher: require('./launcher.js'),
    networkBroker: require('./network_broker.js'),
    NetworkBroker: require('./network_broker.js').NetworkBroker,
    isAllowedPublicIp: require('./network_broker.js').isAllowedPublicIp,
    secureConstellation: require('./secure_constellation.js'),
    SecureConstellationManager: require('./secure_constellation.js').SecureConstellationManager,
    originDerivation: require('./origin_derivation.js'),
    hostIpcPolicy,
    hostIpcRouter,
    registerHostRegistry,
    resetHostRegistry,
    defaultSecureConstellation,
    respawnCrashedSurface,
    showHostCrashChrome,
    allocateRemotePaneShm,
    resetRemotePaneShm,
    createControlSocketPair,
    createGpuSocketPair,
    ensureWebglBroker,
    registerGpuWebglBroker,
    sendFdsOverSocket,
    registerSurfacePolicy,
    register_surface_policy: registerSurfacePolicy,
    dispatchToSurface,
    dispatch_to_surface: dispatchToSurface,
    getIpcPort: () => ipcServerPort,
    warmup: ensureApp,
    exit: (code = 0) => {
        if (globalApp && typeof globalApp.exit === 'function') {
            globalApp.exit(code);
        } else {
            process.exit(code);
        }
    },
};
