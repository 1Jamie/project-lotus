const { spawn } = require('child_process');

// Auto-fix for Linux TLS allocation issue (ERR_DLOPEN_FAILED)
// This must run BEFORE loading the native module in index.js
if (process.platform === 'linux') {
    const requiredTunable = 'glibc.rtld.optional_static_tls=4096';
    const currentTunables = process.env.GLIBC_TUNABLES || '';

    // LOTUS_TLS_FIXED=1 is injected into the respawned process below.
    // If it's already set we've already fixed the environment -- never respawn again,
    // even if the tunable string looks wrong, to prevent an infinite loop.
    const alreadyFixed = process.env.LOTUS_TLS_FIXED === '1';

    if (!alreadyFixed && !currentTunables.includes('optional_static_tls=4096')) {
        const newTunables = currentTunables
            ? `${currentTunables}:${requiredTunable}`
            : requiredTunable;

        const { spawnSync } = require('child_process');
        const result = spawnSync(process.execPath, process.argv.slice(1), {
            stdio: 'inherit',
            env: {
                ...process.env,
                GLIBC_TUNABLES: newTunables,
                LOTUS_TLS_FIXED: '1',  // <-- prevents any re-spawn loop
            }
        });

        process.exit(result.status ?? 1);

        // Stop execution of the current process (don't load native module)
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


const { App, createWindow } = require('./index.js');
const EventEmitter = require('events');
let msgpackr;
try {
    msgpackr = require('msgpackr');
} catch (e) {
    console.warn('msgpackr not found, IPC will fail if binary');
}

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
    sendTo(windowId, channel, data) {
        const win = windows.get(windowId);
        if (win) win.sendToRenderer(channel, data);
    }

    /**
     * Register a request/reply handler for window.lotus.invoke() calls.
     * The handler receives the payload (with _replyId stripped) and may
     * return a value or a Promise. The resolved value is automatically
     * sent back to the originating renderer window only (not broadcast).
     * Thrown / rejected errors are forwarded as { _error: message }.
     */
    handle(channel, handler) {
        // EventEmitter passes extra args -- second arg is the windowId threaded
        // through ipcMain.emit() by the batch routing code below.
        this.on(channel, async (data, fromWindowId) => {
            const replyId = data && data._replyId;
            if (!replyId) return; // not an invoke() call; ignore
            const payload = Object.assign({}, data);
            delete payload._replyId;
            try {
                const result = await handler(payload);
                // Reply to the originating window only -- not all windows.
                if (fromWindowId) {
                    this.sendTo(fromWindowId, replyId, result);
                } else {
                    this.send(replyId, result); // fallback for legacy paths
                }
            } catch (err) {
                const errPayload = { _error: err?.message ?? String(err) };
                if (fromWindowId) {
                    this.sendTo(fromWindowId, replyId, errPayload);
                } else {
                    this.send(replyId, errPayload);
                }
            }
        });
    }
}

const isProfiling = process.argv.includes('--profile');
const startTime = Date.now();

if (isProfiling) {
    console.log(`[PROFILE] Node.js process started at ${new Date(startTime).toISOString()}`);
}

const ipcMain = new IpcMain();
let globalApp = null;
const windows = new Map();

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

        globalApp = new App((data) => {
            try {
                const buffer = Buffer.from(data);
                if (!msgpackr) return;
                const msg = msgpackr.unpack(buffer);

                // Handle app-ready event
                if (msg.event === 'app-ready') {
                    if (isProfiling) console.log(`[PROFILE] App/IPC Ready event received.`);
                    ipcMain.emit('ready', msg);
                    return;
                }

                // All other events should have a window_id
                const windowId = msg.window_id;
                const win = windows.get(windowId);

                if (msg.event === 'ready') {
                    if (isProfiling) {
                        const time = Date.now() - startTime;
                        console.log(`[PROFILE] Window ${windowId} READY event received by JS after ${time}ms`);
                    }
                    if (win) win.emit('ready');
                    return;
                }

                if (msg.event === 'load-status') {
                    if (isProfiling) {
                        const time = Date.now() - startTime;
                        console.log(`[PROFILE] Window ${windowId || 'UNKNOWN'} load-status: ${msg.status} after ${time}ms`);
                        if (msg.status === 'complete') {
                            console.log(`[PROFILE] Window ${windowId || 'UNKNOWN'} Total Load Time: ${time}ms`);
                        }
                    }
                    if (win) win.emit('load-status', msg.status);
                    return;
                }

                if (msg.event === 'frame-ready') {
                    if (win) win.emit('frame-ready');
                    return;
                }

                if (msg.event === 'window-closed') {
                    if (win) {
                        win.emit('closed');
                        windows.delete(windowId);
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
                    if (win) win.emit('resize', { width: msg.width, height: msg.height });
                    return;
                }

                // IPC Batch message routing (from /batch endpoint)
                if (msg.event === 'ipc-batch') {
                    const rawBatch = msg._raw_batch;
                    const batchWindow = windows.get(msg.window_id);
                    if (rawBatch instanceof Uint8Array || Buffer.isBuffer(rawBatch)) {
                        try {
                            const batch = msgpackr.unpack(Buffer.from(rawBatch));
                            if (Array.isArray(batch)) {
                                batch.forEach(m => {
                                    if (Array.isArray(m) && m.length === 2) {
                                        if (m[0] === 'lotus:set-drag-regions') {
                                            if (batchWindow) batchWindow.updateDragRegions(m[1]);
                                        } else {
                                            // Pass windowId as 2nd arg so handle() can reply to this window only.
                                            ipcMain.emit(m[0], m[1], msg.window_id);
                                        }
                                    }
                                });
                            }
                        } catch (e) {
                            console.error('[lotus] Failed to decode ipc-batch:', e);
                        }
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
                                // Pass windowId as 2nd arg so handle() can reply to this window only.
                                ipcMain.emit(m[0], m[1], msg.window_id);
                            }
                        }
                    });
                }
            } catch (e) {
                console.error('[lotus] Failed to process event:', e);
            }
        }, isProfiling, appIdentifier, msgpackrSource);
    }
    return globalApp;
}

class ServoWindow extends EventEmitter {
    constructor(options = {}) {
        super();
        ensureApp();

        if (typeof options === 'string') {
            options = { initialUrl: options };
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
            visible: true
        };

        const finalOptions = { ...defaultOptions, ...options };

        if (finalOptions.root) {
            // Validate root path
            const path = require('path');
            if (!path.isAbsolute(finalOptions.root)) {
                console.warn(`[Lotus] Warning: 'root' path should be absolute: ${finalOptions.root}`);
                finalOptions.root = path.resolve(finalOptions.root);
            }

            // ADD THIS LINE: Force POSIX-style paths so Servo's Rust URL parser doesn't choke on Windows
            finalOptions.root = finalOptions.root.replace(/\\/g, '/');

            // If using root, initialUrl is automatically set to our custom protocol
            finalOptions.initialUrl = `lotus-resource://localhost/${finalOptions.index}`;
        }

        this.handle = createWindow(finalOptions);
        this.id = this.handle.getId();
        windows.set(this.id, this);
    }

    loadUrl(url) {
        this.handle.loadUrl(url);
    }

    sendToRenderer(channel, data) {
        if (!msgpackr) {
            console.error('[Lotus] msgpackr not loaded, cannot sendToRenderer');
            return;
        }
        // Pack as a single-entry batch [[channel, data]] -- the renderer's
        // window.lotus._ws.onmessage handler already decodes this format.
        const packed = msgpackr.pack([[channel, data]]);
        this.handle.sendToRenderer(Buffer.from(packed));
    }

    executeScript(script) {
        this.handle.executeScript(script);
    }

    close() {
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
        if (this.handle && this.handle.updateDragRegions) {
            this.handle.updateDragRegions(JSON.stringify(rects));
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
        if (this.handle && this.handle.maximize) {
            this.handle.maximize();
        }
    }

    unmaximize() {
        if (this.handle && this.handle.unmaximize) {
            this.handle.unmaximize();
        }
    }

    focus() {
        if (this.handle && this.handle.focus) {
            this.handle.focus();
        }
    }
}

module.exports = {
    ServoWindow,
    ipcMain,
    app: {
        quit: () => globalApp && globalApp.quit(),
        warmup: ensureApp
    }
};
