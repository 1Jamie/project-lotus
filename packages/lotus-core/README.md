# @lotus/core

The runtime engine for Lotus applications. Provides the Servo rendering engine, window management, and IPC system as a Node.js native addon.

## Installation

```bash
npm install @lotus/core
```

> **Note:** Pre-built binaries are available for Linux (x86_64). For other platforms, you'll need to build from source (see [Building from Source](#building-from-source)).

## API Reference

### Exports

```javascript
const { ServoWindow, ipcMain, app } = require('@lotus/core');
```

### `app`

The application lifecycle controller.

| Method | Description |
|--------|-------------|
| `app.warmup()` | Pre-initialize the Servo engine. Call before creating windows for faster startup. |
| `app.quit()` | Shut down the application. |

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
| `id` | `string` | Random UUID | Window identifier. **Required for state persistence.** |
| `root` | `string` | `undefined` | Absolute path to UI directory. Enables Hybrid Mode (`lotus-resource://`). |
| `index` | `string` | `'index.html'` | Entry HTML file (relative to `root`). |
| `initialUrl` | `string` | — | URL to load (alternative to `root` + `index`). |
| `width` | `number` | `1024` | Window width in pixels. |
| `height` | `number` | `768` | Window height in pixels. |
| `title` | `string` | `'Lotus'` | Window title. |
| `transparent` | `boolean` | `false` | Enable OS-level window transparency. |
| `visible` | `boolean` | `true` | Show window immediately. Set `false` to show after `frame-ready`. |
| `frameless` | `boolean` | `false` | Remove the native window frame. |
| `resizable` | `boolean` | `true` | Allow window resizing. |
| `maximized` | `boolean` | `false` | Start maximized. |
| `fullscreen` | `boolean` | `false` | Start in fullscreen. |
| `alwaysOnTop` | `boolean` | `false` | Keep window above others. |
| `restoreState` | `boolean` | `true` | Restore previous position/size (requires `id`). |

#### Methods

| Method | Description |
|--------|-------------|
| `win.loadUrl(url)` | Navigate to a URL. |
| `win.executeScript(js)` | Execute JavaScript in the renderer context. |
| `win.sendToRenderer(channel, data)` | Send data to the renderer on a named channel. |
| `win.setTitle(title)` | Update the window title. |
| `win.setSize(width, height)` | Resize the window. |
| `win.setPosition(x, y)` | Move the window. |
| `win.show()` | Show the window. |
| `win.hide()` | Hide the window. |
| `win.close()` | Close and destroy the window. |

#### Events

| Event | Callback | Description |
|-------|----------|-------------|
| `'ready'` | `()` | Window has been created and is operational. |
| `'frame-ready'` | `()` | First frame has been rendered. Best time to call `show()`. |
| `'load-status'` | `(status: string)` | Page load status changed (`'loading'`, `'complete'`, etc.). |
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
const { ipcMain } = require('@lotus/core');

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

Instead of spinning up an HTTP server to serve your UI files, Lotus serves them directly from disk via the `lotus-resource://` custom protocol.

```javascript
const win = new ServoWindow({
    root: path.join(__dirname, 'ui'),  // Files served from here
    index: 'index.html'               // Entry point
});
// Internally loads: lotus-resource://localhost/index.html
```

**Benefits:**
- No HTTP server overhead
- No port collisions
- Directory jailing (can't escape the root path)

### Transparency (No White Flash)

```javascript
const win = new ServoWindow({
    transparent: true,
    visible: false
});

win.once('frame-ready', () => win.show());
```

The window background is fully transparent until your CSS paints it. Combined with `visible: false`, this eliminates the white flash that plagues Electron apps.

```css
body { background: transparent; }
.app { background: rgba(0, 0, 0, 0.9); }
```

### Window State Persistence

Windows with an `id` automatically save their position, size, and maximized state to `~/.config/<app-name>/window-state.json`.

```javascript
const win = new ServoWindow({
    id: 'main-window',    // Required for state saving
    restoreState: true     // Default
});
```

### Multi-Window

Each window costs ~80MB (shared renderer). No new browser instances.

```javascript
const win1 = new ServoWindow({ id: 'editor', title: 'Editor' });
const win2 = new ServoWindow({ id: 'preview', title: 'Preview' });
```

## Architecture

```
@lotus/core
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

> **Warning:** First build compiles the Servo rendering engine. This takes a while. Subsequent builds are much faster.

## Runtime Dependencies

| Package | Purpose |
|---------|---------|
| `msgpackr` | MsgPack serialization for IPC (binary message packing) |

These are installed automatically when you `npm install @lotus/core`.

## License

MIT
