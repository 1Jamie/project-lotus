const { spawn } = require('child_process');

// Auto-fix for Linux TLS allocation issue (ERR_DLOPEN_FAILED)
// This must run BEFORE loading the native module in index.js
if (process.platform === 'linux') {
    const requiredTunable = 'glibc.rtld.optional_static_tls=4096';
    const currentTunables = process.env.GLIBC_TUNABLES || '';

    if (!currentTunables.includes('optional_static_tls=4096')) {
        const newTunables = currentTunables
            ? `${currentTunables}:${requiredTunable}`
            : requiredTunable;

        const { spawnSync } = require('child_process');
        const result = spawnSync(process.execPath, process.argv.slice(1), {
            stdio: 'inherit',
            env: {
                ...process.env,
                GLIBC_TUNABLES: newTunables
            }
        });

        process.exit(result.status);

        // Stop execution of the current process (don't load native module)
        return;
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

    send(channel, data) {
        // Send to all windows (or we could have an 'activeWindow' concept)
        for (const win of windows.values()) {
            win.sendToRenderer(channel, data);
        }
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
            const fs = require('fs');
            const path = require('path');
            
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

                // IPC Message routing
                if (Array.isArray(msg)) {
                    // Check if it's a batch message (array of [channel, data])
                    msg.forEach(m => {
                        if (Array.isArray(m) && m.length === 2) {
                            ipcMain.emit(m[0], m[1]);
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
            // If using root, initialUrl is automatically set to our custom protocol
            // We use 'localhost' as authority, but the Rust side will resolve based on window ID if needed,
            // or just serve relative to the root.
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
        const json = JSON.stringify(data);
        this.handle.sendToRenderer(channel, json);
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

    setPosition(x, y) {
        this.handle.setPosition(x, y);
    }

    show() {
        this.handle.show();
    }

    hide() {
        this.handle.hide();
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
