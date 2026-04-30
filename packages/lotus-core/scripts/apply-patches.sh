#!/bin/bash
# Lotus Servo Patching Script
# This ensures our custom engine modifications are layered correctly over the upstream submodule.

SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" && pwd )"
PROJECT_ROOT="$SCRIPT_DIR/.."
SERVO_DIR="$PROJECT_ROOT/servo"
PATCH_FILE="$PROJECT_ROOT/servo-compositor.patch"

echo "[Lotus] Applying Servo engine patches..."

if [ ! -d "$SERVO_DIR" ]; then
    echo "[Error] Servo submodule directory not found at $SERVO_DIR"
    exit 1
fi

if [ ! -f "$PATCH_FILE" ]; then
    echo "[Error] Patch file not found at $PATCH_FILE"
    exit 1
fi

# Apply the patch
cd "$SERVO_DIR"
git apply "../$(basename "$PATCH_FILE")"

if [ $? -eq 0 ]; then
    echo "[Lotus] Servo patches applied successfully."
else
    echo "[Error] Failed to apply Servo patches. They might already be applied or there's a conflict."
    # We exit 0 here to avoid breaking the build if the patch is already applied
    exit 0
fi
