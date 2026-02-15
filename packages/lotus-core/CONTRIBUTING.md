# Contributing to @lotus/core

## Building Locally

Since `lotus` is now a monorepo, you can build the core runtime using npm workspaces or by navigating into the directory.

### From Root (Recommended)
To build both the Core and the CLI:
```bash
npm run build
```

To build ONLY the Core (Runtime):
```bash
npm run build --workspace=@lotus/core
```

### From `packages/lotus-core`
```bash
cd packages/lotus-core
npm run build
```

## Binary Distribution
By default, `npm install` tries to download a pre-built binary. To force a local build when installing (e.g., if you are working on the Rust code), you should just run the build command above. The `install.js` script is only for end-users who don't have the repo checked out.
