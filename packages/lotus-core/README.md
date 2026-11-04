# @lotus-gui/core

The runtime engine for Lotus desktop applications.

`@lotus-gui/core` embeds the **Servo** rendering engine, native OS window management (`winit` + OpenGL/ANGLE), and a binary-packed WebSocket IPC bridge into Node.js as a native addon. Node.js runs your application logic and OS integrations directly, while Servo renders your HTML, CSS, and browser JavaScript.

* [**Step-by-Step Tutorial (Building Your First Lotus App)**](https://github.com/1Jamie/project-lotus/blob/main/TUTORIAL.md)
* [**What's New in v0.4.0 & Architecture Overview**](https://github.com/1Jamie/project-lotus/blob/main/README.md#whats-new-in-v040)
* [**`@lotus-gui/dev` CLI & Packager Docs**](https://github.com/1Jamie/project-lotus/blob/main/packages/lotus-dev/README.md)

---

## Installation

```bash
npm install @lotus-gui/core
```

Pre-built native `.node` binaries are included for **Linux (`x86_64`)** and **Windows (`x64`)**. See the [root README platform notes](https://github.com/1Jamie/project-lotus/blob/main/README.md#linux-runtime-notes) for details on Linux `GLIBC_TUNABLES` handling and Windows ANGLE setup.

---

## 30-Second Quick Start

```javascript
const { ServoWindow, ipcMain, app } = require('@lotus-gui/core');
const path = require('path');

// 1. Unlock encrypted VFS (safe no-op if not built with --encrypt) and pre-warm Servo
app.initVfs();
app.warmup();

// 2. Open a transparent, frameless window serving local UI files
const win = new ServoWindow({
    id: 'main-window',                // Automatically saves & restores window position/size
    root: path.join(__dirname, 'ui'), // Serves files via lotus-resource://localhost/
    index: 'index.html',
    width: 1024,
    height: 768,
    title: 'My Lotus App',
    frameless: true,
    transparent: true,
    visible: false                    // Wait until the first frame is painted before showing
});

// 3. Reveal the window with zero white-flash
win.once('ready-to-show', () => win.show());

// 4. Respond to `await window.lotus.invoke('ping', { msg: 'hello' })` from the webpage
ipcMain.handle('ping', async ({ msg }) => {
    return { reply: `Pong: ${msg}`, pid: process.pid };
});
```

---

## API Reference

```javascript
const { ServoWindow, LayoutBuilder, Anchor, ipcMain, app } = require('@lotus-gui/core');
```

### 1. `app`: Application Lifecycle

`app` controls global engine initialization, VFS mounting, and shutdown.

| Method | Returns | Description |
|--------|---------|-------------|
| `app.initVfs()` | `void` | Initializes the native Encrypted VFS. **Must be called before `app.warmup()`.** If your app was not packaged with `--encrypt`, this safely skips itself. |
| `app.warmup()` | `void` | Pre-initializes the Servo rendering thread and IPC server before you create your first window. |
| `app.getSystemTheme()` | `'light' \| 'dark'` | Returns the current OS color scheme preference. |
| `app.quit()` | `void` | Requests a graceful shutdown, closing open windows before exiting (with a 1s fallback exit timer). |
| `app.exit(code = 0)` | `void` | Immediately terminates the application process with the given exit code. |

---

### 2. `ServoWindow`: Window Management

Creates and manages a native OS window backed by Servo. Extends Node's `EventEmitter`.

```javascript
const win = new ServoWindow(options);
// Or pass a URL string directly:
// const win = new ServoWindow('https://servo.org');
```

> **Note on Remote URLs & IPC:** For security, the `window.lotus` IPC credentials (`token` and `port`) are only populated on `lotus-resource://` pages and `localhost` / `127.0.0.1` origins. When a window or pane loads an external remote URL (such as `https://servo.org`), `window.lotus.token` and `window.lotus.port` are `null` and IPC methods are disabled on that page.

#### Constructor Options (`WindowOptions`)

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `id` | `string` | Random UUID | Unique identifier for the window. **Pass a static ID (e.g. `'main'`) to enable automatic window position & size persistence.** |
| `root` | `string` | `undefined` | Absolute path to your local UI directory (e.g. `path.join(__dirname, 'ui')`). Enables Hybrid Mode (`lotus-resource://localhost/`) and maps automatically to the Encrypted VFS in `--encrypt` builds. |
| `index` | `string` | `'index.html'` | Entry HTML file relative to `root`. |
| `initialUrl` | `string` | `'about:blank'` | Direct URL to load (used when `root` is not set). |
| `width` | `number` | `1024` | Initial window width in logical pixels. |
| `height` | `number` | `768` | Initial window height in logical pixels. |
| `title` | `string` | `'Lotus'` | Native window title. |
| `frameless` | `boolean` | `false` | Removes the native OS title bar and window frame so you can build custom chrome (`frame: false` is also supported as an alias). |
| `transparent` | `boolean` | `false` | Enables OS-level window transparency (`0x00000000` clear color). |
| `cornerRadius` | `number` | `undefined` | Corner radius hint for rounded transparent windows. |
| `visible` | `boolean` | `true` | Whether to show the window immediately. Set to `false` and call `win.show()` inside `win.once('ready-to-show', ...)` for zero white-flash. |
| `resizable` | `boolean` | `true` | Whether the user can resize the window. |
| `maximized` | `boolean` | `false` | Start the window maximized. |
| `fullscreen` | `boolean` | `false` | Start the window in fullscreen mode. |
| `alwaysOnTop` | `boolean` | `false` | Keep the window floating above other OS windows. |
| `restoreState` | `boolean` | `true` | Restore previous position, size, and maximized state when `id` is provided. |
| `wmClass` | `string` | `undefined` | Linux Window Manager class for taskbar grouping. |
| `autoResizeMain` | `boolean` | `true` | Automatically anchor the default `'main'` pane to `Anchor.Fill` when using a single-pane window. Set to `false` when passing custom `panes`. |
| `panes` | `PaneOptions[]` | `[]` | Array of panes to initialize atomically with the window (typically generated via `layout.config().panes`). |
| `sandbox` | `boolean` | `false` | Run the window's panes inside an unprivileged, OS-sandboxed child engine (`PID_secure_engine`) with Landlock FS jailing, Seccomp Stage B lockdown, and anti-SSRF network isolation (`secure: true` is also supported as a backwards-compatible alias). |
| `allowedChannels` | `string[]` | `[]` | Track 2 capability channels authorized for sandboxed panes in this window. |
| `selectMenu` | `SelectMenuOptions` | `{ enabled: true, theme: 'auto' }` | Customize or disable the native `<select>` dropdown overlay pane. |
| `scrollbars` | `boolean \| ScrollbarOptions` | `true` | Configure or disable injected overlay scrollbars (`{ enabled, theme: 'auto' \| 'light' \| 'dark', autoHide: true, width: 8 }`). |

#### Window Methods

| Method | Description |
|--------|-------------|
| `win.show()` / `win.hide()` | Show or hide the window. |
| `win.focus()` | Bring the window to the front and focus input. |
| `win.minimize()` / `win.unminimize()` | Minimize or restore the window. |
| `win.maximize()` / `win.unmaximize()` | Maximize or unmaximize the window. |
| `win.isMaximized()` | Returns `true` if the window is currently maximized (updated automatically on OS/window resize events). |
| `win.toggleMaximize()` | Maximizes or unmaximizes the window based on `win.isMaximized()` and returns the new boolean state. |
| `win.close()` | Close and destroy the window. When the last open window closes, Lotus quits automatically. |
| `win.setTitle(title)` | Change the window title at runtime. |
| `win.setSize(width, height)` | Resize the window programmatically. |
| `win.setMinSize(width, height)` | Enforce minimum window dimensions (pass `0, 0` to clear). |
| `win.setMaxSize(width, height)` | Enforce maximum window dimensions (pass `0, 0` to clear). |
| `win.setPosition(x, y)` | Move the window to specific screen coordinates. |
| `win.loadUrl(url)` | Navigate the window's `'main'` pane to a new URL. |
| `win.executeScript(js)` | Execute arbitrary JavaScript inside the `'main'` pane's DOM context. |
| `win.sendToRenderer(channel, data, immediate?)` | Send an IPC message to the `'main'` pane (`window.lotus.on(channel, ...)`). |
| `win.sendToPaneRenderer(paneId, channel, data, immediate?)` | Send an IPC message to a specific named pane inside this window. |
| `win.createPane(id, options?)` | Create and attach a new `Pane` web view (`win.addPane` is an alias). Returns the `Pane` instance. |
| `win.getPane(id)` | Retrieve a `Pane` instance by its ID (or via the `win.panes` Map). |
| `win.removePane(id)` | Destroy a custom pane and free its Servo web view resources (note: `'main'` cannot be removed). |
| `win.setSelectMenuTheme(theme)` | Change the `<select>` overlay theme at runtime (`'auto'`, `'light'`, `'dark'`, or a `SelectMenuTheme` object). |
| `win.setSelectMenuCss(css)` | Inject custom CSS rules into the `<select>` dropdown overlay. |

#### Window Events

| Event | Callback Signature | When It Fires |
|-------|--------------------|---------------|
| `'ready-to-show'` | `()` | **First frame painted.** Best moment to call `win.show()` when starting with `visible: false`. (Alias: `'frame-ready'`) |
| `'ready'` | `(data)` | The webpage DOM and IPC bridge have initialized. (Alias: `'dom-ready'`) |
| `'load-status'` | `(status: 'started' \| 'head-parsed' \| 'complete', paneId: string)` | Navigation lifecycle updates for any pane in the window. |
| `'title-changed'` | `(title: string, paneId: string)` | `<title>` changed in one of the window's panes. |
| `'resize'` | `({ width, height, logicalWidth, logicalHeight, maximized })` | Window resized or maximized state changed. Includes physical (`width`/`height`) and DPI-scaled logical (`logicalWidth`/`logicalHeight`) dimensions. Throttled to 25ms. (Alias: `'resized'`) |
| `'moved'` | `({ x, y })` | Window moved on screen. |
| `'focus'` / `'blur'` | `()` | Window gained or lost input focus. |
| `'close'` / `'closed'` | `()` | Window was closed. |
| `'select-menu'` | `(event: SelectMenuEvent)` | A `<select>` element was clicked. Call `event.preventDefault()` if you want to render your own custom dropdown UI. |
| `'file-hover'` | `({ path: string })` | An OS file is being dragged over the window (fires once per file). |
| `'file-hover-cancelled'` | `()` | A file drag left the window without dropping. |
| `'file-drop'` | `({ path: string })` | A file was dropped onto the window from the OS file manager. |

---

### 3. `LayoutBuilder`, `Pane` & `Anchor`: Multi-Pane Compositor

Lotus lets you run multiple independent web views (`Pane`s) inside a single OS window on a single shared OpenGL context. Every pane has its own DOM, URL, and `window.lotus` IPC bridge, without OS child-window positioning lag.

#### Declarative Layouts with `LayoutBuilder`

`LayoutBuilder` computes dock order automatically. Docked panes claim space from the edges inward in the order you declare them, and `.fill()` takes the remaining center rectangle:

```javascript
const { ServoWindow, LayoutBuilder } = require('@lotus-gui/core');
const path = require('path');

const layout = new LayoutBuilder()
    .top('controls', 40, { url: 'lotus-resource://localhost/controls.html' })
    .left('sidebar', 260, { url: 'lotus-resource://localhost/sidebar.html' })
    .bottom('terminal', 220, { url: 'lotus-resource://localhost/terminal.html', visible: false })
    .fill('content', { url: 'lotus-resource://localhost/content.html' })
    .absolute('palette', 300, 150, 600, 400, {
        url: 'lotus-resource://localhost/palette.html',
        zIndex: 100,
        visible: false
    });

const win = new ServoWindow({
    id: 'workspace',
    root: path.join(__dirname, 'ui'),
    width: 1200,
    height: 800,
    frameless: true,
    transparent: true,
    visible: false,
    autoResizeMain: false,
    ...layout.config() // Passes `{ panes: [...] }`
});
```

| `LayoutBuilder` Method | Description |
|------------------------|-------------|
| `.left(id, width, options?)` | Dock a pane to the left edge with a fixed width (`Anchor.Left`). |
| `.right(id, width, options?)` | Dock a pane to the right edge with a fixed width (`Anchor.Right`). |
| `.top(id, height, options?)` | Dock a pane to the top edge with a fixed height (`Anchor.Top`). |
| `.bottom(id, height, options?)` | Dock a pane to the bottom edge with a fixed height (`Anchor.Bottom`). |
| `.fill(id, options?)` | Fill all remaining space after docked panes are placed (`Anchor.Fill`). |
| `.absolute(id, x, y, width, height, options?)` | Position a floating/overlay pane at explicit logical coordinates (`Anchor.None`). Use `zIndex` in `options` to layer above docked panes. |
| `.config()` | Returns `{ panes: PaneOptions[] }` ready to spread into `new ServoWindow({ ... })`. |

#### Managing Individual `Pane` Instances

Every pane created via `LayoutBuilder` or `win.createPane(id, options)` is stored in `win.panes` and accessible via `win.getPane(id)`.

When you call `pane.setVisible(false)` or `pane.toggleVisible()` on a docked pane (such as a collapsible sidebar or bottom terminal), the Rust compositor automatically recalculates the remaining space and expands or shrinks your `Fill` pane on the next frame.

| `Pane` Method / Event | Description |
|-----------------------|-------------|
| `pane.setVisible(bool)` | Show or hide the pane. Triggers an automatic native layout reflow for docked panes. |
| `pane.isVisible()` | Returns `true` if the pane is currently visible, `false` otherwise (also accessible via `pane.visible`). |
| `pane.toggleVisible()` | Flips the pane's visibility state (`!pane.isVisible()`) and returns the new boolean state. |
| `pane.setRect(x, y, width, height)` | Reposition and resize an `Anchor.None` (floating/absolute) pane in logical pixels. |
| `pane.focus()` | Direct keyboard input focus to this pane. |
| `pane.loadUrl(url)` | Navigate this specific pane to a new URL. |
| `pane.executeScript(js)` | Run JavaScript inside this pane's DOM context. |
| `pane.remove()` | Destroy this pane and remove it from the window (`win.removePane(pane.id)`). |
| `pane.on('load-status', (status) => ...)` | Listen for `'started'`, `'head-parsed'`, or `'complete'` on this pane. |
| `pane.on('title-changed', (title) => ...)` | Listen for document title changes on this pane. |

---

### 4. Process-Isolated Sandboxed Panes (`sandbox: true`)

Lotus features an OS-level multi-process security sandbox designed specifically for hosting untrusted content, external web pages, or third-party web views safely alongside your trusted host UI.

By passing `sandbox: true` when creating a pane (or on the window), Lotus runs the pane inside a dedicated, unprivileged child engine process (`PID_secure_engine`) managed by the `lotus-supervisor` daemon. (`secure: true` is also supported as a backwards-compatible alias).

#### Quick Example: Embedding an Isolated Web Pane

```javascript
const { ServoWindow, LayoutBuilder, ipcMain } = require('@lotus-gui/core');
const path = require('path');

// 1. Authorize specific capability methods for the sandboxed pane (Track 2 IPC)
ipcMain.register('vault.getSecret', async () => {
    return { token: 'secret-token-123' };
}, {
    channel: 'vault',
    schema: { token: 'string' }
});

// 2. Build a layout combining trusted local UI with an untrusted sandboxed web pane
const layout = new LayoutBuilder()
    .left('sidebar', 260, { url: 'lotus-resource://localhost/sidebar.html' })
    .fill('browser', {
        url: 'https://example.com',
        sandbox: true,                          // Spawns inside an isolated, sandboxed child engine
        allowedChannels: ['vault'],             // Strictly grants access only to the 'vault' channel
        trustedOrigins: ['https://example.com'] // Bounds Track 2 IPC credentials to this origin
    });

const win = new ServoWindow({
    id: 'my-workspace',
    root: path.join(__dirname, 'ui'),
    width: 1280,
    height: 800,
    autoResizeMain: false,
    ...layout.config()
});

// You can also create sandboxed panes dynamically at runtime:
const runtimePane = win.createPane('untrusted-tab', {
    url: 'https://untrusted-site.org',
    sandbox: true,
    allowedChannels: [] // Zero IPC access by default
});
```

#### Calling from the Sandboxed Webpage (`window.lotus`)

In a sandboxed pane, `window.lotus` uses the capability-gated **Track 2 IPC bridge**:

```javascript
// Sandboxed pages use invoke() for authorized capability channels
try {
    const res = await window.lotus.invoke('vault.getSecret', {});
    console.log('Received authorized secret:', res.token);
} catch (err) {
    console.error('Call rejected or channel unauthorized:', err.message);
}

// Listening for host events on granted channels
window.lotus.on('vault.alert', (data) => {
    console.log('Host security alert:', data);
});
```

#### Multi-Layer Sandboxing Architecture

When `sandbox: true` is enabled, five coordinated defense layers protect your host system:

```text
[ Node.js Host Process (PID_main) ]
  ├── OpenGL / ANGLE Compositor (Window Blit via Read-Only SHM)
  ├── HostIpcPolicyRouter (Track 2 Verification & Origin Derivation)
  ├── Axum IPC Gateway (127.0.0.1:<port> - Token Auth & Rate Limiting)
  └── NetworkBroker (Strict Anti-SSRF Proxy & Cookie Store)
          ▲
          │ SCM_RIGHTS Descriptors / Unix Domain Sockets
          ▼
[ lotus-supervisor (Stage A Setup Daemon) ]
  └── Unprivileged Namespaces (CLONE_NEWUSER | CLONE_NEWPID | CLONE_NEWNET | CLONE_NEWNS | CLONE_NEWIPC)
          │
          ▼ execve()
[ lotus-engine (PID_secure_engine) ]
  ├── Landlock ABI v1-v5 (Zero host write access; isolated 0o700 sandbox dir; no /tmp access)
  ├── Stage B Seccomp BPF (SECCOMP_FILTER_FLAG_TSYNC on all 77 Servo threads; kills chmod/mount/ptrace)
  ├── Dual-Plane Sealed SHM (pixel_rw / ack_ro; sealed memfd with fchmod(0o400) locked down)
  ├── lotus-net-proxy (Loopback forwarder bridging egress across broker_socket)
  └── UserContentManager (Injected client bridge with calling-realm stack inspection)
```

1. **Linux OS Namespaces (`CLONE_NEWUSER`, `CLONE_NEWNET`, `CLONE_NEWPID`, `CLONE_NEWNS`, `CLONE_NEWIPC`):**
   The sandboxed engine runs in its own user namespace with zero Linux capabilities (`no_new_privs`), an isolated PID namespace (`PID 1`), and a dedicated network namespace with only a loopback interface.
2. **Landlock Filesystem Jailing & Directory Jailing:**
   The supervisor applies Landlock rules prohibiting write, create, or delete access to host directories (including `/`, `/tmp`, `/home`, `/etc`). The engine only receives read access to system fonts and shared libraries, plus isolated write access strictly to a private `0o700` temporary directory (`/tmp/lotus-sandbox-XXXXXX`) that is immediately purged upon process exit.
3. **Stage B Seccomp BPF Syscall Lockdown:**
   Before parsing untrusted HTML or executing JavaScript, `lotus-engine` synchronizes a strict seccomp BPF filter across all 77 internal Servo threads (`SECCOMP_FILTER_FLAG_TSYNC`). Dangerous syscalls—including `SYS_fchmod`, `SYS_fchmodat`, `SYS_ptrace`, `SYS_mount`, `SYS_execve`, `SYS_unshare`, and `SYS_setns`—are denied with immediate `SIGSYS` termination.
4. **Zero-Copy Double-Mapped Shared Memory (SHM):**
   Rendered pixels stream back to the host window compositor via double-mapped POSIX `memfd` buffers. The engine only receives write access to its pixel buffer, while the host retains read-only descriptors sealed with `0o400` permissions. Frame synchronization uses lock-free atomic Seqlocks (`pixel_rw` / `pixel_ro`, `ack_rw` / `ack_ro`), ensuring tear-free rendering at 60+ FPS with zero network overhead.
5. **Anti-SSRF NetworkBroker Egress Proxying:**
   Because the sandboxed engine has no external network access inside `CLONE_NEWNET`, all HTTP/HTTPS traffic is tunneled through the host's `NetworkBroker`:
   - **Loopback & Private Range Blocking:** Rejects private networks (RFC 1918), link-local, loopback (`127.0.0.1`), CGNAT, IPv6 ULA, and 6to4 addresses.
   - **Synchronous DNS Socket Pinning:** Resolves all `A` and `AAAA` records and pins the verified IP address directly at socket creation, defeating DNS rebinding and TOCTOU attacks.
   - **RFC 6265 Host Cookie Store:** Stores cookies securely on the host partitioned by `(surfaceId, topLevelSite)` with strict `HttpOnly` isolation.
   - **Quotas & Timeouts:** Imposes a strict 64 MiB response quota, 30s connection timeouts, and strips authorization headers across cross-origin redirects.

#### Track 2 Authenticated IPC Protocol

Sandboxed panes do not have access to arbitrary `ipcMain.handle()` or `ipcMain.send()` channels. Instead, they use capability-based **Track 2 IPC**:
* **Capability Gating:** You explicitly declare which channels a surface may reach using `allowedChannels: ['channelName']`. Any call outside granted channels is rejected immediately.
* **Schema Validation:** In `ipcMain.register(method, handler, { channel, schema })`, Lotus verifies parameter types and strips object prototypes, outputting strictly onto `Object.create(null)` objects to prevent prototype pollution.
* **Single-Use 256-Bit Tickets:** During navigation, the bridge performs a constant-time handshake (`/ipc/handshake`) with a host bootstrap secret, acquiring a single-use 256-bit ticket that authenticates the WebSocket upgrade (`/ipc/secure-ws`). Replay attacks and brute-force attempts trigger automatic rate limiting.
* **Calling-Realm Stack Guard:** The client-side bridge inspects invocation call-stacks and prototype lineages, immediately blocking malicious subframes (such as same-origin or `about:srcdoc` `<iframe>` elements) with `DOMException: SecurityError` (code 18).
* **Automatic Crash & Restart Recovery:**
   If a sandboxed engine experiences an unhandled panic or process failure, `SecureConstellationManager` detects the exit code, purges the dead process, logs the fault, and safely transparently restarts the secure constellation on subsequent requests without crashing your main Node.js process.

---

### 5. `ipcMain` & `window.lotus`: IPC Reference

Lotus pairs a local WebSocket server (`tokio` + `axum`, authenticated via per-session UUID tokens and header checks; see [IPC Security Model](https://github.com/1Jamie/project-lotus/blob/main/README.md#ipc-security-model)) with **MsgPack** binary serialization, adaptive batching (up to 800 messages or 1MB per burst), and 128KB chunked streaming for large transfers.

#### IPC Patterns

| Pattern | Renderer (`window.lotus`) | Main Process (`ipcMain` / `win`) | Best Used For |
|---------|---------------------------|----------------------------------|---------------|
| **Request / Reply** | `const res = await window.lotus.invoke(ch, data)` | `ipcMain.handle(ch, async (data) => ...)` | Queries, reading files, OS calls, or any async operation that returns a value or throws an error. |
| **Renderer → Node** | `window.lotus.send(ch, data)` | `ipcMain.on(ch, (data, clientId) => ...)` | Fire-and-forget UI events, button clicks, window control commands, binary `Blob` uploads. |
| **Node → Window/Pane** | `window.lotus.on(ch, (data) => ...)` | `win.sendToRenderer(ch, data)` or `win.sendToPaneRenderer(paneId, ch, data)` | Pushing live updates to a specific window or pane. |
| **Node → Broadcast** | `window.lotus.on(ch, (data) => ...)` | `ipcMain.send(ch, data)` | Broadcasting a global state update to every open window. |
| **Node → Client ID** | `window.lotus.on(ch, (data) => ...)` | `ipcMain.sendTo(clientId, ch, data)` | Replying directly to whichever `windowId` or `windowId:paneId` sent an `ipcMain.on` message. |

#### Multi-Pane & Multi-Window IPC Routing

When a pane sends a message via `window.lotus.send('my-event', payload)`, `ipcMain.on` passes the sender's `clientId` (`"windowId"` or `"windowId:paneId"`) as the second argument:

```javascript
ipcMain.on('my-event', (data, clientId) => {
    console.log(`Message from ${clientId}:`, data);
    // Reply only to the exact window/pane that sent the message:
    ipcMain.sendTo(clientId, 'my-ack', { received: true });
});
```

When using `ipcMain.handle(channel, async (data) => ...)`, Lotus automatically routes the resolved value (or thrown error) back to the exact originating window and pane.

---

## Core Concepts & UI Customization

### Hybrid Mode (`lotus-resource://`)

When you pass `root: path.join(__dirname, 'ui')` to `ServoWindow`, Lotus serves your frontend assets directly through a custom `lotus-resource://localhost/` protocol handler in Rust:
* **No local HTTP server:** Avoids port binding and firewall prompts.
* **Directory jailing:** Path traversal (`../../`) outside `root` is blocked at the Rust protocol layer.
* **VFS integration:** Switching between unencrypted dev mode and `--encrypt` production builds requires no changes to your asset URLs.

### Frameless Windows, Drag Regions & Resize Borders

When `frameless: true` is enabled:
1. **Drag Regions:** Mark any titlebar or header element with `-webkit-app-region: drag` in CSS or `data-lotus-drag="true"` in HTML. Add `-webkit-app-region: no-drag` (or `data-lotus-drag="false"`) to buttons inside that region so they remain clickable.
2. **DPI-Scaled Resize Borders:** Lotus creates a 6px (DPI-scaled, 4px minimum) hit zone around all edges and corners of the window in Rust. Hovering the perimeter displays native OS directional resize cursors and click-dragging resizes the window, even when an overlay scrollbar or header sits flush against the edge.

### Customizing Native `<select>` Dropdowns

Because Servo does not open OS-native popup menus for `<select>` tags on its own, Lotus creates a high-z-index overlay pane (`__lotus_select_overlay`) that pops up at the screen coordinates of any clicked `<select>` element and flips upward automatically if the dropdown is near the bottom of the window.

```javascript
const win = new ServoWindow({
    selectMenu: {
        theme: {
            base: 'dark',           // 'light', 'dark', or 'auto'
            bg: '#0f172a',
            border: '#334155',
            text: '#f8fafc',
            hover: '#1e293b',
            active: '#38bdf8',
            activeText: '#0f172a',
            borderRadius: 8,
            maxHeight: 300
        }
    }
});

// Or intercept the event to render your own custom UI:
win.on('select-menu', (event) => {
    // event.preventDefault();
    // event.submit([selectedIndex]);
});
```

### Protecting Assets with the Encrypted VFS

When packaging with `npx lotus build --encrypt` (via `@lotus-gui/dev`), your `ui/` directory is packed into an **AES-256-GCM (AEAD)** encrypted binary blob injected into the executable, while the 32-byte master key is sharded across Rust constants and native ELF/PE sections.

Call `app.initVfs()` before `app.warmup()` in your `main.js`. At runtime, Rust reconstructs the key in native memory and serves decrypted assets through a **128MB byte-limited LRU cache**, so the decryption key never touches the Node.js V8 heap or disk. See [A Note on Encrypted VFS & Security](https://github.com/1Jamie/project-lotus/blob/main/README.md#a-note-on-encrypted-vfs--security) for details on threat models.

---

## Building From Source

For contributor build instructions and Linux/Windows system prerequisites, see [Building the Engine from Source in the root README](https://github.com/1Jamie/project-lotus/blob/main/README.md#building-the-engine-from-source-for-contributors) and [`CONTRIBUTING.md`](https://github.com/1Jamie/project-lotus/blob/main/packages/lotus-core/CONTRIBUTING.md).

## Package Structure

```text
@lotus-gui/core
├── src/lib.rs               # Rust N-API bindings, Servo compositor, winit loop & Axum IPC
├── src/window_state.rs      # Automatic window position/size persistence
├── src/platform.rs          # OS-specific theme & cursor integrations
├── lotus.js                 # High-level JS API (ServoWindow, LayoutBuilder, Pane, IpcMain, app)
├── lotus.d.ts               # TypeScript definitions
├── index.js                 # Native .node binary loader
├── resources/               # Injected IPC bridge, scrollbar, and <select> overlay UI
├── test_apps/
│   ├── test_app/            # Single-window showcase app
│   ├── multi_pane_test_app/ # Multi-pane workspace & command palette showcase
│   └── secure_test_app/     # Process-isolated sandbox & Track 2 showcase
```

## License

MIT
