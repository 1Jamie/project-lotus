# 🪷 Lotus (Servo-Node)

**🏆 "ELECTRON IS AN 80s FORD BRONCO."**
*"Huge. Heavy. Built to survive off-roading, river crossings, and the open internet. Every window spins up a full browser like it’s about to get lost in the wilderness."*

**🏎️ "LOTUS IS... WELL, A LOTUS ELISE."**
*"If a part doesn’t make it start faster, use less memory, or render pixels, it’s gone. No extra suspension. No spare tires. No browser pretending to be an operating system."*

**🥊 THE ARCHITECTURE (Or: Why It's Fast)**
*"Most desktop apps are just opening a preferences panel. We didn't think that required a second operating system."*

• **Electron Strategy:** Puts the browser in charge and lets Node ride shotgun.
  *"It builds a monster truck because it assumes you’re off-roading."*
• **Lotus Strategy:** The opposite.
  *"Node owns the OS. Servo paints the pixels. No magic. No fake sandboxes. No hidden Chromium instances listening to your microphone."*

**🔧 THE ANALOGY THAT EXPLAINS EVERYTHING:**
• **Node.js** is the track.
• **Servo** is the car.
• **IPC** is the steering wheel.
  *"On a track, you don’t worry about potholes. You worry about lap times."*

**TL;DR:**
Electron assumes you're lost. Lotus assumes you know where you're going. And that’s why it’s fast.

---

**💡 THE POINT:**
*"Node.js already does OS integration. We just needed a renderer. That's it. That's the whole project."*

## 🚀 Features (The Good Stuff)

*   **Speed that actually matters:**
    *   Cold start to interactive window in **<300ms**. You can't even blink that fast.
    *   A single window stack (Rust + Node + Servo) runs on **~300MB RAM**.
    *   Adding a second window costs **~80MB**. We share the renderer. We don’t spawn a new universe for every pop-up.

*   **Hybrid Runtime:**
    *   **Core:** Rust-based Servo engine. It renders HTML/CSS. That's it.
    *   **Controller:** Node.js main thread. It does literally everything else.

*   **Hybrid Mode (File Serving):**
    *   **Custom Protocol:** `lotus-resource://` serves files from disk.
    *   **Why?** Because spinning up an HTTP server just to show a JPEG is stupid.
    *   **Security:** Directory jailing. You can't `../../` your way to `/etc/passwd`. Nice try.

*   **Advanced IPC (The Steering Wheel):**
    *   **Localhost IPC Server:** We use `tiny_http` on `127.0.0.1:0`. It works. It's fast.
    *   **Auto-Adapting:** JSON? Binary? Blobs? We don't care. We handle it.
    *   **MsgPack Batching:** We pack small messages together like sardines. Efficient, tasty sardines.
    *   **Zero-Copy:** We try not to copy data. Copying data is for people who like waiting.

*   **Window State Persistence:**
    *   It remembers where you put the window. Groundbreaking technology, I know.
    *   Handles maximized state, size, position. You're welcome.
    
*   **Script Injection:**
    *   Execute arbitrary JS in the renderer from the main process. God mode unlocked.

*   **Native Look & Feel:**
    *   Customizable frames, transparency, and actual working cursors. We don't just emulate a window; we *are* a window.

*   **Multi-Window Support:**
    *   Spawn multiple independent windows from a single Node process.
    *   Shared renderer = ~80MB per extra window. Electron could never.

## 🛠️ Prerequisites

If you want to run this, you need to be on an OS that respects you. 

### Linux (Debian/Ubuntu/Fedora)
This is where development happens. It works here.

*   **Rust:** Stable toolchain.
    ```bash
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
    ```
*   **Node.js:** v22+. Don't come at me with no v14, we legit require it, we are using n-api 4.
*   **System Libraries:** You need these or Rust will scream at you.

    **Ubuntu/Debian:**
    ```bash
    sudo apt-get update
    sudo apt-get install libgl1-mesa-dev libssl-dev python3 libfontconfig1-dev
    ```

    **Fedora:**
    ```bash
    sudo dnf install mesa-libGL-devel openssl-devel python3 fontconfig-devel
    ```

> **Note:** We auto-fix the `GLIBC_TUNABLES` static TLS issue. If you see `ERR_DLOPEN_FAILED` and the app restarts itself, that's just Lotus fixing your environment for you. Don't panic.

### Windows / macOS
*   **Status:** "Here be dragons." 🐉
*   It *should* compile. It uses standard crates. I haven't tested it. If it explodes, that's a feature.
*   (Windows) You probably need Visual Studio Build Tools. Good luck.

## 📦 Building (The Waiting Game)

Clone it. Install dependencies.

```bash
git clone https://github.com/1jamie/project-lotus.git
cd project-lotus
npm install
```

**Build the Native Addon:**

```bash
# Debug Build (Faster compilation, still slow)
npm run build:debug

# Release Build (Optimized, takes eons)
npm run build
```

> **Warning:** The first build takes forever. You are compiling a browser engine and a Node runtime binding. Go make a coffee. Read a book. Learn a new language. (though we all know you are scrolling tiktok or reddit, we all know you aren't being productive while the compile runs, none of us ever are) It gets faster after the first time. I promise.

## 🏃 Running It

The best way to see if it works (and marvel at the speed) is the test app.

```bash
# If you didn't build it yet, see above.
npm start

# For the stats nerds:
npm start -- --profile
```

The `--profile` flag prints timing metrics so you can feel superior about your startup times.

## 🧪 Smoke Tests

To verify the raw native binding without the fancy JS wrapper:

```bash
npm test
```

## 🎯 Usage (Code Snippets)

### Hybrid Mode: Serving Files
Stop using Express to serve static files. It's embarrassing.

```javascript
const { ServoWindow, app } = require('servo-node');

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
const { ipcMain } = require('servo-node');

ipcMain.on('channel', (data) => {
    console.log('Renderer said:', data);
    ipcMain.send('reply', { status: 'acknowledged' });
});
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

## 📂 Project Structure (For the curious)

*   `src/lib.rs` - The Brain. Main Rust entry point. Handles N-API, Event Loop, IPC.
*   `src/window_state.rs` - The Memory. Remembers where you put your windows.
*   `src/platform.rs` - The Politeness. Proper OS integrations.
*   `lotus.js` - The Body. High-level Node.js wrapper.
*   `index.js` - The Glue. Native binding loader.
*   `example.js` - The Hello World.
*   `test_app/` - The Real Demo. Full-featured app showing off everything.
*   `cicd_specification.md` - The Factory Instructions.

## 🤝 Contributing

PRs are welcome. If you break the `winit` or `glutin` version requirements, I will close your PR with extreme prejudice. We need specific embedding traits and im already sitting on the edge with winit 0.30.2, dont push me off the edge it has already mentally put me on.

1.  Fork it.
2.  Branch it (`git checkout -b feature/cool-stuff`).
3.  Commit it (`git commit -m 'Added cool stuff'`).
4.  Push it.
5.  PR it.

---
**License:** MIT. Do whatever you want, just don't blame me if your computer takes flight.



**P.S.**

The entire framework (node gui lib) core is 1,781 lines of code.

If that feels suspiciously light, it's because it is. We didn't try to build an OS inside your OS; we just gave Node a window and cut the fat until there was nothing left but speed.

Electron carries the weight of the world. Lotus just carries the pixels.