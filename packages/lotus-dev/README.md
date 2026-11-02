# @lotus-gui/dev

The CLI toolkit for scaffolding, live-reloading, and packaging Lotus desktop applications.

`@lotus-gui/dev` provides the `lotus` command-line tool. It handles creating new projects with `lotus init`, running a debounced hot-reload loop during development with `lotus dev`, and compiling your application into a standalone Node.js Single Executable Application (SEA) wrapped in native OS installers (`deb`, `rpm`, `appimage`, `pacman`, `flatpak`, `msi`, `exe`) with `lotus build`.

* [**Step-by-Step Tutorial (Building Your First Lotus App)**](../../TUTORIAL.md)
* [**`@lotus-gui/core` Runtime API Reference**](../lotus-core/README.md)

---

## Installation

```bash
npm install --save-dev @lotus-gui/dev
```

Once installed in your project, you can run commands via `npx lotus <command>` or inside your `package.json` scripts. You can also scaffold a brand-new project anywhere without installing first:

```bash
npx @lotus-gui/dev init my-app
```

---

## CLI Commands

### 1. `lotus init [projectName]`: Scaffold a New App

Creates a new Lotus project directory complete with `package.json`, `lotus.config.json`, a pre-configured `main.js` entry point, and a starter `ui/index.html`.

```bash
# Interactive mode (prompts for app name, version, author, license, etc.)
npx lotus init my-app

# Non-interactive mode (pass flags directly for CI or scripts)
npx lotus init my-app \
  --name "My App" \
  --app-version 1.0.0 \
  --description "Fast desktop utility" \
  --author "Your Name" \
  --homepage "https://github.com/you/my-app" \
  --license MIT
```

| Flag | Default | Description |
|------|---------|-------------|
| `--name <name>` | `<projectName>` | Display name of the application (`lotus.config.json` `name`). |
| `--app-version <ver>` | `0.1.0` | Initial SemVer version string. |
| `--description <desc>` | `"A Lotus desktop application"` | Short summary used in `package.json` and Linux/Windows installer metadata. |
| `--author <author>` | `""` | Maintainer or publisher name. |
| `--homepage <url>` | `""` | Project homepage or repository URL. |
| `--license <spdx>` | `MIT` | SPDX license identifier. |

---

### 2. `lotus dev [entry]`: Live-Reloading Development Runner

Starts your application with `LOTUS_DEV=true` and watches your project files for changes using `chokidar`.

```bash
npx lotus dev            # Defaults to index.js
npx lotus dev main.js    # Run a custom entry point
```

**How `lotus dev` works:**
* **File Watching:** Monitors your code and `ui/` files while ignoring `node_modules/`, `dist/`, `.git/`, lockfiles, and hidden dotfiles.
* **Debounced Restarts (200ms):** If you save multiple files at once, `lotus dev` waits 200ms so it only restarts your app once.
* **Graceful Shutdown + Force-Kill Fallback:** Sends `SIGTERM` to your running Lotus process and waits up to 2 seconds for windows to close cleanly (saving window state) before falling back to `SIGKILL` to prevent orphaned processes.

---

### 3. `lotus build`: Package Native Installers

Bundles your JavaScript and native addons into a standalone executable and packages it into an OS installer inside `dist/installers/`.

```bash
# Linux Installer Targets
npx lotus build --target deb        # Debian / Ubuntu (.deb) [default]
npx lotus build --target rpm        # Fedora / RHEL / openSUSE (.rpm)
npx lotus build --target appimage   # Portable Linux AppImage (.AppImage)
npx lotus build --target pacman     # Arch Linux (.pkg.tar.zst)
npx lotus build --target flatpak    # Flatpak bundle (.flatpak)

# Windows Installer Targets (run on a Windows host or CI runner)
npx lotus build --platform win32 --target msi   # WiX MSI installer (.msi)
npx lotus build --platform win32 --target exe   # NSIS setup executable (.exe)

# Closed-Source Encrypted Build (packs ui/ into an AES-256-GCM VFS inside the binary)
npx lotus build --target deb --encrypt
```

| Flag | Supported Values | Default | Description |
|------|------------------|---------|-------------|
| `--target <format>` | `deb`, `rpm`, `appimage`, `pacman`, `flatpak`, `msi` (`wix`), `exe` (`nsis`) | `deb` | Target installer format to generate. |
| `--platform <os>` | `linux`, `win32` | Current OS (`process.platform`) | Target operating system. Because Lotus bundles native `.node` binaries and patches host executables, build on the target OS (or matching CI runner). |
| `--encrypt` | *(boolean flag)* | `false` | Encrypts your frontend UI assets into an AEAD (AES-256-GCM) Virtual File System injected directly into the binary. |

---

### 4. `lotus clean`: Clean Build Artifacts

Removes the `dist/` staging and installer directory.

```bash
npx lotus clean
```

---

## Configuring `lotus.config.json`

`lotus.config.json` sits in your project root and controls how `@lotus-gui/dev` bundles and packages your application.

### Full Example

```json
{
    "name": "MyApp",
    "version": "1.0.0",
    "appId": "com.example.myapp",
    "license": "MIT",
    "description": "A desktop application built with Lotus",
    "main": "main.js",
    "executableName": "my-app",
    "icon": "./assets/icon.png",
    "author": "Your Name",
    "homepage": "https://github.com/you/my-app",
    "resources": ["./ui", "./assets"],
    "build": {
        "linux": {
            "wmClass": "my-app",
            "section": "utils",
            "categories": ["Utility", "Development"]
        },
        "windows": {},
        "wix": {},
        "nsis": {}
    }
}
```

### Configuration Reference

| Field | Required | Description |
|-------|----------|-------------|
| `name` | **Yes** | Display name of your application (`productName` in installers and desktop entries). |
| `version` | **Yes** | SemVer version string (`Major.Minor.Patch`, e.g., `"1.0.0"`). |
| `main` | No | Entry point script. Falls back to `package.json` `main`, then `index.js`. |
| `executableName` | No | Output binary name (e.g., `my-app` → `/usr/bin/my-app` or `my-app.exe`). Defaults to lowercase hyphenated `name`. |
| `appId` | No | Reverse-DNS identifier (e.g., `com.company.myapp`). Defaults to `com.lotus.<executableName>` (or `org.lotus.<executableName>` for Flatpak). |
| `description` | No | Package description shown in OS package managers. |
| `author` | No | Maintainer / Publisher name (used in Windows Registry and Linux package metadata). |
| `license` | No | SPDX license identifier. Defaults to `"Proprietary"`. |
| `homepage` | No | Project URL included in package metadata. |
| `icon` | No | Path to your application icon (`.png`, `.jpg`, or `.ico`). On Windows, PNG/JPG icons are automatically wrapped into `.ico` containers; on Linux Flatpak/RPM builds, icons are resized to `512x512` in `/usr/share/icons/hicolor/`. |
| `resources` | Recommended | Array of extra directories/files to copy into the packaged app (e.g. `["./ui"]`). Note: `lotus build` also auto-detects `ui/`, `public/`, `assets/`, `static/`, `web/`, and `renderer/` if present. |
| `build.linux` | No | Linux desktop integration options: `wmClass`, `section`, `categories`. |
| `build.windows` | No | Passed through to CrabNebula packager `windows` config (e.g., code signing `certificateThumbprint`, `signCommand`). |
| `build.wix` | No | Passed through to CrabNebula WiX (`.msi`) configuration (`languages`, `template`, `fragments`). |
| `build.nsis` | No | Passed through to CrabNebula NSIS (`.exe`) configuration (`installMode`, `compression`, `preinstallSection`). |

---

## How the `lotus build` Pipeline Works

When you run `npx lotus build`, `@lotus-gui/dev` runs six steps:

1. **JS Bundling & FHS Path Shimming (`esbuild`):**
   Bundles your `main.js` dependency tree into a single file (`bundle.js`) while keeping `.node` addons external. It injects a `__dirname` shim that locates native `.node` files and `msgpackr-renderer.js` whether the app is running from `/usr/bin/<app>` (looking in `/usr/lib/<app>/`), inside a Flatpak (`/app/lib/<app>/`), or next to a Windows `.exe`.
2. **Native Addon & ANGLE Extraction:**
   Crawls `node_modules` (including monorepo parent folders) to extract the `.node` binary and `msgpackr-renderer.js`. On Windows builds, it also extracts `libEGL.dll`, `libGLESv2.dll`, and `d3dcompiler_47.dll` for hardware-accelerated ANGLE rendering.
3. **Node SEA (Single Executable Application) Compilation:**
   Generates a V8 SEA blob (`node --experimental-sea-config`) and injects it into a copy of the Node.js binary via `postject`.
4. **Encrypted VFS & Key Sharding (when `--encrypt` is passed):**
   Generates a random 32-byte master key, shards it into three parts (`s1 ^ s2 ^ s3 = masterKey` where Shard 1 lives in compiled Rust and Shards 2 & 3 are injected as native binary sections `LOTUS_APP_S1` and `LOTUS_APP_S2`), encrypts every file in `ui/` with **AES-256-GCM**, and embeds the `LOTUS_VFS` section into the executable via `objcopy` before stripping raw `ui/` files from the installer.
5. **Windows PE Header Patching & Silent VC++ Redistributable:**
   On Windows, `node.exe` is compiled as a console subsystem binary (`IMAGE_SUBSYSTEM_WINDOWS_CUI`), which normally opens a black command prompt window behind GUI apps. `lotus build` patches the PE header subsystem byte to `IMAGE_SUBSYSTEM_WINDOWS_GUI` (`2`) so your app launches without a console window. It also downloads and embeds `vc_redist.x64.exe` into the MSI/NSIS installer.
6. **OS Packaging:**
   * **`deb`, `pacman`, `msi`, `exe`:** Packaged via `@crabnebula/packager`.
   * **`appimage`:** Downloads `appimagetool` to `~/.lotus-gui/tools/` (once) and builds a self-contained `.AppImage` with an FHS-compliant `AppDir` and `AppRun` wrapper.
   * **`rpm`:** Generates a `.spec` file and invokes `rpmbuild` to package `/usr/bin/<app>`, `/usr/lib/<app>/*`, `.desktop` entries, and icons.
   * **`flatpak`:** Generates an `org.freedesktop.Platform` manifest (with X11, Wayland, DRI, and IPC permissions) and invokes `flatpak-builder` + `flatpak build-bundle`.

### Host System Requirements for Building Installers

* **All Platforms:** Node.js v22+ (with SEA support) and a `lotus.config.json` file.
* **Linux (`.deb`):** `dpkg-dev` and `fakeroot` (`sudo apt install dpkg-dev fakeroot`).
* **Linux (`.rpm`):** `rpm-build` (`sudo dnf install rpm-build`).
* **Linux (`.flatpak`):** `flatpak` and `flatpak-builder` with `org.freedesktop.Platform//24.08` and `org.freedesktop.Sdk//24.08`.
* **Linux (`--encrypt`):** `binutils` (`objcopy`).
* **Windows (`.msi` / `.exe`):** Must run on Windows; requires WiX Toolset v3 (for `.msi`) or NSIS (for `.exe`). Keep your `version` in `Major.Minor.Patch` format (e.g. `1.0.0`) and keep `appId` constant across releases so Windows MSI upgrades recognize your existing installation.

---

## Build Output Structure

After running `lotus build`, your `dist/` directory contains:

```text
dist/
├── app/                                  # Staged SEA binary & extracted native dependencies
│   ├── my-app                            # Standalone executable (with SEA + optional VFS)
│   ├── lotus.linux-x64-gnu.node          # Native Servo/Rust runtime addon
│   └── msgpackr-renderer.js              # Renderer IPC serialization script
└── installers/                           # Generated OS installers
    ├── my-app-1.0.0-x86_64.AppImage
    ├── my-app_1.0.0_amd64.deb
    └── my-app-1.0.0-1.x86_64.rpm
```

## License

MIT
