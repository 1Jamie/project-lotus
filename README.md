# 🪷 Project Lotus (Servo-Node)

**A lightweight, embedded browser runtime using Rust (Servo) as the rendering engine and Node.js as the controller.**

Lotus provides a modern, high-performance alternative to Electron with a fraction of the resources and heavily optimized startup times. It leverages the speed and safety of the Servo browser engine and the ubiquity of the Node.js ecosystem.

**The Aim:** Prove that a web-based app engine can be fast, memory-efficient, and that Electron isn't the only (or even the best) option for many use cases. Electron treats every window like a separate browser; Lotus treats every window like a first-class citizen of a single, unified engine.

**The Proof:**
*   **Speed:** Cold start to full interactive window in **<300ms**.
*   **Efficiency:** A single window stack (Rust + Node + Servo) runs on **~300MB RAM**.
*   **Scaling:** Adding a second window only costs **~80MB RAM** because we share the renderer instance. No spinning up entirely new browser processes per window!
*   **IPC:** A custom hybrid IPC system that is significantly faster and safer than Electron's default.

## 🚀 Features

*   **Hybrid Runtime:**
    *   **Core:** Rust-based Servo engine running on a dedicated thread.
    *   **Controller:** Node.js main thread for business logic, file I/O, and state management.
*   **Hybrid Mode (File Serving):**
    *   **Custom Protocol:** `lotus-resource://` for serving files directly from disk without a Node.js HTTP server.
    *   **Security:** Directory jailing ensures the renderer can only access files within the specified root directory.
    *   **Performance:** Eliminates Node.js HTTP server overhead and port collision issues.
    *   **Simple API:** Just specify a `root` directory and `index` file when creating a window.
*   **Performance:**
    *   Native N-API (`napi-rs`) integration.
    *   Zero-copy mechanisms where possible (msgpackr and making sure use inside each piece both node and rust are as zero copy as possible).
    *   Hardware-accelerated rendering via `winit` and `glutin`.
*   **Advanced IPC System:**
    *   **Localhost IPC Server:** Built-in `tiny_http` server on `127.0.0.1:0` for robust bi-directional communication.
    *   **Auto-Adapting API:** Automatically handles both JSON objects and binary data (Blob, ArrayBuffer, TypedArrays).
    *   **MsgPack Batching:** Small messages are automatically batched and serialized with MsgPack for efficiency.
    *   **Binary Streaming:** Large binary data is sent directly via POST requests without serialization overhead.
    *   **Authentication:** Token-based authentication ensures only authorized renderer processes can communicate.
    *   **Thread-Safe:** High-speed communication between Node.js and Servo threads.
*   **Window State Persistence:**
    *   Automatically saves and restores window position, size, and maximized state across sessions.
    *   Per-application configuration storage using OS-appropriate directories.
*   **Script Injection:**
    *   Execute arbitrary JavaScript in the renderer context from the main process.
*   **Multi-Window Support:**
    *   Spawn multiple independent Servo windows from a single Node.js process with minimal overhead.
    *   Shared renderer instance means adding a second window only costs ~80MB RAM instead of spinning up an entire new browser process.
*   **Native Look & Feel:**
    *   Customizable window frames, titles, and transparency.
    *   Correct OS cursor handling with full Servo cursor icon support.

## 🛠️ Prerequisites

To build Project Lotus, you need the following dependencies installed on your system:

### Linux (Debian/Ubuntu/Fedora)
*   **Rust:** Stable toolchain is required.
    ```bash
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
    ```
*   **Node.js:** v22+ (tested on v22 and using N-API 4 so dont recomend any lower).
*   **System Libraries:** You must install these before building, or the Rust compilation *will* fail.

    **Ubuntu/Debian:**
    ```bash
    sudo apt-get update
    sudo apt-get install libgl1-mesa-dev libssl-dev python3 libfontconfig1-dev
    ```

    **Fedora:**
    ```bash
    sudo dnf install mesa-libGL-devel openssl-devel python3 fontconfig-devel
    ```

> **Note:** On Linux, Lotus automatically handles the `GLIBC_TUNABLES` environment variable fix for static TLS allocation. If you see `ERR_DLOPEN_FAILED`, the wrapper will automatically restart the process with the correct settings. You don't need to do anything manually!

### Windows / macOS
*   **Status:** **Experimental / Untested.**
*   I haven't done anything crazy that *should* break cross-platform compatibility (standard crates), but... haven't tested it yet. Mileage may vary!
*   (Windows) likely requires Visual Studio Build Tools (C++).

## 📦 Building

Clone the repository and install the Node.js dependencies (CLI tools):

```bash
git clone https://github.com/1jamie/project-lotus.git
cd project-lotus

# Installs @napi-rs/cli and other build tools
npm install
```

**Build the Native Addon:**
This command compiles the Rust code in `src/` and generates the `servo-node.linux-x64-gnu.node` binary.

```bash
# Debug Build (Faster compilation, larger binary)
npm run build:debug

# Release Build (Optimized, smaller binary)
npm run build
```

**Note and Warning:**
This IS going to take FOREVER to do the first build. i know, but you have to clone the whole servo repo and build it from source and then build the whole n-api and stuffs. after the first build it does get a lot faster. I plan to eventually integrate a ci/cd system that node will just be able to pull the pre-compiled platform .node files from github releases. let me get it somewhat stable first. I know, the compile times suck ass, but we have all been there, im working on it <3

### Running the Test App

The best way to see Lotus in action is to run the included test application. This app demonstrates the hybrid runtime, **Hybrid Mode** file serving, IPC communication, and multi-window capabilities.

```bash
# Ensure you have built the project first (npm run build:debug)
npm start

# Optional: Enable performance profiling
npm start -- --profile
```

The `--profile` flag will output detailed timing metrics for app initialization, window creation, and page load events.

## 🧪 Running Smoke Tests

To verify the raw native binding without the Lotus application framework:

```bash
npm test
```

## 🎯 Advanced Usage

### Hybrid Mode: Serving Local Files

Instead of running a Node.js HTTP server, you can serve your UI files directly from disk using the `lotus-resource://` protocol. This is **faster**, **more secure**, and eliminates port conflicts.

```javascript
const { ServoWindow, app } = require('servo-node');

// Pre-warm the backend
app.warmup();

// Create a window that serves files from the 'ui/' directory
const win = new ServoWindow({
    root: '/absolute/path/to/ui',  // Absolute path to your UI directory
    index: 'index.html',            // Entry point file
    width: 1024,
    height: 768,
    title: "My Hybrid Lotus App"
});

// Files are served via lotus-resource://localhost/
// e.g., lotus-resource://localhost/index.html
//       lotus-resource://localhost/styles.css
//       lotus-resource://localhost/scripts/app.js
```

**Security Note:** The renderer is "jailed" to the `root` directory. It cannot access files outside this directory, preventing directory traversal attacks.

### IPC Communication

The IPC bridge (`window.lotus`) is automatically injected into every page. It supports both JSON objects and binary data.

**In the Renderer (Browser):**
```javascript
// Send JSON data to the backend
window.lotus.send('my-channel', { foo: 'bar', count: 42 });

// Send binary data (automatically uses POST endpoint)
const blob = new Blob(['binary data']);
window.lotus.send('binary-channel', blob);

// Listen for messages from backend
window.lotus.on('response-channel', (data) => {
    console.log('Received from backend:', data);
});
```

**In the Main Process (Node.js):**
```javascript
const { ipcMain } = require('servo-node');

// Listen for messages from renderer
ipcMain.on('my-channel', (data) => {
    console.log('Received:', data);
    
    // Send response back to renderer
    ipcMain.send('response-channel', { result: 'success' });
});
```

### Multi-Window Support

Creating additional windows is cheap (~80MB per window) because they share the same Servo renderer instance:

```javascript
const win1 = new ServoWindow({ title: "Window 1" });
const win2 = new ServoWindow({ title: "Window 2" });
const win3 = new ServoWindow({ title: "Window 3" });
// All three windows share the same renderer process!
```

## 📂 Project Structure

*   `src/lib.rs` - Main Rust entry point. Handles the N-API bridge, Event Loop, IPC server, and resource loading.
*   `src/window_state.rs` - Window state persistence manager for saving/restoring window positions and sizes.
*   `src/platform.rs` - Platform-specific window management utilities (always-on-top, attention requests).
*   `lotus.js` - High-level Node.js wrapper API with automatic TLS fix and event handling.
*   `index.js` - Native binding loader (auto-generated by napi-rs).
*   `index.d.ts` - TypeScript type definitions.
*   `example.js` - Basic example demonstrating window creation and URL loading.
*   `test_app/` - Full-featured demo application showcasing Hybrid Mode, IPC, and multi-window support.
*   `cicd_specification.md` - CI/CD pipeline implementation details.

## 🤝 Contributing

Contributions are welcome! Please ensure you match the strict version requirements for `winit` and `glutin` enabling the specific embedding traits used by Servo.

1.  Fork the repository.
2.  Create your feature branch (`git checkout -b feature/amazing-feature`).
3.  Commit your changes (`git commit -m 'Add some amazing feature'`).
4.  Push to the branch (`git push origin feature/amazing-feature`).
5.  Open a Pull Request.

---
**License:** MIT
