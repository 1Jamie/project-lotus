# @lotus-gui/dev

CLI toolkit for developing, building, and packaging Lotus applications into distributable installers.

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

Build your application into a distributable installer package.

```bash
lotus build --platform linux --target deb
lotus build --platform linux --target rpm
```

| Flag | Values | Default | Description |
|------|--------|---------|-------------|
| `--platform` | `linux`, `win32` | Current OS | Target platform (Windows support is experimental/WIP) |
| `--target` | `deb`, `rpm` | `deb` | Installer format (Linux only) |

**What it does:**
1. Reads `lotus.config.json` from the current directory
2. Copies your app files into a staging directory (`dist/app/resources/app/`)
3. Copies `node_modules` (preserving package structure)
4. Generates a wrapper shell script as the executable
5. Packages everything into a `.deb` or `.rpm` installer
6. Output goes to `dist/installers/`

**System Requirements:**
- `lotus.config.json` in the current directory
- For RPM targets (Fedora/RHEL):
  ```bash
  sudo dnf install rpm-build
  ```
- For DEB targets (Ubuntu/Debian):
  ```bash
  sudo apt install dpkg-dev fakeroot
  ```

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
| `author` | No | Maintainer name for package metadata. |
| `homepage` | No | Project URL for package metadata. |

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
├── app/                    # Staged application
│   ├── resources/app/      # Your app files + node_modules
│   ├── my-app              # Wrapper shell script
│   ├── version             # Version file
│   └── LICENSE             # License file
└── installers/
    └── my-app-1.0.0-1.x86_64.rpm  # (or .deb)
```

The generated wrapper script runs your app with Node.js:
```bash
#!/bin/sh
exec node "/usr/lib/my-app/resources/app/main.js" "$@"
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

# Build an RPM
npx lotus build --platform linux --target rpm

# Clean build artifacts
npx lotus clean
```

### Install the Built Package

```bash
# RPM (Fedora/RHEL)
sudo dnf install ./dist/installers/my-app-1.0.0-1.x86_64.rpm

# DEB (Ubuntu/Debian)
sudo dpkg -i ./dist/installers/my-app_1.0.0_amd64.deb

# Run it
my-app
```

## Architecture

```
@lotus-gui/dev
├── bin/lotus.js          # CLI entry point (commander-based)
├── lib/templates/
│   └── spec.ejs          # Custom RPM spec template
├── index.js              # Package entry (exports CLI path)
└── package.json
```

### Dependencies

| Package | Purpose |
|---------|---------|
| `commander` | CLI argument parsing |
| `chokidar` | File watching for hot-reload |
| `electron-installer-debian` | `.deb` package generation |
| `electron-installer-redhat` | `.rpm` package generation |
| `electron-winstaller` | Windows installer generation (planned) |

## License

MIT
