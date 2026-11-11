#!/usr/bin/env bash
# Idempotent: apply packages/lotus-core/patches/servo-v0.7.0-webgl-broker.patch to the Servo submodule
# when pinned at tag v0.7.0, and redirect crates.io `servo` to that path via [patch.crates-io].
#
# Linux-only: the patch is for the sandboxed GPU broker + jail networking path.
# Windows / other hosts use stock crates.io Servo (no path patch injection).
#
# The Cargo.toml injection is ephemeral (between markers below) — do not commit it.
# `npm run prebuild` / CI apply this on Linux; Windows prebuild no-ops.

set -euo pipefail

SERVO_PATH_PATCH_BEGIN='# >>> lotus-servo-path-patch (linux build only; do not commit)'
SERVO_PATH_PATCH_END='# <<< lotus-servo-path-patch'

inject_servo_path_patch() {
  local cargo_toml="$1"
  if rg -q 'lotus-servo-path-patch' "$cargo_toml"; then
    echo "[apply-servo-patch] Cargo.toml already redirects crates.io servo → submodule path."
    return 0
  fi
  if ! rg -q '^\[patch\.crates-io\]' "$cargo_toml"; then
    printf '\n[patch.crates-io]\n%s\nservo = { path = "servo/components/servo" }\n%s\n' \
      "$SERVO_PATH_PATCH_BEGIN" "$SERVO_PATH_PATCH_END" >> "$cargo_toml"
  else
    local tmp
    tmp="$(mktemp)"
    awk -v begin="$SERVO_PATH_PATCH_BEGIN" -v end="$SERVO_PATH_PATCH_END" '
      BEGIN { done = 0 }
      /^\[patch\.crates-io\]$/ && !done {
        print
        print begin
        print "servo = { path = \"servo/components/servo\" }"
        print end
        done = 1
        next
      }
      { print }
      END {
        if (!done) {
          print ""
          print "[patch.crates-io]"
          print begin
          print "servo = { path = \"servo/components/servo\" }"
          print end
        }
      }
    ' "$cargo_toml" > "$tmp"
    mv "$tmp" "$cargo_toml"
  fi
  echo "[apply-servo-patch] Injected crates.io → path redirect for servo into Cargo.toml."
}

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "[apply-servo-patch] Skipping on $(uname -s) (sandbox Servo patch is Linux-only)."
  exit 0
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SERVO_DIR="$ROOT/packages/lotus-core/servo"
PATCH="$ROOT/packages/lotus-core/patches/servo-v0.7.0-webgl-broker.patch"
CARGO_TOML="$ROOT/packages/lotus-core/Cargo.toml"
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
else
  git apply "$PATCH"
  echo "[apply-servo-patch] Applied $PATCH"
fi

inject_servo_path_patch "$CARGO_TOML"
