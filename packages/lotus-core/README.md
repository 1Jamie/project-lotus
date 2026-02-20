# @lotus-gui/core

The runtime engine for Lotus applications. Provides the Servo rendering engine, window management, and IPC system as a Node.js native addon.

## Installation

```bash
npm install @lotus-gui/core
```

> **Note:** Pre-built binaries are available for **Linux (x86_64)** and **Windows (x64)**. You likely do not need to build from source. Seriously, save your CPU cycles. I bled so you don't have to.

## API Reference

### Exports

```javascript
const { ServoWindow, ipcMain, app } = require('@lotus-gui/core');
```

### `app`

The application lifecycle controller.

| Method | Description |
|--------|-------------|
| `app.warmup()` | Pre-initialize the Servo engine. Call before creating windows for faster startup. Like revving the engine at a red light. |
| `app.quit()` | Shut down the application. Terminate with extreme prejudice. |

```javascript
app.warmup(); // Pre-warm the engine
```

### `ServoWindow`

Creates and manages a native window powered by the Servo rendering engine. Extends `EventEmitter`.

#### Constructor Options

```javascript
const win = new ServoWindow(options);
```

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `id` | `string` | Random UUID | Window identifier. **Required for state persistence** (unless you want goldfish memory). |
| `root` | `string` | `undefined` | Absolute path to UI directory. Enables Hybrid Mode (`lotus-resource://`). Keeps your files properly jailed. |
| `index` | `string` | `'index.html'` | Entry HTML file (relative to `root`). |
| `initialUrl` | `string` | — | URL to load (alternative to `root` + `index`). |
| `width` | `number` | `1024` | Window width in pixels. |
| `height` | `number` | `768` | Window height in pixels. |
| `title` | `string` | `'Lotus'` | Window title. |
| `transparent` | `boolean` | `false` | Enable OS-level window transparency. Make your app look like a ghost. |
| `visible` | `boolean` | `true` | Show window immediately. Set `false` to show after `frame-ready` to avoid the dreaded white flash of death. |
| `frameless` | `boolean` | `false` | Remove the native window frame. Enables custom drag regions. The OS doesn't own your title bar anymore. |
| `resizable` | `boolean` | `true` | Allow window resizing. |
| `maximized` | `boolean` | `false` | Start maximized. Assert dominance immediately. |
| `fullscreen` | `boolean` | `false` | Start in fullscreen. |
| `alwaysOnTop` | `boolean` | `false` | Keep window above others. Because you're the main character. |
| `restoreState` | `boolean` | `true` | Restore previous position/size (requires `id`). |

#### Methods

| Method | Description |
|--------|-----------|
| `win.loadUrl(url)` | Navigate to a URL. |
| `win.executeScript(js)` | Execute JavaScript in the renderer context. |
| `win.sendToRenderer(channel, data)` | Send data to the renderer on a named channel. |
| `win.setTitle(title)` | Update the window title. |
| `win.setDecorations(bool)` | Toggle native window decorations at runtime. |
| `win.setSize(width, height)` | Resize the window. |
| `win.setPosition(x, y)` | Move the window. |
| `win.minimize()` | Minimize the window to the taskbar/dock. |
| `win.unminimize()` | Restore the window from a minimized state. |
| `win.maximize()` | Maximize the window to fill the screen. |
| `win.unmaximize()` | Restore the window from a maximized state. |
| `win.focus()` | Bring the window to the front and request focus. |
| `win.show()` | Show the window. |
| `win.hide()` | Hide the window. |
| `win.close()` | Close and destroy the window. |

#### Events

| Event | Callback | Description |
|-------|----------|-------------|
| `'ready'` | `()` | Window has been created and is operational. Ready for orders. |
| `'frame-ready'` | `()` | First frame has been rendered. Best time to call `show()` unless you actually enjoy blinding your users. |
| `'load-status'` | `(status: string)` | Page load status changed (`'loading'`, `'complete'`, etc.). |
| `'moved'` | `({ x: number, y: number })` | Window position changed on the screen. |
| `'resize'` | `({ width: number, height: number })` | Window size changed. |
| `'focus'` | `()` | Window gained focus. |
| `'blur'` | `()` | Window lost focus. |
| `'closed'` | `()` | Window was closed. |

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

### `ipcMain`

Handles communication between the main process (Node.js) and renderer (webpage). Extends `EventEmitter`.

#### Main Process

```javascript
const { ipcMain } = require('@lotus-gui/core');

// Listen for messages from the renderer
ipcMain.on('channel-name', (data) => {
    console.log('Got:', data);
});

// Send to all windows
ipcMain.send('response-channel', { status: 'ok' });
```

#### Renderer (Webpage)

The `window.lotus` bridge is automatically injected into every page.

```javascript
// Send to main process
window.lotus.send('channel-name', { key: 'value' });

// Binary data
window.lotus.send('binary-channel', new Blob(['binary data']));

// Listen for responses
window.lotus.on('response-channel', (data) => {
    console.log('Got:', data);
});
```

## Concepts

### Hybrid Mode (`lotus-resource://`)

Instead of spinning up a whole HTTP server just to serve your UI files (which, let's be honest, is embarrassing), Lotus serves them directly from disk via the `lotus-resource://` custom protocol.

```javascript
const win = new ServoWindow({
    root: path.join(__dirname, 'ui'),  // Files served from here
    index: 'index.html'               // Entry point
});
// Internally loads: lotus-resource://localhost/index.html
```

**Benefits:**
- No HTTP server overhead. Because you're serving static files, not running a datacenter.
- No port collisions. Because `EADDRINUSE` on port 8080 is cursed.
- Directory jailing. You can't escape the root path. Try `../../`, I dare you.

### Transparency (No White Flash)

```javascript
const win = new ServoWindow({
    transparent: true,
    visible: false
});

win.once('frame-ready', () => win.show());
```

The window background is fully transparent until your CSS paints it. Combined with `visible: false`, this entirely eliminates the white flash that plagues web gui based apps. You're welcome.

```css
body { background: transparent; }
.app { background: rgba(0, 0, 0, 0.9); }
```

### Window State Persistence

Windows with an `id` automatically save their position, size, and maximized state to `~/.config/<app-name>/window-state.json`. By default, windows are amnesiac. Give them an `id` so they remember where they parked.

```javascript
const win = new ServoWindow({
    id: 'main-window',    // Required for state saving
    restoreState: true     // Default
});
```

### Frameless Windows

Remove native window decorations and implement your own title bar, controls, and drag behavior entirely in HTML/CSS.

```javascript
const win = new ServoWindow({
    frameless: true,
    transparent: true,   // Pair with transparent for a fully custom look
    visible: false,
});
win.once('frame-ready', () => win.show());
```

#### Drag Regions

Lotus automatically detects elements with `-webkit-app-region: drag` or `data-lotus-drag` and registers them as drag handles. No JavaScript needed on your side.

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

CSS cursors work normally inside the window — `cursor: grab`, `cursor: pointer`, `cursor: text`, `cursor: wait` all function exactly as expected on hover.

### Multi-Window

Each window costs ~80MB (shared renderer). No new browser instances. We don't spawn a whole new universe (or download more RAM) just for a popup.

```javascript
const win1 = new ServoWindow({ id: 'editor', title: 'Editor' });
const win2 = new ServoWindow({ id: 'preview', title: 'Preview' });
```

## Architecture

```
@lotus-gui/core
├── src/lib.rs           # Rust N-API bindings, Servo event loop, IPC
├── src/window_state.rs  # Window position/size persistence
├── src/platform.rs      # OS-specific integrations (themes, cursors)
├── lotus.js             # High-level JS API (ServoWindow, IpcMain, App)
├── index.js             # Native .node binary loader
└── resources/           # IPC bridge injection scripts
```

The Rust layer (`src/lib.rs`) handles:
- Servo lifecycle and rendering
- Native window creation via `winit`/`glutin`
- IPC server (`tiny_http` on `127.0.0.1:0`)
- `lotus-resource://` protocol handler
- Event dispatch to Node.js via MsgPack

The JavaScript layer (`lotus.js`) provides:
- `ServoWindow` class wrapping native window handles
- `IpcMain` event emitter for message routing
- Auto-fix for Linux TLS allocation issues
- Profiling support (`--profile` flag)

## Building from Source

### Prerequisites

- **Rust** stable toolchain
- **Node.js** v22+
- System libraries (OpenGL, OpenSSL, fontconfig)

### Build

```bash
# From the monorepo root
cd packages/lotus-core

# Debug build
npm run build:debug

# Release build (optimized)
npm run build
```

> **Warning:** First build compiles the Servo rendering engine. This takes a while. Go grab a coffee, or question your life choices. Subsequent builds are much faster.

## Runtime Dependencies

| Package | Purpose |
|---------|---------|
| `msgpackr` | MsgPack serialization for IPC (binary message packing) |

These are installed automatically when you `npm install @lotus-gui/core`.

## License

MIT
