# @lotus-gui/core

**The high-performance, low-latency runtime engine at the heart of the Lotus ecosystem.**

`@lotus-gui/core` provides the Servo rendering engine, window management, and IPC system as a Node.js native addon. It is designed to be the "steering wheel" for your application, allowing Node.js to own the OS logic while Servo paints the pixels.

---

> ### ⚠️ IMPORTANT
>
> **NEW SUPPORT FOR ENCRYPTED APPS with AEAD-ENCRYPTED (AES-256-GCM) VFS**
> The latest version adds support for packaging your app in a VFS with a natively derived key. This allows developers to ship closed-source applications while still leveraging web-based frontends. The decryption key is sharded across the native binary and never touches the V8 heap or the Node.js environment.
>
> **Disclaimer about Encryption and Closed-Source Software**: Lotus supports encryption to protect your intellectual property. However, the encryption layer relies on secrets injected during the build process. Anyone with access to the decrypted application in memory (e.g., using debugging tools like WinDbg, Frida, or manual memory inspection) may eventually extract these secrets. Users should be aware that client-side encryption is primarily a deterrent against casual reverse engineering and unauthorized distribution, not a guarantee of absolute security against determined adversaries. As always, security in distributed binaries is a game of making extraction difficult enough that most people won't bother. It is fundamentally impossible to keep assets perfectly secure in the presence of a dedicated reverse engineer.

## 🚀 What’s New in v0.3.2 (Encrypted VFS & Stability)

This release brings support for strictly closed-source applications and significant stability improvements to the Lotus runtime.

*   **🔒 Asset Protection (Encrypted VFS):** Want to ship a closed-source app? Lotus uses an AEAD-encrypted (AES-256-GCM) Virtual File System where the decryption key **never touches JavaScript or the V8 heap**. 
*   **⚡ Native Key Sharding:** The CLI shards the master key across compiled Rust constants and native binary sections (ELF/PE). The Rust core autonomously derives the key in protected memory.
*   **🧊 Byte-Limited LRU Cache:** We mitigate decryption overhead with a strict 128MB byte-limited LRU cache. Once an asset is loaded, it's served from memory at blistering speeds without risking an Out-Of-Memory (OOM) panic on heavy media files.
*   **🎨 Ghost-Mode Transparency:** True OS-level transparency with zero white-flash on startup. If your CSS is transparent, your window is transparent.
*   **⚡ Fast IPC:** The WebSocket bridge ensures the UI thread remains responsive even under heavy loads with MsgPack batching and opportunistic flushing.

---

## Installation

```bash
npm install @lotus-gui/core
```

> **Note:** Pre-built binaries are available for **Linux (x86_64)** and **Windows (x64)**. Manual compilation is typically not required.

---

## API Reference

### Exports

```javascript
const { ServoWindow, ipcMain, app } = require('@lotus-gui/core');
```

### `app`

The application lifecycle controller.

| Method | Description |
|--------|-------------|
| `app.initVfs()` | Initialize the Encrypted VFS natively. Must be called before `warmup()`. If the app wasn't built with `--encrypt`, this safely skips itself. |
| `app.warmup()` | Pre-initialize the Servo engine. Call before creating windows for faster startup. |
| `app.quit()` | Shut down the application and close all windows. |

```javascript
app.initVfs(); // Initialize secure VFS (if present)
app.warmup();  // Pre-warm the engine
```

### `ServoWindow`

Creates and manages a native window powered by the Servo rendering engine. Extends `EventEmitter`.

#### Constructor Options

```javascript
const win = new ServoWindow(options);
```

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `id` | `string` | Random UUID | Unique identifier for the window. **Required for state persistence.** |
| `root` | `string` | `undefined` | Absolute path to the UI directory. Enables Hybrid Mode (`lotus-resource://`). If using an encrypted build, this path will map to the VFS internally. |
| `index` | `string` | `'index.html'` | Entry HTML file relative to `root`. |
| `initialUrl` | `string` | -- | URL to load (alternative to `root` + `index`). |
| `width` | `number` | `1024` | Initial window width in pixels. |
| `height` | `number` | `768` | Initial window height in pixels. |
| `title` | `string` | `'Lotus'` | Window title. |
| `transparent` | `boolean` | `false` | Enable OS-level window transparency. |
| `visible` | `boolean` | `true` | Show window immediately. Set to `false` and show after `frame-ready` to avoid a white flash during loading. |
| `frameless` | `boolean` | `false` | Remove the native window frame to enable custom title bars and drag regions. |
| `resizable` | `boolean` | `true` | Allow the user to resize the window. |
| `maximized` | `boolean` | `false` | Start the window in a maximized state. |
| `fullscreen` | `boolean` | `false` | Start the window in fullscreen mode. |
| `alwaysOnTop` | `boolean` | `false` | Keep the window above all other windows. |
| `restoreState` | `boolean` | `true` | Automatically restore previous position and size (requires `id`). |

#### Methods

| Method | Description |
|--------|-----------|
| `win.loadUrl(url)` | Navigate the window to a new URL. |
| `win.executeScript(js)` | Execute arbitrary JavaScript in the renderer context. |
| `win.sendToRenderer(channel, data)` | Send a message to the renderer on a named channel. |
| `win.setTitle(title)` | Update the window title. |
| `win.setDecorations(bool)` | Toggle native window decorations at runtime. |
| `win.setSize(width, height)` | Resize the window programmatically. |
| `win.setMinSize(width, height)` | Set a minimum window size. Pass `0, 0` to remove constraints. |
| `win.setMaxSize(width, height)` | Set a maximum window size. Pass `0, 0` to remove constraints. |
| `win.setPosition(x, y)` | Move the window to specific screen coordinates. |
| `win.minimize()` | Minimize the window. |
| `win.unminimize()` | Restore the window from a minimized state. |
| `win.maximize()` | Maximize the window. |
| `win.unmaximize()` | Restore the window from a maximized state. |
| `win.focus()` | Bring the window to the front and request input focus. |
| `win.show()` | Make the window visible. |
| `win.hide()` | Hide the window. |
| `win.close()` | Close and destroy the window instance. |

#### Events

| Event | Callback | Description |
|-------|----------|-------------|
| `'ready'` | `()` | Window has been successfully created and is ready for use. |
| `'frame-ready'` | `()` | The first frame has been rendered. Recommended time to call `show()`. |
| `'load-status'` | `(status: string)` | Page load status changed (`'loading'`, `'complete'`). |
| `'moved'` | `({ x: number, y: number })` | Window position has changed. |
| `'resize'` | `({ width: number, height: number })` | Window size has changed. |
| `'focus'` | `()` | Window gained focus. |
| `'blur'` | `()` | Window lost focus. |
| `'closed'` | `()` | Window was closed and destroyed. |
| `'file-hover'` | `({ path: string })` | A file is being dragged over the window. Fires once per file. |
| `'file-hover-cancelled'` | `()` | A drag operation left the window without dropping. |
| `'file-drop'` | `({ path: string })` | A file was dropped onto the window. Fires once per file -- accumulate multiple events if you need multi-file support. |

```javascript
const win = new ServoWindow({
    id: 'main',
    root: path.join(__dirname, 'ui'),
    transparent: true,
    visible: false // Don't show until ready
});

// Show only after first frame (prevents white flash)
win.once('frame-ready', () => win.show());
```

---

### `ipcMain`

Handles communication between the Node.js main process and the Servo renderer.

#### Main Process

```javascript
const { ipcMain } = require('@lotus-gui/core');

// Listen for messages from the renderer (fire-and-forget)
ipcMain.on('my-channel', (data) => {
    console.log('Received:', data);
});

// Push data to all open windows
ipcMain.send('status-update', { online: true });

// Handle requests from the renderer (request/reply)
ipcMain.handle('get-data', async ({ id }) => {
    return await database.fetch(id); 
});
```

#### Renderer (Webpage)

The `window.lotus` bridge is automatically available in every page.

```javascript
// Send a message to the main process
window.lotus.send('my-channel', { message: 'Hello!' });

// Send binary data
window.lotus.send('upload', new Blob(['data']));

// Listen for messages from the main process
window.lotus.on('status-update', (data) => {
    console.log('Server status:', data.online);
});

// Invoke a remote method and await the result
const user = await window.lotus.invoke('get-user', { id: 42 });
console.log(user.name);
```

#### IPC Pattern Reference

| Pattern | Renderer | Node.js | Use when |
|---------|----------|---------|----------|
| Fire-and-forget | `lotus.send(ch, data)` | `ipcMain.on(ch, fn)` | Notifications, events |
| Push from Node | `lotus.on(ch, fn)` | `ipcMain.send(ch, data)` | Server-initiated updates |
| **Request/reply** | `await lotus.invoke(ch, data)` | `ipcMain.handle(ch, async fn)` | **Queries, CRUD, any async call** |

> **Note:** `handle` and `on` can coexist on the same channel. `handle` only fires when the message includes a `_replyId` (i.e., sent via `invoke`). Plain `send` calls still reach `on` listeners.

---

## Core Concepts

### Hybrid Mode (`lotus-resource://`)

Lotus serves your UI files directly from disk via a custom protocol, eliminating the need for a local web server.

*   **Performance:** Zero network overhead for local file serving.
*   **Security:** Enforces strict directory jailing; files outside the specified `root` cannot be accessed.
*   **Reliability:** Avoids port collisions and local firewall issues.

```javascript
const win = new ServoWindow({
    root: path.join(__dirname, 'ui'),  // Files served from here
    index: 'index.html'               // Entry point
});
// Internally loads: lotus-resource://localhost/index.html
```

### 🔒 The Encrypted VFS

If you are building proprietary software and don't want users simply unzipping your binary to steal your assets, use the `--encrypt` flag during build (see `@lotus-gui/dev`).

When encrypted, Lotus packs your `ui/` folder into a high-entropy binary blob injected directly into the executable. Decryption happens entirely in native memory, ensuring your code is never exposed to the JavaScript environment or the OS filesystem.

**Implementation:**
```javascript
const { ServoWindow, app } = require('@lotus-gui/core');
const path = require('path');

// 1. Initialize the Encrypted VFS natively (Must happen before warmup)
app.initVfs(); 

// 2. Wake up the engine
app.warmup(); 

const win = new ServoWindow({
    id: 'secure-window',
    root: path.join(__dirname, 'ui'), // Maps to the encrypted VFS!
    index: 'index.html',
    width: 1024,
    height: 768
});
```

### Transparency & "White Flash" Elimination

By default, the window remains transparent until the content finishes loading and tells the engine to show the window.

```javascript
const win = new ServoWindow({
    transparent: true,
    visible: false
});
win.once('frame-ready', () => win.show());
```

The window background is fully transparent until your CSS paints it. Combined with `visible: false`, this entirely eliminates the white flash that often plagues web-based GUI apps.

```css
body { background: transparent; }
.app { background: rgba(0, 0, 0, 0.9); }
```

### Frameless Windows & Drag Regions

Implement your own design language by removing OS decorations. Lotus automatically detects and handles drag regions defined in your CSS or HTML.

```html
<!-- Option 1: CSS property (same as macOS/Electron convention) -->
<div style="-webkit-app-region: drag; cursor: grab; height: 36px;">
    Drag to move window
</div>

<!-- Option 2: Data attribute -->
<div data-lotus-drag="true" style="cursor: grab; height: 36px;">
    Drag to move window
</div>
```

**How it works:**
1. Injected JS scans the DOM for matching elements using `MutationObserver` + `ResizeObserver`.
2. Their bounding rects are sent to Rust via the IPC batch channel (`lotus:set-drag-regions`).
3. On `MouseDown` inside a registered region, Lotus calls `drag_window()` on the OS.
4. On `MouseDown` on the 8px resize border, Lotus calls `drag_resize_window()` on the OS.
5. Servo handles all other mouse events and cursor changes normally.

#### Resize Borders

Frameless windows automatically get 8px invisible hit-zones on every edge and corner. No configuration needed. Move the mouse to any edge to see the resize cursor and click-drag to resize.

#### Cursor Behavior

| Zone | Who controls the cursor |
|------|--------------------------|
| 8px resize border | Rust (shows directional resize cursors) |
| Drag region | Servo/CSS (`cursor: grab`, etc.) |
| Content area | Servo/CSS (`cursor: pointer`, `cursor: text`, etc.) |

CSS cursors work normally inside the window -- `cursor: grab`, `cursor: pointer`, `cursor: text`, `cursor: wait` all function exactly as expected on hover.

### Window State Persistence

Windows with an `id` automatically save their position, size, and maximized state to `~/.config/<app-name>/window-state.json`.

*   **No ID?** A random UUID is generated for the session, and the window starts with the default or specified size.
*   **With ID?** Previous state is checked on launch. If found, the window is restored to its last known position immediately.

```javascript
const win = new ServoWindow({
    id: 'main-window',    // Required for state saving
    restoreState: true     // Default
});
```

### Multi-Window

Each window costs ~80MB (shared renderer). No new browser instances. We don't spawn a whole new universe (or download more RAM) just for a popup.

```javascript
const win1 = new ServoWindow({ id: 'editor', title: 'Editor' });
const win2 = new ServoWindow({ id: 'preview', title: 'Preview' });
```

## Architecture

| Component | Responsibility |
|-----------|----------------|
| **Rust Layer** | Servo engine management, native window creation (`winit`/`glutin`), IPC server (`tokio`+`axum`), protocol handling, and binary event dispatch via MsgPack. |
| **JS Layer** | High-level API (`ServoWindow`, `ipcMain`, `app`), binding management, auto-fixing Linux TLS allocation issues, and profiling support. |

---

## Building from Source

### Prerequisites
- **Rust** stable toolchain.
- **Node.js** v22+.
- System libraries: OpenGL, OpenSSL, fontconfig, dbus-1, and pkg-config.

### Build Commands
```bash
# From the monorepo root
cd packages/lotus-core

# Debug build
npm run build:debug

# Release build (optimized)
npm run build
```

> **Note:** The first build compiles the entire Servo rendering engine and may take a significant amount of time. Subsequent builds are incremental and much faster.

---

## Runtime Dependencies

| Package | Purpose |
|---------|---------|
| `msgpackr` | MsgPack serialization for high-performance IPC. |

These are installed automatically when you `npm install @lotus-gui/core`.

## Package Structure

```
@lotus-gui/core
├── src/lib.rs           # Rust N-API bindings, Servo event loop, IPC
├── src/window_state.rs  # Window position/size persistence
├── src/platform.rs      # OS-specific integrations (themes, cursors)
├── lotus.js             # High-level JS API (ServoWindow, IpcMain, App)
├── index.js             # Native .node binary loader
└── resources/           # IPC bridge injection scripts
```

---

## License

MIT


**AI DISCLAIMER**

I have used ai in the project for templating, troubleshooting, diving through source code to find the reference i needed to read and learn how worked, and documentation originally while i was rapidly itteratting and have since rewrote the documentation myself now the api is more stable. I have tried my best and spent countless hours and days to ensure that the code is correct and that the documentation is accurate, its not a huge code base so i have touched and worked with every line of code in this repo. I will tell you that this is not some BS "vibe coded" system or project. I have worked in IT, Programming and Engineering for going on 11 years. I spent the last 7 years as a Linux enterprise dev/ Linux systems engineer. I have spent a lot of time designing, testing working and griding my hard hours on this project and ensuring this is not some garbage that a unexpirenced person whipped together without actually knowing and understand the way computers work and how to practice proper programing hygene, testing and safe software lifecyle practices. I know what is where, why what works what way, i decided how each and every piece works, written a large amount of it, i have tested different setups and dependancies like the ipc system. I spent a lot of time researching takio, readind the docs figuring out exactly how to use it with my use case and axum so i can web socket the world of my ipc. I dumped so many hours into learning winit 0.30 (which btw, it is a pain to use by its features and layout are amazing, i do recomend the time to learn the new conventions of 0.30), i have poured over so much of the servo repo trying to get the best pieces integrated as nicely as possible and spent so much time fighting with the windows rendering pipeline. i do understand that there is a lot of worries around it and people vibe coding stuff but this is not "vibe coded", this is hundreads of my hours, nights and nights of deep coffee pots and genuine passion for this project. It was involved in this project but it is not running, planning, testing, integrating, designing or any of that, it was just a tool used to speed up piece here and there. There are way too many moving parts here and i have spent way too much time on this to have it reduced down to that.