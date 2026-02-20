# 🪷 Lotus (lotus-gui)

**🏆 "ELECTRON IS AN 80s FORD BRONCO."**
*"Huge. Heavy. Built to survive off-roading, river crossings, and the open internet. Every window spins up a full browser like it's about to get lost in the wilderness."*

**🏎️ "LOTUS IS... WELL, A LOTUS ELISE."**
*"If a part doesn't make it start faster, use less memory, or render pixels, it's gone. No extra suspension. No spare tires. No browser pretending to be an operating system."*

**🥊 THE ARCHITECTURE (Or: Why It's Fast)**
*"Most desktop apps are just opening a preferences panel. I didn't think that required a second operating system."*

• **Electron Strategy:** Puts the browser in charge and lets Node ride shotgun.
  *"It builds a monster truck because it assumes you're off-roading."*
• **Lotus Strategy:** The opposite.
  *"Node owns the OS. Servo paints the pixels. No magic. No fake sandboxes. No hidden Chromium instances listening to your microphone."*

**🚨 STATUS: ALPHA (BUT IT WORKS)**
We have working **Windows** and **Linux** builds available on npm (`@lotus-gui/core@0.2.0`).
Mac support is missing (because their ecosystem needs an adult, please be its adult!). BSD and SUSE support is planned (because I know the pain points over there, see Roadmap). Tested support for building fully packaged .rpm installers for Linux but should support .deb and basic windows installers.

**🔧 THE ANALOGY THAT EXPLAINS EVERYTHING:**
• **Node.js** is the track.
• **Servo** is the car.
• **IPC** is the steering wheel.
  *"On a track, you don't worry about potholes. You worry about lap times."*

**TL;DR:**
Electron assumes you're lost. Lotus assumes you know where you're going. And that's why it's fast.

---

**💡 THE POINT:**
*"Node.js already does OS integration. We just needed a renderer. That's it. That's the whole project."*

## 🚀 Features (The Good Stuff)

*   **Speed that actually matters:**
    *   Cold start to interactive window in **<300ms**. You can't even blink that fast.
    *   A single window stack (Rust + Node + Servo) runs on **~300MB RAM**.
    *   Adding a second window costs **~80MB**. We share the renderer. We don't spawn a new universe for every pop-up.

*   **Hybrid Runtime:**
    *   **Core:** Rust-based Servo engine. It renders HTML/CSS. That's it.
    *   **Controller:** Node.js main thread. It does literally everything else.

*   **Hybrid Mode (File Serving):**
    *   **Custom Protocol:** `lotus-resource://` serves files from disk.
    *   **Why?** Because spinning up an HTTP server just to show a JPEG is stupid.
    *   **Security:** Directory jailing. You can't `../../` your way to `/etc/passwd`. Nice try.

*   **Advanced IPC (The Steering Wheel):**
    *   **Localhost IPC Server:** We use `tiny_http` on `127.0.0.1:0`. It works. It's fast.
    *   **Auto-Adapting:** JSON? Binary? Blobs? We don't care. We handle it and almost never have to serialize it.
    *   **MsgPack Batching:** We pack small messages together like sardines. Efficient, tasty sardines.
    *   **Zero-Copy:** We try not to copy data. Copying data is for people who like waiting.

*   **Window State Persistence:**
    *   It remembers where you put the window (if you give it an ID). Groundbreaking technology, I know.
    *   Handles maximized state, size, position. You're welcome.
    
*   **Script Injection:**
    *   Execute arbitrary JS in the renderer from the main process. God mode unlocked.

*   **Native Look & Feel:**
    *   **true OS transparency**, and actual working cursors. We don't just emulate a window; we *are* a window.
    *   **No White Flash:** We paint transparently. Your users won't be blinded by a white box while your 5MB of JS loads.

*   **Frameless Windows:**
    *   Kill the title bar. Remove the frame. Build whatever crap you want.
    *   **Custom Drag Regions:** Mark any element with `-webkit-app-region: drag` or `data-lotus-drag`. Lotus bridges it to the OS — no JS required.
    *   **Custom Resize Borders:** 8px invisible resize handles on every edge and corner. They just work.
    *   **Cursor-Aware:** Resize cursors show up at the borders. Servo drives all other cursors (grab, pointer, text, etc.) — no interference.

*   **Multi-Window Support:**
    *   Spawn multiple independent windows from a single Node process.
    *   Shared renderer = ~80MB per extra window. Electron could never.

---

## 📦 Monorepo Structure

Lotus is organized as a monorepo with two packages:

```
lotus/
├── packages/
│   ├── lotus-core/          # @lotus-gui/core — Runtime engine (Servo + Node bindings)
│   │   ├── src/             # Rust source (N-API bindings, window management)
│   │   ├── lotus.js         # High-level JS API (ServoWindow, IpcMain, App)
│   │   ├── index.js         # Native binding loader
│   │   ├── resources/       # IPC bridge scripts, debugger
│   │   └── test_app/        # Example application
│   │
│   └── lotus-dev/           # @lotus-gui/dev — CLI toolkit for development & packaging
│       ├── bin/lotus.js      # CLI entry point (lotus dev, build, clean)
│       └── lib/templates/    # Installer templates (RPM spec, etc.)
│
├── package.json             # Monorepo root (npm workspaces)
└── README.md                # You are here
```

| Package | npm Name | What It Does |
|---------|----------|--------------|
| [lotus-core](./packages/lotus-core/) | `@lotus-gui/core` | The runtime — Servo engine, window management, IPC. This is what your app `require()`s. |
| [lotus-dev](./packages/lotus-dev/) | `@lotus-gui/dev` | CLI toolkit — dev server with hot-reload, build system, DEB/RPM installer packaging. |

## 🛠️ Prerequisites

If you want to run this, you need to be on an OS that respects you. 

### Linux (Debian/Ubuntu/Fedora)
This is where development happens. It works here. Fully working `.node` file for Linux is in the artifacts tab.

*   **Node.js:** v22+. Don't come at me with v14, we legit require it, we are using N-API 4.
*   **System Libraries:** You need these or things will scream at you.

    **Ubuntu/Debian:**
    ```bash
    sudo apt-get update
    sudo apt-get install libgl1-mesa-dev libssl-dev python3 libfontconfig1-dev

    # Required for building .deb installers with `lotus build`
    sudo apt-get install dpkg-dev fakeroot
    ```

    **Fedora:**
    ```bash
    sudo dnf install mesa-libGL-devel openssl-devel python3 fontconfig-devel

    # Required for building .rpm installers with `lotus build`
    sudo dnf install rpm-build
    ```

> **Note:** We auto-fix the `GLIBC_TUNABLES` static TLS issue. If you see `ERR_DLOPEN_FAILED` and the app restarts itself, that's just Lotus fixing your environment for you. Don't panic.

### Windows
*   **Status:** Alpha ("It Works!")
*   **NPM:** ✅ Verified - `@lotus-gui/core` includes the pre-built Windows binary.
*   **Build Requirements (only if building from source):** Visual Studio Build Tools + `choco install llvm nasm python311`.


### macOS
*   **Status:** HELP WANTED 🆘
*   I removed CI support because I honestly just don't know enough about the Mac app lifecycle to do it right. If you are a Mac developer and want to fix this, PRs are welcome. I just don't have a system to test on. "Here be dragons still." 🐉 (Translation: Please save me from Xcode.)

---
 
 ## 🛠 Platform Support Matrix
 
 | Platform | Arch | Native Binary (`.node`) | Installer Target | Status |
 | :--- | :--- | :--- | :--- | :--- |
 | **Linux (Debian/Ubuntu)** | x64 | ✅ Verified | `.deb` (Stable) | Ready |
 | **Linux (Fedora/RHEL)** | x64 | ✅ Verified | `.rpm` (Stable) | Ready |
 | **Linux (openSUSE)** | x64 | 🛠 Testing | `.rpm` (Planned v0.3) | Beta |
 | **Windows** | x64 | ✅ Verified | `.msi` (Planned v0.3) | Alpha |
 | **FreeBSD** | x64 | ⏳ Planned | TBD (Planned v0.3) | Roadmap |
 | **macOS** | arm64 | 🆘 Help Wanted | TBD | On Hold |

> **Note:**
> *   **Installer Target:** The packaged distribution format (what users download/install)
> *   **Native Binary:** The `.node` file that powers the runtime (what developers `require()`)
 
 ### **Verification Progress Tracking**
 For a detailed breakdown of build success, environmental linking, and functional testing per platform, see the master tracking issue: **🚀 Multi-Platform Support & Build Targets**.
 
 * **Build Success:** CI/CD produces artifacts without warnings.
 * **Functionality:** Core features operational on clean installs.
 * **Environment:** Proper linking of native dependencies.
 
 ---

## 🚀 Quick Start (Usage from NPM)

> **The Easy Way:** working Windows and Linux builds are on npm. You don't need to build from source.

### Option 1: Quick Start (Recommended)

The fastest way to get started is with the CLI:

```bash
npx lotus init my-app
cd my-app
npm install
npx lotus dev
```

### Option 2: Manual Setup

If you prefer to set things up yourself:

```bash
mkdir my-lotus-app && cd my-lotus-app
npm init -y
npm install @lotus-gui/core @lotus-gui/dev
```

> Then see the example code below or check `node_modules/@lotus-gui/core/test_app` for a full reference.


### Step 1: Create `lotus.config.json`

This file controls your app's metadata and build settings:

```json
{
    "name": "MyApp",
    "version": "1.0.0",
    "license": "MIT",
    "description": "My desktop app, minus the bloat",
    "main": "main.js",
    "executableName": "my-app",
    "icon": "./assets/icon.png",
    "build": {
        "linux": {
            "wmClass": "my-app",
            "categories": ["Utility"]
        }
    }
}
```

### Step 2: Create `main.js`

```javascript
const { ServoWindow, ipcMain, app } = require('@lotus-gui/core');
const path = require('path');

app.warmup(); // Wake up the engine

const win = new ServoWindow({
    id: 'main-window',
    root: path.join(__dirname, 'ui'),
    index: 'index.html',
    width: 1024,
    height: 768,
    title: "My Lotus App",
    transparent: true,
    visible: false
});

// Show only after first frame — no white flash, ever
win.once('frame-ready', () => win.show());

// IPC: talk to the webpage
ipcMain.on('hello', (data) => {
    console.log('Renderer says:', data);
    ipcMain.send('reply', { message: 'Hello from Node!' });
});
```

### Step 3: Create your UI

```bash
mkdir ui
```

`ui/index.html`:
```html
<!DOCTYPE html>
<html>
<head><title>My App</title></head>
<body style="background: transparent;">
    <div style="background: rgba(0,0,0,0.9); color: white; padding: 2rem; border-radius: 12px;">
        <h1>Hello from Lotus! 🪷</h1>
        <button onclick="window.lotus.send('hello', { from: 'renderer' })">
            Talk to Node.js
        </button>
    </div>
    <script>
        window.lotus.on('reply', (data) => {
            console.log('Node says:', data.message);
        });
    </script>
</body>
</html>
```

### Step 4: Run it

```bash
npx lotus dev main.js
```

---

## ⚙️ `lotus.config.json` Reference

The config file lives in your project root and controls both runtime behavior and build output.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | `string` | Yes | Application display name |
| `version` | `string` | Yes | Semver version (e.g., `"1.0.0"`) |
| `license` | `string` | No | SPDX license identifier. Defaults to `"Proprietary"` |
| `description` | `string` | No | Short description (used in package managers) |
| `main` | `string` | No | Entry point file. Falls back to `package.json` `main`, then `index.js` |
| `executableName` | `string` | No | Binary name (e.g., `my-app` → `/usr/bin/my-app`). Defaults to lowercase `name` |
| `icon` | `string` | No | Path to app icon (relative to project root) |
| `author` | `string` | No | Maintainer name for package metadata |
| `homepage` | `string` | No | Project URL |
| `build.linux.wmClass` | `string` | No | Window manager class (taskbar grouping) |
| `build.linux.section` | `string` | No | Package section (default: `"utils"`) |
| `build.linux.categories` | `string[]` | No | Desktop entry categories |

## 🔧 CLI Commands (`@lotus-gui/dev`)

The `@lotus-gui/dev` package provides the `lotus` CLI:

```bash
# Start dev server with hot-reload (watches for changes, auto-restarts)
lotus dev [entry]

# Build a distributable installer (DEB or RPM)
lotus build --platform <linux|win32> --target <deb|rpm>

# Clean build artifacts (removes dist/)
lotus clean
```

See the full [@lotus-gui/dev documentation](./packages/lotus-dev/README.md) for details on build output, flags, and project setup.

## 🎯 Usage (Code Snippets)

### Hybrid Mode: Serving Files
Stop using Express to serve static files. It's embarrassing.

```javascript
const { ServoWindow, app } = require('@lotus-gui/core');

app.warmup(); // Wake up the engine

const win = new ServoWindow({
    root: '/absolute/path/to/ui',  // Jail the renderer here
    index: 'index.html',            // Start here
    width: 1024,
    height: 768,
    title: "My Hybrid Lotus App"
});

// Now serving at lotus-resource://localhost/index.html
```

### IPC: Talking to the Machine
The renderer is a webpage. The main process is Node. They talk.

**Renderer (The Webpage):**
```javascript
// Send stuff.
window.lotus.send('channel', { magic: true });

// Send heavy stuff.
const blob = new Blob(['pure binary fury']);
window.lotus.send('binary-channel', blob);
```

**Main Process (Node):**
```javascript
const { ipcMain } = require('@lotus-gui/core');

ipcMain.on('channel', (data) => {
    console.log('Renderer said:', data);
    ipcMain.send('reply', { status: 'acknowledged' });
});
```

### Native Transparency: "Ghost Mode"
Want a window that keeps the OS vibe? We bridge OS transparency directly to your CSS.

```javascript
const win = new ServoWindow({
    transparent: true, // The magic switch
    title: "Ghost Window"
});
```

**How it works:**
1.  We set the Servo shell background to `0x00000000` (fully transparent).
2.  We tell the OS to make the window transparent.
3.  **Result:** The window is invisible. The *only* thing visible is what **you** paint.

**In your CSS:**
```css
/* This makes the whole app see-through to the desktop */
body {
    background: transparent; 
}

/* This adds a semi-transparent glass effect */
.container {
    background: rgba(0, 0, 0, 0.8); 
    color: white;
}
```

**The "White Flash" Killer:**
Because the default backbone is transparent, there is **zero white flash** on startup. If your app takes 10ms to load, the user sees their wallpaper for 10ms, not a blinding white rectangle. You're welcome.

### Frameless Windows: "Build Your Own Window"
Tired of the OS telling you what your title bar looks like? Remove it.

```javascript
const win = new ServoWindow({
    frameless: true,         // Kill the native frame
    transparent: true,       // Optional: go fully borderless
    title: "My Borderless App"
});
```

Out of the box you get:
- **8px resize borders** on every edge/corner — just move the mouse to the edge.
- **Drag regions** driven by CSS — no JS wiring required.

**In your HTML:**
```html
<!-- These two approaches both work -->
<div style="-webkit-app-region: drag; cursor: grab;">Drag me to move the window</div>
<div data-lotus-drag="true">Also works</div>
```

Lotus auto-detects elements with `-webkit-app-region: drag` or `data-lotus-drag` via injected JS and sends their coordinates to Rust. Mouse down on one of those elements → `drag_window()`. Mouse down on the border → `drag_resize_window()`. Everything else → Servo handles it normally.

To exclude an element inside a drag region (like a close button), use `-webkit-app-region: no-drag` or `data-lotus-drag="false"`.

### Window Controls & Lifecycle
Full control over the OS window manager directly from JavaScript. You don't need to write native code to build a custom title bar.

```javascript
// Window manipulation
win.minimize();
win.unminimize();
win.maximize();
win.unmaximize();
win.focus();

// Listen to OS-level events
win.on('moved', ({ x, y }) => console.log('Window moved to', x, y));
win.on('resize', ({ width, height }) => console.log('Resized to', width, height));
win.on('focus', () => console.log('Window gained focus'));
win.on('blur', () => console.log('Window lost focus'));
```

### Multi-Window Support
Creating specific windows? Easy. They share the same renderer instance, so it costs ~80MB per extra window instead of ~300MB.

```javascript
const win1 = new ServoWindow({ title: "Window 1" });
const win2 = new ServoWindow({ title: "Window 2" });
const win3 = new ServoWindow({ title: "Window 3" });
// All three windows share the same renderer process.
// Efficient.
```

### Window State Persistence: "Total Recall"
By default, windows have the memory span of a goldfish. They forget where they were. If you want them to remember, give them a name.

```javascript
const win = new ServoWindow({
    id: "main-window", // REQUIRED for state saving
    title: "I Remember Everything",
    restoreState: true // Default is true, obviously
});
```

**The Logic:**
*   **No ID?** We generate a random UUID. New session, new window, default size.
*   **With ID?** We check `~/.config/app-name/window-state.json`. If we've seen "main-window" before, we put it back exactly where you left it.
*   It snaps back to the last known position faster than you can say "Electron is bloat."

### Building Distributable Packages
Once your app is ready, build it into a real installer:

```bash
# Build an RPM (Fedora/RHEL)
npx lotus build --platform linux --target rpm

# Build a DEB (Ubuntu/Debian)  
npx lotus build --platform linux --target deb

# Install it
sudo dnf install ./dist/installers/my-app-1.0.0-1.x86_64.rpm
# or
sudo dpkg -i ./dist/installers/my-app_1.0.0_amd64.deb
```

Your app is now a real installed application with a binary in `/usr/bin/` and everything. Just like a grown-up program.

---

## 🏗️ Building from Source (The Waiting Game)

> **Pro Tip:** You don't actually have to build this yourself. Check the **Actions** tab on GitHub. Every commit produces working artifacts for Linux and Windows. Download, unzip, use the time saved to beat that level you've been procrastinating on. (expect npm install support without having to build yourself soon — you can just grab the `.node` files from the artifacts tab)

```bash
git clone https://github.com/1jamie/project-lotus.git
cd project-lotus
npm install
```

**Build the Native Addon:**

```bash
cd packages/lotus-core

# Debug Build (Faster compilation, still slow)
npm run build:debug

# Release Build (Optimized, takes eons)
npm run build
```

**Additional Requirements for Building:**
*   **Rust:** Stable toolchain.
    ```bash
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
    ```
*   **Windows:** Visual Studio Build Tools + `choco install llvm nasm python311`

> **Warning:** The first build takes forever. You are compiling a browser engine and a Node runtime binding. Go make a coffee. Read a book. Learn a new language. (though we all know you are scrolling TikTok or Reddit, we all know you aren't being productive while the compile runs, none of us ever are) It gets faster after the first time. I promise.

## 📂 Project Structure (For the curious)

*   `packages/lotus-core/src/lib.rs` - The Brain. Main Rust entry point. Handles N-API, Event Loop, IPC.
*   `packages/lotus-core/src/window_state.rs` - The Memory. Remembers where you put your windows.
*   `packages/lotus-core/src/platform.rs` - The Politeness. Proper OS integrations.
*   `packages/lotus-core/lotus.js` - The Body. High-level Node.js wrapper (`ServoWindow`, `IpcMain`, `App`).
*   `packages/lotus-core/index.js` - The Glue. Native binding loader.
*   `packages/lotus-core/test_app/` - The Real Demo. Full-featured test app.
*   `packages/lotus-dev/bin/lotus.js` - The Toolbox. CLI for dev, build, and clean commands.
*   `packages/lotus-dev/lib/templates/` - The Factory. Installer templates (RPM spec, etc.).

For detailed API documentation, see:
*   [@lotus-gui/core README](./packages/lotus-core/README.md) — Full `ServoWindow` API, IPC reference, architecture
*   [@lotus-gui/dev README](./packages/lotus-dev/README.md) — CLI commands, config reference, build pipeline

## 🤝 Contributing

PRs are welcome. If you break the `winit` or `glutin` version requirements, I will close your PR with extreme prejudice. We need specific embedding traits and I'm already sitting on the edge with winit 0.30.2, don't push me off the edge it has already mentally put me on!

1.  Fork it.
2.  Branch it (`git checkout -b feature/cool-stuff`).
3.  Commit it (`git commit -m 'Added cool stuff'`).
4.  Push it.
5.  PR it.

---
## 🗺️ Roadmap

### v0.2.0: The Shell & Frame (RELEASED)
* ✅ **Frameless Mode:** Toggle window decorations off.
* ✅ **CSS Dragging:** Bridge for custom CSS drag areas (`-webkit-app-region: drag`, `data-lotus-drag`).
* ✅ **Resize Borders:** Custom 8px resize hit-zones on all edges/corners.
* ✅ **Dev CLI:** `lotus init` command added to create a new Lotus project.

> **Note:** Surprise! I changed my mind and gave you frameless support lol. I was just gonna say deal with it and give you native menu support but then i realized the headache.... so here we are! surprise! Now you can do wtf you want with window decorations! 

### v0.3.0: The Support Expansion
*   **Windows Support:** Full MSI/EXE distribution (moving beyond just the `.node` binary).
*   **BSD Support:** Bringing the renderer to the BSD community.
*   **SUSE Support:** Expanding `@lotus-gui/dev` to handle OpenSUSE RPM quirks.
*   **Mac Support?** (If someone donates a Mac or a contributor steps up).
*   **Verify/support build for all supported platforms.**: Go through the build process for all supported platforms and verify that it works and fix where it does not.

### v0.4.0: The Future (Community Input Welcome)
*   **Build optimization:** See what we can do about the electron builder as it does add deps we may not need and may be able to make it install less deps during the install processes.
*   **Debugger:** Add a debugger to the dev CLI to give access to a dev console for helping with development. Will probably have a seperate package called lotus-core-debug that the dev CLI will use to start the app with a build of lotus with the debug symbols and a developers console in its own window to help with debugging and gui development.
*   **Open to suggestions:** I'm open to suggestions for the future. If you have an idea, let me know. Right now v0.4.0 is just a rough tenative plan for what I might do in the future.

---
**License:** MIT. Do whatever you want, just don't blame me if your computer achieves sentience and takes flight.



**P.S.**

The entire framework core is ~2,500 lines of code.

If that feels suspiciously light, it's because it is. I didn't try to build an OS inside your OS; I just gave Node a window and cut the fat until there was nothing left but speed.

Electron carries the weight of the world. Lotus just carries the pixels.