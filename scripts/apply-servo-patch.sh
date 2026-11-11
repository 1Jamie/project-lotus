#!/usr/bin/env bash
# Idempotent: apply packages/lotus-core/patches/servo-v0.7.0-webgl-broker.patch to the Servo submodule
# when pinned at tag v0.7.0.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SERVO_DIR="$ROOT/packages/lotus-core/servo"
PATCH="$ROOT/packages/lotus-core/patches/servo-v0.7.0-webgl-broker.patch"
# Tag objects are often missing from shallow Actions submodule checkouts; pin by commit too.
PINNED_SERVO_COMMIT="aac43a3f31a259f04a574f5ec4e959c943ad7cc7"

if [[ ! -d "$SERVO_DIR/.git" && ! -f "$SERVO_DIR/.git" ]]; then
  echo "[apply-servo-patch] Servo submodule missing at $SERVO_DIR" >&2
  exit 1
fi

cd "$SERVO_DIR"

HEAD="$(git rev-parse HEAD)"
TAG="$(git describe --tags --exact-match 2>/dev/null || true)"
if [[ "$TAG" != "v0.7.0" && "$HEAD" != "$PINNED_SERVO_COMMIT" ]]; then
  echo "[apply-servo-patch] Expected submodule at tag v0.7.0 (${PINNED_SERVO_COMMIT}), got: ${TAG:-no-tag} @ ${HEAD}" >&2
  exit 1
fi

if git apply --reverse --check "$PATCH" 2>/dev/null; then
  echo "[apply-servo-patch] Patch already applied."
  exit 0
fi

git apply "$PATCH"
echo "[apply-servo-patch] Applied $PATCH"
