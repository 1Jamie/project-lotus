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
*   **Performance:**
    *   Native N-API (`napi-rs`) integration.
    *   Zero-copy mechanisms where possible (msgpackr and making sure use inside each piece both node and rust are as zero copy as possible).
    *   Hardware-accelerated rendering via `winit` and `glutin`.
*   **Better IPC:**
    *   Thread-safe, high-speed communication between Node.js and Servo.
    *   Capable of handling binary data without serialization overhead.
*   **Script Injection:**
    *   Execute arbitrary JavaScript in the renderer context from the main process.
*   **Multi-Window Support:**
    *   Spawn multiple independent Servo windows from a single Node.js process with minimal overhead.
*   **Native Look & Feel:**
    *   Customizable window frames, titles, and transparency.
    *   Correct OS cursor handling.

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

The best way to see Lotus in action is to run the included test application. This app demonstrates the hybrid runtime, IPC communication, and multi-window capabilities.

```bash
# Ensure you have built the project first (npm run build:debug)
npm start
```

## 🧪 Running Smoke Tests

To verify the raw native binding without the Lotus application framework:

```bash
npm test
```

## 📂 Project Structure

*   `src/lib.rs` - Main Rust entry point. Handles the N-API bridge and Event Loop.
*   `src/window_state.rs` - State management for window instances.
*   `lotus.js` - High-level Node.js wrapper API.
*   `index.js` - Native binding loader.
*   `test_app/` - Demo application and integration tests.
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
