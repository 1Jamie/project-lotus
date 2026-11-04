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
    createControlSocketPair,
    sendFdsOverSocket,
    registerSurfacePolicy,
    dispatchToSurface
} = require('./index.js');
const { SecureConstellationManager } = require('./secure_constellation.js');
const EventEmitter = require('events');
const crypto = require('node:crypto');

const defaultSecureConstellation = new SecureConstellationManager({
    engineBin: path.resolve(__dirname, '../lotus-engine/target/debug/lotus-engine'),
    supervisorBin: path.resolve(__dirname, '../lotus-supervisor/target/debug/lotus-supervisor'),
});
let msgpackr;
try {
    msgpackr = require('msgpackr');
} catch (e) {
    console.warn('msgpackr not found, IPC will fail if binary');
}

const hostIpcPolicy = require('./host_ipc_policy.js');
const hostIpcRouter = new hostIpcPolicy.HostIpcPolicyRouter();

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

                // Two-phase output projection
                const safeResult = hostIpcPolicy.validateAndProjectOutput(options ? options.schema : undefined, rawResult);

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
                const errPayload = { _error: err?.message ?? String(err) };
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
    const isSecure = options.secure === true;
    const separateProcess = options.separateProcess !== false; // defaults to true when secure: true
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

    // On Linux, verify that unprivileged user namespaces are not disabled by the kernel or AppArmor
    try {
        const fs = require('node:fs');
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
                                if (json && json.type === 'invoke') {
                                    const payload = Object.assign({}, json.payload || {});
                                    payload._replyId = json.id;
                                    payload._documentId = json.documentId;
                                    ipcMain.emit(json.method, payload, clientId);
                                }
                            } else {
                                throw unpackErr;
                            }
                        }
                    } else if (type === MSG_TYPE_CHUNK) {
                        assembler.handleChunk(clientId, data, unpacker);
                    } else if (type === 0x7B) { // '{' character - Track 2 JSON frame
                        try {
                            const json = JSON.parse(data.toString('utf8'));
                            if (json && json.type === 'invoke') {
                                const payload = Object.assign({}, json.payload || {});
                                payload._replyId = json.id;
                                payload._documentId = json.documentId;
                                ipcMain.emit(json.method, payload, clientId);
                            }
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

    if (msg.event === 'window-closed') {
        if (win) {
            win.emit('close');
            win.emit('closed');
            for (const p of win.panes.keys()) {
                hostIpcRouter.surfaceTracker.removeSurface(`${windowId}:${p}`);
            }
            windows.delete(windowId);
        }
        if (windows.size === 0) {
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

class LayoutBuilder {
    constructor() {
        this._panes = [];
        this._currentDockOrder = 0;
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
        return { panes: this._panes };
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
        this._engineInstance = null;
        this._shm = null;
    }

    loadUrl(url) {
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
        this.window.handle.setPaneRect(this.id, x, y, width, height);
    }

    remove() {
        this.window.removePane(this.id);
    }

    setVisible(visible) {
        this.visible = Boolean(visible);
        this.window.handle.setPaneVisible(this.id, this.visible);
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
                dockOrder: 0
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

        this.handle = createWindow(finalOptions);
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

        
        // Register all panes created atomically with the window
        if (finalOptions.panes) {
            finalOptions.panes.forEach(p => {
                this.panes.set(p.id, new Pane(this, p.id, p.visible !== false));
            });
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
        if (finalOptions.panes) {
            finalOptions.panes.forEach(p => {
                const surfaceId = `${this.id}:${p.id}`;
                const paneObj = this.panes.get(p.id);
                if (paneObj) paneObj.surfaceId = surfaceId;

                hostIpcRouter.surfaceTracker.createSurface(surfaceId, {
                    secure: finalOptions.secure === true,
                    allowedChannels: this._allowedChannels,
                    trustedOrigins: this._trustedOrigins
                });

                const initNavId = `nav-init-${crypto.randomBytes(8).toString('hex')}`;
                if (paneObj) {
                    paneObj._pendingNavId = initNavId;
                    paneObj._pendingUrl = p.url || 'about:blank';
                }
                hostIpcRouter.surfaceTracker.startNavigation(surfaceId, initNavId, p.url || 'about:blank');
            });
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
        const { url = 'about:blank', x = 0, y = 0, width = 0, height = 0, zIndex = 0, anchor = 0, dockOrder = 0, visible = true } = options;
        const isSecure = options.secure === true || (this._finalOptions && this._finalOptions.secure === true);
        const surfaceId = `${this.id}:${id}`;

        const pane = new Pane(this, id, visible);
        pane.surfaceId = surfaceId;
        pane.isRemote = isSecure;

        hostIpcRouter.surfaceTracker.createSurface(surfaceId, {
            secure: isSecure,
            allowedChannels: options.allowedChannels || this._allowedChannels || ['vault', 'telemetry'],
            trustedOrigins: options.trustedOrigins || this._trustedOrigins || []
        });

        const initNavId = `nav-init-${crypto.randomBytes(8).toString('hex')}`;
        pane._pendingNavId = initNavId;
        pane._pendingUrl = url;
        hostIpcRouter.surfaceTracker.startNavigation(surfaceId, initNavId, url);

        if (isSecure && typeof allocateRemotePaneShm === 'function') {
            const shmWidth = Math.max(1, width || this._logicalWidth || 800);
            const shmHeight = Math.max(1, height || this._logicalHeight || 600);
            const shm = allocateRemotePaneShm(surfaceId, shmWidth, shmHeight);
            pane._shm = shm;

            const pixelRoFd = shm.pixelRoFd ?? shm.roFd;
            const ackRwFd = shm.ackRwFd ?? -1;
            this.handle.createRemotePane(id, pixelRoFd, shm.totalSize, ackRwFd, x, y, width, height, zIndex, anchor, dockOrder, visible);

            defaultSecureConstellation.acquireSecureEngine(surfaceId, {
                pane,
                url,
                shm,
            }).then(engineRes => {
                pane._engineInstance = engineRes.instance;
                if (engineRes.instance.controlFd && this.handle && typeof this.handle.attachRemotePaneInput === 'function') {
                    try {
                        this.handle.attachRemotePaneInput(id, engineRes.instance.controlFd);
                    } catch (e) {
                        console.error('[createPane] attachRemotePaneInput error:', e);
                    }
                }
                if (url && url !== 'about:blank') {
                    try {
                        engineRes.instance.controlStream.write(JSON.stringify({
                            type: 'Navigate',
                            surface_id: surfaceId,
                            navigation_id: 1,
                            url,
                        }) + '\n');
                    } catch {}
                }
            }).catch(err => {
                pane.emit('error', err);
            });
        } else {
            this.handle.createPane(id, url, x, y, width, height, zIndex, anchor, dockOrder, visible);
        }

        this.panes.set(id, pane);

        return pane;
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
    app: {
        quit: () => {
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
        createControlSocketPair,
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
    defaultSecureConstellation,
    allocateRemotePaneShm,
    createControlSocketPair,
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
