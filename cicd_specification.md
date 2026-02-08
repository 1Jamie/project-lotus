# CI/CD Specification

## Overview
This document outlines the requirements for the automation and distribution pipeline for the project. The goal is to establish a robust GitHub Actions workflow for building and releasing the Node.js native addon.

## Release Workflow
**File:** `.github/workflows/release.yml`
**Tools:** `napi-build/action@v2`

### Triggers
The workflow should be triggered by:
*   Pushes to the `main` branch.
*   Pushes of tags matching the pattern `v*` (e.g., `v1.0.0`).

### Build Targets
The build pipeline must support the following targets to ensure cross-platform compatibility:
1.  **Linux:** `x86_64-unknown-linux-gnu`
2.  **Windows:** `x86_64-pc-windows-msvc`
3.  **macOS:** `aarch64-apple-darwin` (Apple Silicon M1/M2/M3)

### Distribution Actions
*   **NPM Publishing:** Automatically upload the build artifacts to the NPM registry.
*   **Authentication:** Use the `NPM_TOKEN` secret for authentication.

## Environment Requirements
To ensure successful builds, the CI environment must be configured with the necessary dependencies:

### Linux Dependencies
*   `libgl1-mesa-dev`
*   `libssl-dev`
*   `python3`

Ensure these packages are installed in the runner before the build step executes.
