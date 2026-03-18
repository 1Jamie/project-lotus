# @lotus-gui/dev

**The CLI toolkit for building, debugging, and packaging Lotus applications.**

`@lotus-gui/dev` provides the tools necessary to initialize projects, run a development server with hot-reload, and package your application into native installers for multiple platforms.

---

## 📦 What’s New in v0.3.0 (Distribution & Packaging)

The v0.3.0 release introduces automated installer generation, turning your project into a distributable product with a single command.

*   **🏗️ One-Command Build:** Generate professional installers (DEB, RPM, MSI, NSIS) tailored for your target platform using `lotus build`.
*   **🪟 Windows Support (MSI/EXE):** Fully integrated WiX-based MSI and NSIS-based EXE generation.
    *   **VC++ Redist Chaining:** Automatically embeds and installs the Microsoft C++ Runtime silently.
    *   **ANGLE Injection:** Bundles necessary DLLs for hardware-accelerated rendering on Windows 10 & 11.
    *   **Icon Conversion:** Automatically handles PNG/JPG to `.ico` conversion for application icons.
*   **🐧 Linux Distribution:** Native support for DEB, RPM, AppImage, Pacman, and Flatpak.
*   **🔥 Hot-Reload Dev Server:** A high-performance development environment that monitors file changes and auto-restarts the engine for rapid iteration.

---

## Installation

```bash
npm install @lotus-gui/dev
```

This installation provides the `lotus` CLI command.

---

## CLI Commands

### `lotus dev [entry]`

Launch your application with hot-reloading. The CLI watches for file changes and automatically restarts the process.

```bash
lotus dev              # Uses index.js or entry in package.json
lotus dev main.js      # Uses custom entry point
```

*   **Watch Mode:** Monitors all source files while ignoring `node_modules` and `dist`.
*   **Clean Lifecycle:** Processes are terminated cleanly before restart to avoid resource leaks.

**What it does:**
- Starts your app with Node.js.
- Watches all files for changes.
- Auto-restarts on any file change.
- Kills the previous process before starting the new one.

### `lotus init [projectName]`

Initialize a new Lotus project with interactive prompts for metadata.

```bash
lotus init my-app        # Creates 'my-app' directory
```

**Interactive Prompts:**
If flags are not provided, the CLI will ask for:
- Project Name (directory)
- Application Name
- Version
- Description
- Author
- Homepage / Repository URL
- License

**Non-Interactive Flags:**
```bash
lotus init my-app --name "My App" --app-version 1.0.0 --homepage "https://example.com"
```

| Flag | Description |
|------|-------------|
| `--name` | Application display name. |
| `--app-version` | Application version (Semver). |
| `--description` | Short project description. |
| `--author` | Author name. |
| `--homepage` | Project homepage or repository URL. |
| `--license` | License identifier (default: `MIT`). |

### `lotus build`

Bundle your application using Node SEA and `@crabnebula/packager` to create native installers.

```bash
# Example Linux Targets
lotus build --target deb
lotus build --target appimage

# Example Windows Targets (must run on Windows)
lotus build --target msi --platform win32
lotus build --target exe --platform win32
```

| Flag | Values | Default | Description |
|------|--------|---------|-------------|
| `--target` | `deb`, `appimage`, `pacman`, `rpm`, `flatpak`, `msi`, `exe` | `deb` | Target installer format. |
| `--platform` | `linux`, `win32` | Current OS | Target OS platform. |

**System Requirements:**
- `lotus.config.json` in the current directory.
- Modern Node.js (v20+ with SEA support).
- **Windows (`msi`/`exe`)**: WiX Toolset v3 must be installed and builds must run on a Windows host.

### `lotus clean`

Remove the `dist/` directory and all build artifacts.

---

## `lotus.config.json`

The build system as well as the runtime utilize `lotus.config.json` for application metadata.

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

### Configuration Reference

| Field | Required | Description |
|-------|----------|-------------|
| `name` | Yes | Application display name used as `productName` in installers. |
| `version` | Yes | Semver version string (e.g., `"1.0.0"`). |
| `license` | No | SPDX license identifier. Defaults to `"Proprietary"`. |
| `description` | No | Short description shown in package managers. |
| `main` | No | Entry point file. Defaults to `package.json` `main`. |
| `executableName`| No | Binary name (e.g., `my-app`). Defaults to lowercase `name`. |
| `icon` | No | Path to application icon (relative to project root). |
| `author` | No | Maintainer name. Highly recommended for Windows installers. |
| `homepage` | No | Project URL for package metadata. |
| `appId` | No | Reverse domain identifier (e.g., `com.company.app`). |
| `resources` | **Rec.** | Array of paths to bundle (e.g., `["./ui"]`). Required for UI files. |

### OS-Specific Options

#### Windows Build Options (`build.windows`, `build.wix`, `build.nsis`)

| Field | Description |
|-------|-------------|
| `build.windows` | Configures signing: `certificateThumbprint`, `signCommand`, etc. |
| `build.wix` | Configures MSI options: `fipsCompliant`, `languages`, `template`. |
| `build.nsis` | Configures EXE options: `installMode`, `compression`, `languages`. |

#### Linux Build Options (`build.linux`)

| Field | Description |
|-------|-------------|
| `wmClass` | Window manager class identifier for taskbar grouping. |
| `section` | Package section (default: `"utils"`). |
| `categories` | Desktop entry categories (e.g., `["Utility"]`). |

### Windows Packaging Requirements

When building for Windows, the following rules are strictly enforced:
1. **Version Format**: Version must be `Major.Minor.Patch`. Pre-release tags are automatically stripped for installer compatibility.
2. **`appId` / UpgradeCode**: The `appId` determines the MSI `UpgradeCode`. Changing it in the future will prevent Windows from recognizing the update as the same application.
3. **`author` / Publisher**: Acts as the Windows Registry Manufacturer. Defaults to `"Lotus Dev"` if omitted.

---

## Build Pipeline

The `lotus build` command follows these steps:
1.  **Read Config:** Reads `lotus.config.json` from the current directory.
2.  **Bundling:** Uses `esbuild` to bundle application JS into a single file and proxies native modules.
3.  **Discovery:** Discovers native `.node` modules and copies them out for the installer.
4.  **SEA Generation:** Creates a Node.js Single Executable Application blob.
5.  **Injection:** Injects the payload into a Node.js binary.
6.  **Packaging:** Invokes CrabNebula to wrap the binary, native modules, and resources into the final installer format.

## Build Output

After running `lotus build`, the `dist/` directory contains:

```
dist/
├── app/                    # Staged application components
│   ├── my-app              # Node SEA Single Executable Binary
│   └── lotus.node          # Extracted native bindings
└── installers/             # Generated OS-specific installers
    ├── my-app-1.0.0.AppImage 
    └── my-app_1.0.0_amd64.deb
```

## Project Setup Example

A minimal Lotus project follows this structure:

```
my-lotus-app/
├── lotus.config.json    # Build configuration
├── package.json         # Dependencies (@lotus-gui/core, @lotus-gui/dev)
├── main.js              # Entry point
└── ui/
    └── index.html       # Application UI
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

## Dependencies

| Package | Purpose |
|---------|---------|
| `commander` | CLI argument parsing. |
| `chokidar` | File watching for hot-reload. |
| `esbuild` | JS bundling and code transformation. |
| `@crabnebula/packager` | Installer generation for DEB, RPM, MSI, etc. |
| `postject` | SEA binary injection. |

## Package Structure

```
@lotus-gui/dev
├── bin/lotus.js          # CLI entry point (commander-based build pipeline)
├── index.js              # Package entry (exports CLI path)
└── package.json
```

---

## License

MIT


**AI DISCLAIMER**

I have used ai in the project for templating, troubleshooting, diving through source code to find the reference i needed to read and learn how worked, and documentation originally while i was rapidly itteratting and have since rewrote the documentation myself now the api is more stable. I have tried my best and spent countless hours and days to ensure that the code is correct and that the documentation is accurate, its not a huge code base so i have touched and worked with every line of code in this repo. I will tell you that this is not some BS "vibe coded" system or project. I have worked in IT, Programming and Engineering for going on 11 years. I spent the last 7 years as a Linux enterprise dev/ Linux systems engineer. I have spent a lot of time designing, testing working and griding my hard hours on this project and ensuring this is not some garbage that a unexpirenced person whipped together without actually knowing and understand the way computers work and how to practice proper programing hygene, testing and safe software lifecyle practices. I know what is where, why what works what way, i decided how each and every piece works, written a large amount of it, i have tested different setups and dependancies like the ipc system. I spent a lot of time researching takio, readind the docs figuring out exactly how to use it with my use case and axum so i can web socket the world of my ipc. I dumped so many hours into learning winit 0.30 (which btw, it is a pain to use by its features and layout are amazing, i do recomend the time to learn the new conventions of 0.30), i have poured over so much of the servo repo trying to get the best pieces integrated as nicely as possible and spent so much time fighting with the windows rendering pipeline. i do understand that there is a lot of worries around it and people vibe coding stuff but this is not "vibe coded", this is hundreads of my hours, nights and nights of deep coffee pots and genuine passion for this project. It was involved in this project but it is not running, planning, testing, integrating, designing or any of that, it was just a tool used to speed up piece here and there. There are way too many moving parts here and i have spent way too much time on this to have it reduced down to that.
