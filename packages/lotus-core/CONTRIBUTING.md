# Contributing to @lotus-gui/core

Thanks for your interest in contributing to Lotus! Because Lotus is organized as an npm workspaces monorepo (`@lotus-gui/core` and `@lotus-gui/dev`), you can build and test the native runtime either from the root directory or inside `packages/lotus-core`.

---

## Building Locally

> **Note:** By default, `npm install` downloads pre-built `.node` binaries for end-users. When working on the Rust code (`src/*.rs`), run the build scripts below to compile your local changes. Servo is the git submodule at `servo/` (tag `v0.7.0`) plus `patches/servo-v0.7.0-webgl-broker.patch`, applied automatically by `npm run build` / `scripts/apply-servo-patch.sh`.

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
```

---

## Servo

Lotus depends on the published `servo` crate at version 0.7.0. Trusted panes use `LotusPaneContext`, a `RenderingContext` in this crate, so each pane paints into its own framebuffer on the window GL context. The compositor blits those framebuffers into the scene.
