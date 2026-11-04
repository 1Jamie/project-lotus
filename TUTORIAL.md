# Building Your First Lotus App

Welcome to Lotus. If you have built a desktop app with web technologies before, you already know most of what you need.

The biggest difference in Lotus is how direct the boundary is:
* **Node.js is the driver.** Your `main.js` runs directly on the host OS with full Node.js capabilities.
* **Servo is the pixel engine.** Your HTML, CSS, and browser JavaScript live in `ui/`. Servo renders those pixels onto a hardware-accelerated window (`winit` + OpenGL/ANGLE) without spinning up a Chromium browser instance.
* **IPC (`window.lotus` ↔ `ipcMain`) is the steering wheel.** Binary-packed (MsgPack) WebSockets connect your UI to your Node.js process over localhost (authenticated with a per-session UUID token).

In this tutorial, we will build a **"System Pulse" Desktop Monitor** from scratch. Along the way, we will cover:
1. Scaffolding a project with `@lotus-gui/dev`
2. Opening a native transparent window with zero white-flash
3. Designing a frameless window with custom drag regions and window controls
4. Using `invoke()` / `handle()` and `send()` / `on()` to stream OS stats from Node.js to the UI
5. Splitting a window into multiple panes with `LayoutBuilder`
6. Sandboxing external or untrusted content with `sandbox: true`
7. Packaging your app into a native installer (and optionally encrypting your assets)

---

## Step 1: Scaffold Your Project

Pre-built native binaries for Linux and Windows are included on npm, so you do not need to compile Rust or Servo to get started.

Open your terminal and use the `@lotus-gui/dev` CLI to initialize a new project:

```bash
npx @lotus-gui/dev init system-pulse
cd system-pulse
npm install
```

When prompted by `lotus init`, press **Enter** to accept the defaults. Your project directory will look like this:

```text
system-pulse/
├── lotus.config.json    # App metadata, icon path, and installer settings
├── package.json         # Scripts and dependencies (@lotus-gui/core & @lotus-gui/dev)
├── main.js              # Node.js main process (OS integration & window management)
└── ui/
    └── index.html       # Your frontend UI (served via lotus-resource://)
```

Start the hot-reloading dev runner:

```bash
npx lotus dev main.js
```

`lotus dev` watches your project directory (ignoring `node_modules` and `dist`). Whenever you save a change to `main.js` or your `ui/` files, it cleanly stops the previous window and restarts your app.

---

## Step 2: Understanding the Window Lifecycle (`main.js`)

Open `main.js` and replace its contents with the following setup:

```javascript
const { ServoWindow, ipcMain, app } = require('@lotus-gui/core');
const path = require('path');
const os = require('os');

// 1. Initialize VFS (no-op during dev, unlocks encrypted assets if built with --encrypt)
app.initVfs();

// 2. Pre-warm the Servo engine before creating windows
app.warmup();

const win = new ServoWindow({
    id: 'system-pulse-main',          // Giving an ID enables automatic position/size saving
    root: path.join(__dirname, 'ui'), // Serves files via lotus-resource://localhost/
    index: 'index.html',
    width: 900,
    height: 600,
    title: 'System Pulse',
    frameless: true,                  // Remove OS titlebar so we can draw our own
    transparent: true,                // OS-level window transparency
    visible: false,                   // Keep hidden until the first frame is painted
    selectMenu: { theme: 'dark' },    // Style native <select> popups ('auto', 'dark', 'light', or token object)
    scrollbars: { theme: 'dark', autoHide: true, width: 8 } // Configure injected GPU overlay scrollbars
});

// Enforce minimum window dimensions (600x400)
win.setMinSize(600, 400);

// 3. Show the window only after Servo paints the very first frame
win.once('ready-to-show', () => {
    win.show();
});
```

### Why these options matter:
* **`root: path.join(__dirname, 'ui')` (Hybrid Mode):** Lotus does not start a local HTTP server to serve your HTML/CSS. Instead, it registers a custom `lotus-resource://localhost/` protocol inside Servo and jails file access strictly to your `ui/` directory.
* **`transparent: true` + `visible: false` + `ready-to-show`:** The window starts hidden, paints its first frame onto a transparent OpenGL surface, and reveals itself on `ready-to-show` so users never see a white startup flash.
* **`id: 'system-pulse-main'`:** Whenever you give a `ServoWindow` a static `id`, Lotus saves its screen coordinates, size, and maximized state to disk (`~/.config/<app>/window-state.json`) and restores it on the next launch.
* **`selectMenu` & `scrollbars`:** Servo does not render OS-native `<select>` popups or scrollbars on its own. Lotus bridges both automatically: `<select>` elements (including `<optgroup>` and multi-select) open in a dedicated high-z-index overlay pane (`__lotus_select_overlay`) that never gets clipped by parent containers or split panes, and pages get GPU-positioned (`translate3d`) overlay scrollbars that expand on hover without blocking the 6px frameless window resize edge. You can also change dropdown styles at runtime with `win.setSelectMenuTheme('dark')` or `win.setSelectMenuCss(...)`.

---

## Step 3: Talk Between Node.js and the UI (IPC)

Still in `main.js`, let's add two kinds of IPC handlers below our window setup:
1. **Request/Reply (`ipcMain.handle`)** for fetching static machine info when the UI boots.
2. **Fire-and-Forget (`ipcMain.on` & `win.sendToRenderer`)** for window control buttons and live memory/uptime updates every second.

Append this to the bottom of `main.js`:

```javascript
// Request/Reply: Renderer calls `await window.lotus.invoke('get-machine-info')`
ipcMain.handle('get-machine-info', async () => {
    return {
        hostname: os.hostname(),
        platform: `${os.type()} (${os.release()})`,
        arch: os.arch(),
        cpuModel: os.cpus()[0]?.model || 'Unknown CPU',
        cores: os.cpus().length,
        nodeVersion: process.version
    };
});

// Fire-and-Forget: Custom titlebar buttons (minimize, maximize, close)
ipcMain.on('window-control', (action) => {
    if (action === 'minimize') win.minimize();
    if (action === 'maximize') win.toggleMaximize(); // Reads live OS state via win.isMaximized()
    if (action === 'close') win.close();
});

// Push live memory & uptime stats to the UI every second
const statsTimer = setInterval(() => {
    const totalMem = os.totalmem();
    const freeMem = os.freemem();
    const usedMem = totalMem - freeMem;

    win.sendToRenderer('live-stats', {
        memoryPercent: Math.round((usedMem / totalMem) * 100),
        usedGb: (usedMem / 1024 / 1024 / 1024).toFixed(2),
        totalGb: (totalMem / 1024 / 1024 / 1024).toFixed(2),
        uptimeSeconds: Math.floor(os.uptime())
    });
}, 1000);

win.on('closed', () => {
    clearInterval(statsTimer);
});
```

---

## Step 4: Build the Frameless UI (`ui/index.html`)

Because we enabled `frameless: true` and `transparent: true`, we have full control over how the window looks, including rounded corners and our own title bar.

### How Dragging and Resizing Work in Frameless Windows
* **Moving the window:** Add `-webkit-app-region: drag` in CSS (or `data-lotus-drag="true"` in HTML) to your title bar. Put `-webkit-app-region: no-drag` on interactive buttons inside that bar so clicks still reach them.
* **Resizing the window:** Lotus attaches DPI-scaled, 6px invisible resize borders around every edge and corner of a frameless window in Rust.

Replace `ui/index.html` with the following code:

```html
<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <title>System Pulse</title>
    <style>
        * { box-sizing: border-box; margin: 0; padding: 0; }

        /* Keep body transparent so our rounded .window container defines the shape */
        body {
            background: transparent;
            font-family: system-ui, -apple-system, sans-serif;
            color: #e2e8f0;
            height: 100vh;
            overflow: hidden;
            user-select: none;
        }

        .window {
            display: flex;
            flex-direction: column;
            height: 100vh;
            background: rgba(15, 23, 42, 0.94);
            border: 1px solid rgba(148, 163, 184, 0.2);
            border-radius: 12px;
            overflow: hidden;
        }

        /* Custom draggable titlebar */
        .titlebar {
            height: 40px;
            background: rgba(30, 41, 59, 0.9);
            display: flex;
            align-items: center;
            justify-content: space-between;
            padding: 0 14px;
            -webkit-app-region: drag;
            cursor: grab;
            border-bottom: 1px solid rgba(148, 163, 184, 0.15);
        }

        .titlebar-title {
            font-size: 13px;
            font-weight: 600;
            letter-spacing: 0.03em;
            color: #94a3b8;
        }

        /* Exclude buttons from dragging so they can be clicked */
        .window-controls {
            display: flex;
            gap: 8px;
            -webkit-app-region: no-drag;
        }

        .ctrl-btn {
            width: 28px;
            height: 24px;
            border: none;
            border-radius: 6px;
            background: rgba(255, 255, 255, 0.06);
            color: #cbd5e1;
            cursor: pointer;
            font-size: 12px;
        }
        .ctrl-btn:hover { background: rgba(255, 255, 255, 0.15); }
        .ctrl-btn.close:hover { background: #ef4444; color: white; }

        .content {
            padding: 24px;
            display: flex;
            flex-direction: column;
            gap: 20px;
            flex: 1;
            overflow-y: auto;
        }

        .card {
            background: rgba(30, 41, 59, 0.6);
            border: 1px solid rgba(148, 163, 184, 0.12);
            border-radius: 10px;
            padding: 18px;
        }

        .card h2 {
            font-size: 14px;
            text-transform: uppercase;
            letter-spacing: 0.05em;
            color: #38bdf8;
            margin-bottom: 12px;
        }

        .grid {
            display: grid;
            grid-template-columns: repeat(2, 1fr);
            gap: 12px;
            font-size: 14px;
        }

        .stat-label { color: #94a3b8; }
        .stat-value { font-weight: 600; color: #f8fafc; }

        .bar-bg {
            height: 12px;
            background: rgba(15, 23, 42, 0.8);
            border-radius: 6px;
            overflow: hidden;
            margin-top: 10px;
        }

        .bar-fill {
            height: 100%;
            width: 0%;
            background: linear-gradient(90deg, #38bdf8, #818cf8);
            transition: width 0.3s ease;
        }
    </style>
</head>
<body>
    <div class="window">
        <div class="titlebar">
            <span class="titlebar-title">System Pulse</span>
            <div class="window-controls">
                <button class="ctrl-btn" onclick="window.lotus.send('window-control', 'minimize')">─</button>
                <button class="ctrl-btn" onclick="window.lotus.send('window-control', 'maximize')">□</button>
                <button class="ctrl-btn close" onclick="window.lotus.send('window-control', 'close')">✕</button>
            </div>
        </div>

        <div class="content">
            <div class="card">
                <h2>Hardware & OS (via lotus.invoke)</h2>
                <div class="grid">
                    <div><span class="stat-label">Host:</span> <span class="stat-value" id="info-host">...</span></div>
                    <div><span class="stat-label">OS:</span> <span class="stat-value" id="info-os">...</span></div>
                    <div><span class="stat-label">CPU:</span> <span class="stat-value" id="info-cpu">...</span></div>
                    <div><span class="stat-label">Node.js:</span> <span class="stat-value" id="info-node">...</span></div>
                </div>
            </div>

            <div class="card">
                <h2>Live Memory Usage (via lotus.on)</h2>
                <div style="display: flex; justify-content: space-between; font-size: 14px;">
                    <span id="mem-text">Waiting for stream...</span>
                    <span id="uptime-text" class="stat-label"></span>
                </div>
                <div class="bar-bg">
                    <div class="bar-fill" id="mem-bar"></div>
                </div>
            </div>
        </div>
    </div>

    <script>
        // 1. Fetch static machine details using Promise-based invoke()
        async function loadMachineInfo() {
            const info = await window.lotus.invoke('get-machine-info');
            document.getElementById('info-host').textContent = info.hostname;
            document.getElementById('info-os').textContent = info.platform;
            document.getElementById('info-cpu').textContent = `${info.cpuModel} (${info.cores} cores)`;
            document.getElementById('info-node').textContent = info.nodeVersion;
        }

        // 2. Listen for live updates pushed from Node.js every second
        window.lotus.on('live-stats', (stats) => {
            document.getElementById('mem-text').textContent =
                `${stats.usedGb} GB / ${stats.totalGb} GB (${stats.memoryPercent}%)`;
            document.getElementById('mem-bar').style.width = `${stats.memoryPercent}%`;
            document.getElementById('uptime-text').textContent =
                `System Uptime: ${Math.floor(stats.uptimeSeconds / 60)}m ${stats.uptimeSeconds % 60}s`;
        });

        loadMachineInfo();
    </script>
</body>
</html>
```

Save both files. If `npx lotus dev main.js` is running, your window will reload with a translucent, draggable interface streaming live hardware stats from Node.js.

---

## Step 5: Leveling Up with Multi-Pane Layouts (Optional)

If you are building a workspace app (such as an IDE, a multi-column dashboard, or an app with a persistent sidebar, top toolbar, and main editor), Lotus provides a **Multi-Pane OpenGL Compositor** built into `ServoWindow`.

You can split a single window into multiple independent web views (`Pane`s) that all composite onto the **same OpenGL surface**:

```javascript
const { ServoWindow, LayoutBuilder, ipcMain, app } = require('@lotus-gui/core');
const path = require('path');

app.initVfs(); // Always call before warmup() so --encrypt production builds unlock the VFS
app.warmup();

// Define how your panes dock inside the window (order matters)
const layout = new LayoutBuilder()
    .top('titlebar', 40, { url: 'lotus-resource://localhost/titlebar.html' })
    .left('sidebar', 240, { url: 'lotus-resource://localhost/sidebar.html' })
    .bottom('terminal', 200, { url: 'lotus-resource://localhost/terminal.html', visible: false })
    .fill('editor', { url: 'lotus-resource://localhost/editor.html' });

const win = new ServoWindow({
    id: 'workspace-window',
    root: path.join(__dirname, 'ui'),
    width: 1200,
    height: 800,
    frameless: true,
    transparent: true,
    visible: false,
    // Hand window resizing over to the Rust multi-pane layout compositor
    // so a default single 'main' pane doesn't fight our docked panes:
    autoResizeMain: false,
    ...layout.config()
});

win.once('ready-to-show', () => win.show());

// Toggle the bottom terminal pane on demand.
// When you hide or show a docked pane, Rust recalculates the layout
// and expands/shrinks the 'editor' Fill pane on the next frame.
ipcMain.on('toggle-terminal', () => {
    const term = win.getPane('terminal');
    if (term) {
        term.setVisible(!term.isVisible()); // or simply: term.toggleVisible()
    }
});

// Send a message to one specific pane instead of the whole window:
ipcMain.on('file-selected', (file) => {
    win.sendToPaneRenderer('editor', 'open-file', file);
});
```

> **Full working example:** Check out [`packages/lotus-core/test_apps/multi_pane_test_app/`](./packages/lotus-core/test_apps/multi_pane_test_app/) in the repository, which demonstrates a frameless window with a top bar, collapsible bottom terminal, sidebar navigation, and a floating centered command palette overlay.

---

## Step 6: Sandboxing External or Untrusted Content (`sandbox: true`) *(Experimental — Linux Only)*

> **Status & Platform Requirements:** Process-isolated sandboxing (`sandbox: true`) is **experimental** in `v0.4.0` and currently supported on **Linux (`x86_64`) only**. It requires a Landlock-capable Linux kernel (5.13+) with unprivileged user namespaces enabled, and **will not work inside Flatpak** (which blocks nested user and network namespaces).
>
> **Fail-Closed Guarantee:** If `sandbox: true` is requested on an unsupported OS (Windows/macOS), inside Flatpak, or on a host where unprivileged user namespaces are disabled (`kernel.unprivileged_userns_clone=0` or `kernel.apparmor_restrict_unprivileged_userns=1`), Lotus **throws `E_SECURITY_ISOLATION_UNSUPPORTED_PLATFORM` immediately**—it never silently downgrades or loads the page unsandboxed.

What if part of your application needs to load an external documentation site, a third-party widget, or user-provided web content?

Loading untrusted web content in the same process as your Node.js application exposes your host environment if the renderer is compromised. Lotus **mitigates** this risk with **Process-Isolated Sandboxed Panes** (see [Threat Model, Pooling & Crash Behavior](#threat-model-pooling--crash-behavior) below).

By setting `sandbox: true` on a pane, Lotus runs that web view in an unprivileged child engine process (`PID_secure_engine`) with OS-level containment:
* **Linux Namespaces (`CLONE_NEWUSER`, `CLONE_NEWNET`, `CLONE_NEWPID`):** The engine runs with zero capabilities (`no_new_privs`) and has no direct network access; all HTTP/HTTPS traffic goes through the host `NetworkBroker`.
* **Landlock Filesystem Jailing:** Denies write access to host directories (`/`, `/home`, `/etc`, `/var`) and writes only to a private, mode-`0700` scratch directory (`/tmp/lotus-sandbox-XXXXXX`) that is recursively deleted when the engine exits.
* **Seccomp Stage B BPF Lockdown:** Synchronizes a strict syscall filter (`SECCOMP_FILTER_FLAG_TSYNC`) across all engine threads before any untrusted content is parsed, denying dangerous syscalls with `SIGSYS`.
* **Anti-SSRF Network Broker:** Outbound HTTP/HTTPS requests are proxied through the host's `NetworkBroker` with synchronous DNS socket pinning, blocking loopback (`127.0.0.1`, `::1`) and private IP ranges (RFC 1918) by default.
* **CPU Rendering & Shared Memory (SHM):** Sandboxed panes use CPU software rasterization (`SoftwareRenderingContext`) with **no GPU device nodes (`/dev/dri`) and no WebGL**. Frame pixels stream to the host OpenGL compositor over read-only sealed `memfd` buffers (`0o400`) without network hops.

### Adding a Sandboxed Web Pane

When exposing host capabilities to a sandboxed pane, grant only narrow, purpose-specific actions rather than raw credentials, and declare an explicit `returns` validation contract so extra or prototype-polluted properties are stripped before reaching the renderer:

```javascript
const { ServoWindow, LayoutBuilder, ipcMain, app } = require('@lotus-gui/core');
const path = require('path');

app.initVfs(); // Always call before warmup() so --encrypt production builds unlock the VFS
app.warmup();

// 1. Register a narrow capability method with an explicit `returns` contract (Track 2 IPC)
ipcMain.register('notes.save', async (payload) => {
    const title = typeof payload?.title === 'string' ? payload.title.slice(0, 120) : 'Untitled';
    return {
        saved: true,
        noteId: `note-${Date.now()}`,
        internalPath: '/home/user/.local/share/notes.db' // Stripped automatically: not in `returns`
    };
}, {
    channel: 'notes',
    returns: {
        saved: 'boolean',
        noteId: 'string'
    }
});

// 2. Combine trusted local UI with an untrusted sandboxed web pane
const layout = new LayoutBuilder()
    .left('sidebar', 260, { url: 'lotus-resource://localhost/sidebar.html' })
    .fill('docs-viewer', {
        url: 'https://docs.example.com',
        sandbox: true,                               // Spawns inside an unprivileged sandboxed child engine
        allowedChannels: ['notes'],                  // Strictly grants access only to the 'notes' channel
        trustedOrigins: ['https://docs.example.com'] // Bounds Track 2 IPC bridge to this origin
    });

const win = new ServoWindow({
    id: 'workspace-window',
    root: path.join(__dirname, 'ui'),
    width: 1200,
    height: 800,
    autoResizeMain: false,
    ...layout.config()
});

win.once('ready-to-show', () => win.show());
```

### Calling Node from the Sandboxed Page

When the committed page origin is listed in `trustedOrigins`, Lotus injects `window.lotus` (`invoke`, `on`, `off`) into the top-level document and authenticates a single-use ticket WebSocket (`/ipc/handshake` → `/ipc/secure-ws`).

* **If the page navigates to an origin NOT in `trustedOrigins` (or runs inside a subframe / `<iframe>`):** `window.lotus` is `undefined` (and any cross-frame attempt to call `window.parent.lotus.invoke` throws `DOMException: SecurityError`).
* **If the origin is trusted, but calls a method outside `allowedChannels`:** `window.lotus.invoke()` rejects with `E_CHANNEL_FORBIDDEN`.

```javascript
// Inside https://docs.example.com:
if (window.lotus) {
    try {
        const res = await window.lotus.invoke('notes.save', { title: 'Meeting Notes' });
        console.log('Saved note ID:', res.noteId); // res.internalPath is undefined (stripped by `returns`)
    } catch (err) {
        console.error('IPC call rejected:', err.message);
    }
} else {
    // Untrusted origin or subframe: window.lotus is undefined
}
```

### Localhost / Dev Servers & Broker Allowlisting

Because `NetworkBroker` blocks loopback (`127.0.0.1`, `localhost`) and private network ranges (RFC 1918) to prevent SSRF attacks against local services, **a `sandbox: true` pane cannot load a local dev server (`http://localhost:3000`) or intranet URL by default**—the broker will reject the request with `403 Forbidden`.

If you intentionally need a sandboxed pane to reach `localhost` during local development, opt in explicitly on the constellation manager before creating the pane:
```javascript
const { app } = require('@lotus-gui/core');
app.defaultSecureConstellation.allowLocalhost = true;
```

### Threat Model, Pooling & Crash Behavior

1. **What `sandbox: true` Mitigates vs. What Remains:**
   Process isolation and OS sandboxing mitigate renderer memory-corruption or logic bugs by preventing direct filesystem writes, arbitrary syscall execution, GPU driver exploits (since rendering is CPU-only), and local network scanning. They **do not** protect against Linux kernel vulnerabilities (privilege escalation in user namespaces, Landlock, or netfilter), side-channel attacks, or logic bugs in `ipcMain.register()` handlers that you explicitly expose via `allowedChannels`.
2. **Pooled Engine Default vs. `isolated: true`:**
   By default, multiple `sandbox: true` panes share a single pooled `lotus-engine` process (`PID_secure_engine`) to keep memory overhead low. While cookies and Track 2 IPC policies are partitioned per surface on the host, **pooled panes share the same renderer process memory and failure domain**. If you are loading mutually untrusted pages that must be isolated from *each other* as well as from the host, pass `isolated: true` to spawn a dedicated `lotus-supervisor` + `lotus-engine` process pair for that pane:
   ```javascript
   win.createPane('third-party-plugin', {
       url: 'https://untrusted-plugin.example.org',
       sandbox: true,
       isolated: true, // Dedicated OS process, namespaces, and private scratch directory
   });
   ```
3. **What Happens on a Crash:**
   If a sandboxed `lotus-engine` process crashes or is killed by Seccomp (`SIGSYS`), your main Node.js process and trusted local UI panes remain unaffected. Visually, the crashed pane stops updating (retaining its last composited frame or blanking if torn down), and `app.defaultSecureConstellation` emits a `'crashed'` event listing the affected surface IDs:
   ```javascript
   app.defaultSecureConstellation.on('crashed', ({ instanceId, code, sig, surfaces, isolated }) => {
       console.warn(`Sandboxed engine #${instanceId} exited (code=${code}, sig=${sig}) for:`, surfaces);
       // You can remove and recreate the pane, or display an error banner in your host UI
   });
   ```

---

## Step 7: Packaging Your App for Distribution

When you are ready to distribute your app, `@lotus-gui/dev` compiles your JavaScript into a Node.js Single Executable Application (SEA), bundles the native `.node` addon and your `ui/` assets, and wraps everything into a native installer.

### 1. Check your `lotus.config.json`

Make sure your `lotus.config.json` includes `"resources": ["./ui"]` (note: `lotus build` also auto-detects common folders like `ui/`, `public/`, and `assets/`, but listing it explicitly is good practice):

```json
{
    "name": "System Pulse",
    "version": "1.0.0",
    "license": "MIT",
    "description": "Lightweight desktop system monitor built with Lotus",
    "main": "main.js",
    "executableName": "system-pulse",
    "author": "Your Name",
    "resources": ["./ui"],
    "build": {
        "linux": {
            "wmClass": "system-pulse",
            "categories": ["System", "Utility"]
        }
    }
}
```

### 2. Run `lotus build`

Run the build command for your current OS and desired package format:

```bash
# On Linux: build a portable AppImage, a Debian/Ubuntu .deb, an .rpm, Pacman pkg, or Flatpak
npx lotus build --target appimage
npx lotus build --target deb
npx lotus build --target rpm

# On Windows: build an MSI installer (WiX) or an EXE installer (NSIS)
npx lotus build --platform win32 --target msi
npx lotus build --platform win32 --target exe
```

Once finished, your installer will be generated inside `dist/installers/`.

### 3. Optional: Protecting Closed-Source UI Assets (`--encrypt`)

By default, packaged desktop apps place your `ui/` assets alongside the binary (or inside `/usr/lib/<app>/`), meaning users can inspect your HTML/CSS/JS files on disk.

If you are shipping a closed-source application, add `--encrypt`:

```bash
npx lotus build --target deb --encrypt
```

When `--encrypt` is enabled:
1. Lotus skips copying your `ui/` folder to disk.
2. Instead, it encrypts every file in `ui/` using **AES-256-GCM (AEAD)** into a binary VFS blob (`LOTUS_VFS`) and shards the 32-byte master key across compiled Rust constants and native binary sections (`LOTUS_APP_S1`, `LOTUS_APP_S2`).
3. Because we included `app.initVfs()` at the top of `main.js` in Step 2, the Rust runtime derives the key in native memory and serves `lotus-resource://` files from a 128MB byte-limited LRU cache, without the key or raw files touching the disk or the Node.js V8 heap.

---

## Where to Go Next

* **[@lotus-gui/core API Reference](./packages/lotus-core/README.md):** Reference for `ServoWindow`, `LayoutBuilder` and `Pane` methods, customizing `<select>` dropdown themes, and configuring overlay scrollbars.
* **[@lotus-gui/dev CLI & Packaging Guide](./packages/lotus-dev/README.md):** Reference for `lotus.config.json`, Windows MSI/NSIS signing and WiX options, Linux packaging requirements, and the SEA build pipeline.
