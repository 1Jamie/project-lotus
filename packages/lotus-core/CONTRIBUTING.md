# Contributing to @lotus-gui/core

Thanks for your interest in contributing to Lotus! Because Lotus is organized as an npm workspaces monorepo (`@lotus-gui/core` and `@lotus-gui/dev`), you can build and test the native runtime either from the root directory or inside `packages/lotus-core`.

---

## Building Locally

> **Note:** By default, `npm install` downloads pre-built `.node` binaries for end-users. When working on the Rust code (`src/*.rs`) or the Servo submodule (`servo/`), run the build scripts below to compile your local changes.

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

## Patching the Servo Submodule

Lotus tracks upstream Servo v0.7.0 directly in `packages/lotus-core/servo` (not a custom fork). Before building, Lotus applies a minimal 5-line diff (`servo-compositor.patch`) to `components/shared/paint/rendering_context.rs` that passes a `target_fbo` ID into `OffscreenRenderingContext`'s blit callback so multiple panes can composite onto a single window surface.

* The patch is applied automatically before `npm run build` via `scripts/apply-patches.js`.
* You can also apply it manually at any time with:
  ```bash
  cd packages/lotus-core
  npm run patch
  ```
