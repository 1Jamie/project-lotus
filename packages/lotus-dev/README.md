# @lotus-gui/dev
The Swiss Army Knife for building, debugging, and packaging Lotus applications.

📦 v0.3.0: The "Installer Factory" Update
The jump to 0.3.0 turns your project into a distributable product. You write the code; we build the dealership.

🏗️ One-Command Build: Run npx lotus build and get a professional installer tailored for your platform. No more messing with complex C++ build flags or installer scripts.

🪟 Windows Sovereignty (MSI/EXE): Fully working WiX-based MSI generation. We handle the heavy lifting:

VC++ Redist Chaining: Automatically embeds and silently installs the Microsoft C++ Runtime.

ANGLE Injection: Bundles the necessary DLLs for hardware-accelerated rendering on any Windows 10/11 machine.

Icon Conversion: Automatically handles PNG/JPG to .ico conversion for your installers.

🐧 Linux Distribution Mastery: Support for the "Big Five" targets: DEB, RPM, AppImage, Pacman, and Flatpak.

🔥 Hot-Reload Dev Server: Our dev environment watches your Rust bindings and JS source, restarting the engine only when necessary to keep your feedback loop tight.

## Installation

```bash
npm install @lotus-gui/dev
```

This provides the `lotus` CLI command.

## CLI Commands

### `lotus dev [entry]`

Launch your app with hot-reloading. Watches for file changes and auto-restarts.

```bash
lotus dev              # Uses index.js
lotus dev main.js      # Custom entry point
```

**What it does:**
- Starts your app with Node.js
- Watches all files (ignoring `node_modules`, `dist`, dotfiles)
- Auto-restarts on any file change
- Kills the previous process cleanly

- Kills the previous process cleanly
 
 ### `lotus init [projectName]`
 
 Initialize a new Lotus project with interactive prompts.
 
 ```bash
 lotus init               # Prompts for project name
 lotus init my-app        # Creates 'my-app' directory
 ```
 
 **Interactive Prompts:**
 If flags are not provided, the CLI will ask for:
 - Project Name (directory)
 - Application Name
 - Version
 - Description
 - Author
 - Homepage / Repository URL (Important for Linux packages)
 - License
 
 **Non-Interactive Flags:**
 You can skip prompts by providing flags:
 ```bash
 lotus init my-app --name "My App" --app-version 1.0.0 --homepage "https://example.com"
 ```
 
 | Flag | Description |
 |------|-------------|
 | `--name` | Application display name |
 | `--app-version` | Application version (e.g., `1.0.0`) |
 | `--description` | Short description |
 | `--author` | Author name |
 | `--homepage` | Repository or homepage URL |
 | `--license` | License identifier (default: `MIT`) |
 
 ### `lotus build`

Build your application into a native, single-executable distributable installer package using Node SEA and CrabNebula.

```bash
# Linux
lotus build --target deb
lotus build --target appimage
lotus build --target pacman
lotus build --target rpm
lotus build --target flatpak

# Windows (must run on Windows host)
lotus build --target wix --platform win32    # .msi installer
lotus build --target nsis --platform win32   # .exe installer
```

| Flag | Values | Default | Description |
|------|--------|---------|-------------|
| `--target` | `deb`, `appimage`, `pacman`, `rpm`, `flatpak` *(Linux)* · `wix` / `msi`, `nsis` / `exe` *(Windows)* | `deb` | Target installer format. `msi` is an alias for `wix`; `exe` is an alias for `nsis`. |
| `--platform` | `linux`, `win32` | current OS | Target platform. Set to `win32` when building Windows packages on a Windows host. |

**What it does:**
1. Reads `lotus.config.json` from the current directory.
2. Recursively bundles your application JS using `esbuild`.
3. Discovers native `.node` modules and copies them out of the bundle.
4. Generates a Node Single Executable Application (SEA) blob and injects it into a Node.js binary.
5. Invokes `@crabnebula/packager` to wrap the executable and native modules into the final OS-specific installer.
6. Build artifacts go to `dist/app/` and final installers go to `dist/installers/`.

**System Requirements:**
- `lotus.config.json` in the current directory
- Modern Node.js (v20+ with SEA support)
- **Windows (`wix`/`nsis`)**: WiX Toolset v3 must be installed ([wixtoolset.org](https://wixtoolset.org)) and builds must run on a Windows host.

### `lotus clean`

Remove the `dist/` build artifacts directory.

```bash
lotus clean
```

## `lotus.config.json`

The build command reads configuration from `lotus.config.json` in your project root. This file controls both the build output and the installer metadata.

### Full Example

```json
{
    "name": "MyApp",
    "version": "1.0.0",
    "license": "MIT",
    "description": "A desktop application built with Lotus",
    "main": "main.js",
    "executableName": "my-app",
    "icon": "./assets/icon.png",
    "author": "Your Name",
    "homepage": "https://github.com/you/my-app",
    "resources": ["./ui"],
    "build": {
        "linux": {
            "wmClass": "my-app",
            "section": "utils",
            "categories": ["Utility", "Development"]
        }
    }
}
```

### Field Reference

| Field | Required | Description |
|-------|----------|-------------|
| `name` | Yes | Application display name. Used as `productName` in installers. |
| `version` | Yes | Semver version string (e.g., `"1.0.0"`). |
| `license` | No | SPDX license identifier (e.g., `"MIT"`, `"ISC"`). Defaults to `"Proprietary"`. |
| `description` | No | Short description shown in package managers. |
| `main` | No | Entry point file. Determines what the installed app runs. Falls back to `package.json`'s `main`, then `index.js`. |
| `executableName` | No | Binary name (e.g., `my-app` → `/usr/bin/my-app`). Defaults to lowercase `name`. |
| `icon` | No | Path to application icon (relative to project root). |
| `author` | No | Maintainer name for package metadata. It is highly recommended to set this for Windows, as it maps to the Registry `Manufacturer` identity. |
| `homepage` | No | Project URL for package metadata. |
| `appId` | No | Reverse domain identifier (e.g., `com.company.app`). For Windows WiX, this is used to stably generate the `UpgradeCode`. Changing this will break Windows installer upgrades. |
| `resources` | **Recommended** | Array of paths/globs to include in the installer (e.g., `["./ui", "./assets"]`). **Required for UI to render after installation** — when installed, resources are placed next to the executable, which is where `path.join(__dirname, 'ui')` resolves. Common directories (`ui/`, `public/`, `assets/`, `static/`) are auto-detected if present. |


### Windows Build Options (`build.windows`, `build.wix`, `build.nsis`)

You can pass configuration objects directly to the underlying CrabNebula Packager for Windows installers.

| Field | Description |
|-------|-------------|
| `build.windows` | Configures signing: `certificateThumbprint`, `signCommand`, `digestAlgorithm`, etc. |
| `build.wix` | Configures MSI options: `fipsCompliant`, `languages`, `template`, `fragmentPaths`. |
| `build.nsis` | Configures EXE options: `installMode` (currentUser/perMachine), `compression`, `languages`. |

### Windows Packaging Requirements

When building for Windows (`--target wix` or `--target nsis`), the underlying tools (WiX Toolset specifically) enforce strict validation that will fail builds or break updates if not followed:
1. **Version Format**: Version **must** be exactly `Major.Minor.Patch` (e.g., `1.0.0`). The numbers cannot exceed 65535. Pre-release tags (e.g. `1.0.0-beta`) are heavily discouraged, and the installer generation will gracefully strip them, acting as the base version.
2. **`appId` / UpgradeCode**: The `appId` you configure determines the MSI's `UpgradeCode`. If you omit `appId`, it defaults to `com.lotus.my-app`. If you change the `appId` in the future, Windows will treat the update as an entirely new application and will *not* uninstall the old one.
3. **`author` / Publisher**: The `author` field acts as the Windows Registry Manufacturer. If omitted, it defaults to `"Lotus Dev"`. You should set an `author` immediately for proper organization in "Add/Remove Programs".

### Linux Build Options (`build.linux`)

| Field | Description |
|-------|-------------|
| `wmClass` | Window manager class identifier. Used for taskbar grouping. |
| `section` | Package section (default: `"utils"`). |
| `categories` | Desktop entry categories (e.g., `["Utility"]`). |

## Build Output

After running `lotus build`, the `dist/` directory contains:

```
dist/
├── app/                    # Staged application components
│   ├── my-app              # Node SEA Single Executable User Binary
│   ├── lotus.linux-x64-gnu.node # Extracted native bindings
│   └── msgpackr-renderer.js
└── installers/
    ├── my-app-1.0.0-x86_64.AppImage 
    └── my-app_1.0.0_amd64.deb
```

## Project Setup Example

A minimal Lotus project looks like this:

```
my-lotus-app/
├── lotus.config.json    # Build configuration
├── package.json         # npm dependencies (@lotus-gui/core, @lotus-gui/dev)
├── main.js              # App entry point
└── ui/
    └── index.html       # Your UI
```

**`package.json`:**
```json
{
    "name": "my-lotus-app",
    "version": "1.0.0",
    "main": "main.js",
    "dependencies": {
        "@lotus-gui/core": "^0.2.0"
    },
    "devDependencies": {
        "@lotus-gui/dev": "^0.2.0"
    }
}
```

### Development Workflow

```bash
# Run with hot-reload
npx lotus dev main.js

# Build an AppImage for Linux
npx lotus build --target appimage

# Clean build artifacts
npx lotus clean
```

### Install the Built Package

```bash
# DEB (Ubuntu/Debian)
sudo apt install ./dist/installers/my-app_1.0.0_amd64.deb

# Run it
my-app

# Or use the portable AppImage directly!
./dist/installers/my-app-1.0.0-x86_64.AppImage
```

## Architecture

```
@lotus-gui/dev
├── bin/lotus.js          # CLI entry point (commander-based build pipeline)
├── index.js              # Package entry (exports CLI path)
└── package.json
```

### Dependencies

| Package | Purpose |
|---------|---------|
| `commander` | CLI argument parsing |
| `chokidar` | File watching for hot-reload |
| `esbuild` | Code bundling and __dirname proxying |
| `@crabnebula/packager` | OS installation package generation (`.deb`, `.msi`, etc.) |
| `postject` | Injecting payloads into Node.js SEA binaries |

## License

MIT
