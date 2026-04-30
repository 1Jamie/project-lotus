const { spawn } = require('child_process');

// Auto-fix for Linux TLS allocation issue (ERR_DLOPEN_FAILED)
// This must run BEFORE loading the native module in index.js
if (process.platform === 'linux') {
    const requiredTunable = 'glibc.rtld.optional_static_tls=16384';
    const currentTunables = process.env.GLIBC_TUNABLES || '';
    const alreadyFixed = process.env.LOTUS_TLS_FIXED === '1';

    if (!alreadyFixed && !currentTunables.includes('optional_static_tls=')) {
        console.log(`[LOTUS] Detected Linux. Respawning with ${requiredTunable} to prevent TLS allocation issues...`);
        const { spawnSync } = require('child_process');
        const result = spawnSync(process.execPath, process.argv.slice(1), {
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
        
        setInterval(() => this.cleanup(), this.CLEANUP_INTERVAL);
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
const windows = new Map();
const eventQueue = new Map(); // windowId -> Array of pending events
const globalPackers = new Map();
const globalUnpackers = new Map();

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
                const [windowId, paneId] = clientId.split(':');
                handleProcessedMsg(windowId, paneId || 'main', msg);
            } catch (e) {
                console.error(`[lotus] Failed to unpack reassembled message for ${clientId}:`, e);
            }
        });

        const handleProcessedMsg = (windowId, paneId, msg) => {
            const clientId = paneId ? `${windowId}:${paneId}` : windowId;

            // Re-attach window_id and pane_id for compatibility with existing emitters/handlers
            msg.window_id = windowId;
            if (paneId) msg.pane_id = paneId;

            // Handle app-ready event
            if (msg.event === 'app-ready') {
                if (isProfiling) console.log(`[PROFILE] App/IPC Ready event received.`);
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

        globalApp = new App((clientId, messages) => {
            if (!msgpackr) return;
            const unpacker = getUnpacker(clientId);
            const [windowId, paneId] = clientId.split(':');
            
            for (const data of messages) {
                try {
                    const type = data[0];
                    if (type === MSG_TYPE_DATA) {
                        const msg = unpacker.unpack(data.subarray(1));
                        handleProcessedMsg(windowId, paneId || 'main', msg);
                    } else if (type === MSG_TYPE_CHUNK) {
                        assembler.handleChunk(clientId, data, unpacker);
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

    if (msg.event === '_internal-created') {
        if (isProfiling) {
            const time = Date.now() - startTime;
            console.log(`[PROFILE] Window ${windowId} NATIVE CREATED event received by JS after ${time}ms`);
        }
        if (win) {
            // Emit a synthetic resize with the ACTUAL window dimensions immediately after
            // the window handle is created. This corrects stale pane rects when restoreState
            // restores the window to a different size than the hardcoded initial values.
            if (msg.logicalWidth && msg.logicalHeight) {
                const resizePayload = {
                    width: msg.width,
                    height: msg.height,
                    logicalWidth: msg.logicalWidth,
                    logicalHeight: msg.logicalHeight
                };
                win.emit('resize', resizePayload);
                win.emit('resized', resizePayload);
            }
        }
        return;
    }

    if (msg.event === 'load-status') {
        if (win) {
            win.emit('load-status', msg.status, paneId);
            const pane = win.panes.get(paneId);
            if (pane) pane.emit('load-status', msg.status);
        }
        return;
    }

    if (msg.event === 'title-changed') {
        if (win) {
            win.emit('title-changed', msg.title, paneId);
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
        if (win) {
            const payload = { width: msg.width, height: msg.height, logicalWidth: msg.logicalWidth, logicalHeight: msg.logicalHeight };
            
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
    constructor(window, id) {
        super();
        this.window = window;
        this.id = id;
    }

    loadUrl(url) {
        this.window.handle.loadUrlInPane(this.id, url);
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
        this.window.handle.setPaneVisible(this.id, visible);
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
        
        // If the user provided panes, ensure they are in the format Rust expects
        if (finalOptions.panes && Array.isArray(finalOptions.panes)) {
            finalOptions.panes = finalOptions.panes.map(p => ({
                id: p.id,
                url: p.url || 'about:blank',
                x: p.x || 0,
                y: p.y || 0,
                width: p.width || 0,
                height: p.height || 0,
                zIndex: p.zIndex || 0,
                visible: p.visible !== false,
                anchor: p.anchor,
                dockOrder: p.dockOrder
            }));
        }

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

        
        // The 'main' pane is always created by Rust
        this.panes.set('main', new Pane(this, 'main'));
        
        // Register any other panes that were created atomically
        if (finalOptions.panes) {
            finalOptions.panes.forEach(p => {
                if (p.id !== 'main') {
                    this.panes.set(p.id, new Pane(this, p.id));
                }
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
        if (this.panes.has(id)) {
            throw new Error(`Pane with id '${id}' already exists`);
        }
        const { url = 'about:blank', x = 0, y = 0, width = 0, height = 0, zIndex = 0, anchor = 0, dockOrder = 0 } = options;
        this.handle.createPane(id, url, x, y, width, height, zIndex, anchor, dockOrder);
        const pane = new Pane(this, id);
        this.panes.set(id, pane);
        return pane;
    }

    removePane(id) {
        if (id === 'main') {
            throw new Error("Cannot remove the 'main' pane");
        }
        if (this.panes.has(id)) {
            this.handle.removePane(id);
            this.panes.delete(id);
        }
    }

    getPane(id) {
        return this.panes.get(id);
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
    LayoutBuilder,
    Anchor,
    ipcMain,
    app: {
        quit: () => globalApp && globalApp.quit(),
        warmup: ensureApp,
        initVfs: () => {
            ensureApp();
            if (globalApp) globalApp.initVfs();
        }
    }
};
