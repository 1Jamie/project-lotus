# Contributing to @lotus-gui/core

Thanks for your interest in contributing to Lotus! The repo is an **npm workspaces** monorepo for `@lotus-gui/core` and `@lotus-gui/dev`, plus sibling Rust crates `packages/lotus-engine` and `packages/lotus-supervisor` for Linux process-isolated sandboxes. You can build and test the host runtime from the repo root or inside `packages/lotus-core`; build the engine/supervisor crates when working on the sandboxed path.

---

## Building Locally

> **Note:** By default, `npm install` downloads pre-built `.node` binaries for end-users. When working on the Rust code (`src/*.rs`), run the build scripts below to compile your local changes. Servo is the git submodule at `packages/lotus-core/servo` (tag `v0.7.0`) plus `patches/servo-v0.7.0-webgl-broker.patch`, applied automatically by `npm run build` / `scripts/apply-servo-patch.sh`.

### From the Monorepo Root
```bash
# Build all workspaces
npm run build

# Build only @lotus-gui/core
npm run build --workspace=@lotus-gui/core
```

### From `packages/lotus-core` (Recommended for Rust Development)
```bash
cd packages/lotus-core

# Fast incremental debug build (best for local iteration)
npm run build:debug

# Optimized release build
npm run build
```

---

## Running the Showcase & Benchmark Apps

After compiling `@lotus-gui/core`, you can test your changes against the built-in showcase apps:

```bash
cd packages/lotus-core

# 1. Single-window frameless/transparent showcase
npm run test-app

# 2. Multi-pane layout & floating command palette showcase
npm run multipane-test-app

# 3. High-frequency IPC stress benchmark
npm run benchmark

# 4. Process-isolated sandbox + Track 2 + WebGL broker showcase
npm run sandbox-test-app
```

### Sandbox / WebGL broker debug flags

| Variable | Effect |
|----------|--------|
| `LOTUS_DISABLE_GPU_BROKER=1` | Skip host GPU socket registration at sandbox spawn (no `LOTUS_GPU_FD` on the child). |
| `LOTUS_WEBGL_HOLE_PUNCH` | Experimental opaque WebGL hole-punch on the host compositor (off by default). |
| `LOTUS_GPU_FD` | Internal FD set by the launcher for the engine—do not set this in apps. |

---

## Servo

Lotus depends on the published `servo` crate at version 0.7.0. Trusted panes use `LotusPaneContext`, a `RenderingContext` in this crate, so each pane paints into its own framebuffer on the window GL context. The compositor blits those framebuffers into the scene.

Sandboxed `lotus-engine` uses `SoftwareRenderingContext` + sealed SHM for page pixels. Canvas WebGL is brokered to the host via `patches/servo-v0.7.0-webgl-broker.patch` and `src/gl/webgl_broker.rs` when `LOTUS_GPU_FD` is present. Use the env flags above only when debugging those paths.
