#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# Whitemane Launcher — Linux/macOS setup script
#
# Linux: detects the distro, installs runtime dependencies, downloads the
# AppImage, and creates a desktop entry. Works on every major distro
# including SteamOS (immutable FS + stale PGP keys).
#
# macOS: downloads the DMG, copies the .app bundle into /Applications, and
# strips the quarantine xattr so Gatekeeper lets the ad-hoc-signed bundle
# launch. Apple Silicon only.
#
# Usage:
#   curl -fsSL https://launcher.gamefreedom.org/setup.sh | bash
#   # or
#   bash setup.sh [--uninstall] [--no-deps] [--extract] [--install-dir DIR]
# ---------------------------------------------------------------------------
set -euo pipefail

CDN_BASE="https://launcher.gamefreedom.org/artifacts/whitemane"
APPIMAGE_NAME="Whitemane.AppImage"
DMG_NAME="Whitemane.dmg"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/share/whitemane-launcher}"
BIN_DIR="$HOME/.local/bin"
BIN_LINK="$BIN_DIR/whitemane-launcher"
DESKTOP_DIR="$HOME/.local/share/applications"
DESKTOP_FILE="$DESKTOP_DIR/whitemane-launcher.desktop"
ICON_DIR="$HOME/.local/share/icons"

SKIP_DEPS=false
EXTRACT_MODE=false
UNINSTALL=false
STEAMOS_WAS_RO=false
CLEANUP_FILES=()

# ---------------------------------------------------------------------------
# UI helpers
# ---------------------------------------------------------------------------
RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
BLUE='\033[0;34m'; BOLD='\033[1m'; NC='\033[0m'

info()  { printf "${BLUE}::${NC} %s\n" "$*"; }
ok()    { printf "${GREEN}✓${NC} %s\n" "$*"; }
warn()  { printf "${YELLOW}!${NC} %s\n" "$*"; }
err()   { printf "${RED}✗${NC} %s\n" "$*" >&2; }
die()   { err "$*"; exit 1; }

# Safe prompt — works even when stdin is a pipe (curl | bash)
ask_yn() {
  local prompt="$1" default="${2:-n}"
  if [[ -t 0 ]]; then
    read -rp "$prompt " ans </dev/stdin
  elif [[ -e /dev/tty ]]; then
    read -rp "$prompt " ans </dev/tty
  else
    ans="$default"
  fi
  [[ "$ans" =~ ^[Yy]$ ]]
}

# ---------------------------------------------------------------------------
# Cleanup on failure — remove partial downloads
# ---------------------------------------------------------------------------
cleanup_on_exit() {
  local code=$?
  if [[ $code -ne 0 ]]; then
    for f in "${CLEANUP_FILES[@]}"; do
      [[ -f "$f" ]] && rm -f "$f"
    done
  fi
  # SteamOS: re-lock if we crashed while unlocked
  if [[ "$STEAMOS_WAS_RO" == true ]]; then
    warn "Re-locking SteamOS filesystem after error …"
    sudo steamos-readonly enable 2>/dev/null || true
  fi
}
trap cleanup_on_exit EXIT

# ---------------------------------------------------------------------------
# CLI parsing
# ---------------------------------------------------------------------------
usage() {
  cat <<EOF
Usage: $(basename "$0") [OPTIONS]

Options:
  --uninstall       Remove the launcher and desktop entry
  --no-deps         Skip system dependency installation
  --extract         Extract AppImage instead of running via FUSE
  --install-dir D   Override install directory (default: ~/.local/share/whitemane-launcher)
  -h, --help        Show this help
EOF
  exit 0
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --uninstall)    UNINSTALL=true; shift ;;
    --no-deps)      SKIP_DEPS=true; shift ;;
    --extract)      EXTRACT_MODE=true; shift ;;
    --install-dir)
      [[ $# -ge 2 ]] || die "--install-dir requires a path argument."
      INSTALL_DIR="$2"; shift 2 ;;
    -h|--help)      usage ;;
    *) die "Unknown option: $1 (use --help)" ;;
  esac
done

# ---------------------------------------------------------------------------
# Preflight checks
# ---------------------------------------------------------------------------
check_not_root() {
  if [[ "$(id -u)" -eq 0 ]]; then
    warn "Running as root — the launcher will be installed to /root."
    warn "It's recommended to run this as your normal user (sudo is asked when needed)."
    ask_yn "Continue as root? [y/N]" "n" || exit 1
  fi
}

check_arch() {
  local arch
  arch="$(uname -m)"
  if [[ "$arch" != "x86_64" ]]; then
    die "Only x86_64 is supported (detected: $arch)."
  fi
}

check_sudo() {
  if [[ "$SKIP_DEPS" == true ]]; then
    return
  fi
  if [[ "$(id -u)" -eq 0 ]]; then
    return
  fi
  if ! command -v sudo &>/dev/null; then
    die "sudo is required to install system packages. Install sudo or run with --no-deps."
  fi
  info "Checking sudo access (you may be asked for your password) …"
  if ! sudo -v 2>/dev/null; then
    die "Could not obtain sudo privileges. Run with --no-deps to skip package installation."
  fi
}

# ---------------------------------------------------------------------------
# Download helper — curl preferred, wget fallback, with retry
# ---------------------------------------------------------------------------
fetch() {
  local url="$1" dest="$2" attempt max_attempts=3
  if command -v curl &>/dev/null; then
    for attempt in $(seq 1 $max_attempts); do
      if curl -fSL --connect-timeout 15 --progress-bar -o "$dest" "$url"; then
        return 0
      fi
      [[ $attempt -lt $max_attempts ]] && warn "Download failed (attempt $attempt/$max_attempts), retrying …"
    done
  elif command -v wget &>/dev/null; then
    for attempt in $(seq 1 $max_attempts); do
      if wget --timeout=15 --show-progress -qO "$dest" "$url"; then
        return 0
      fi
      [[ $attempt -lt $max_attempts ]] && warn "Download failed (attempt $attempt/$max_attempts), retrying …"
    done
  else
    die "Neither curl nor wget available — install one and retry."
  fi
  die "Failed to download $url after $max_attempts attempts."
}

# Verify downloaded file is not empty or truncated
verify_download() {
  local file="$1" min_bytes="${2:-1000}"
  if [[ ! -f "$file" ]]; then
    die "Download missing: $file"
  fi
  local size
  size="$(wc -c < "$file")"
  if [[ "$size" -lt "$min_bytes" ]]; then
    rm -f "$file"
    die "Downloaded file is too small (${size} bytes) — likely a failed download or server error."
  fi
}

# ---------------------------------------------------------------------------
# OS detection
# ---------------------------------------------------------------------------
DISTRO_ID="unknown"
DISTRO_NAME="Unknown Linux"
DISTRO_FAMILY="unknown"

detect_distro() {
  if [[ ! -f /etc/os-release ]]; then
    warn "Cannot detect distribution (/etc/os-release missing)."
    return
  fi

  # shellcheck source=/dev/null
  . /etc/os-release
  DISTRO_ID="${ID:-unknown}"
  DISTRO_NAME="${PRETTY_NAME:-$ID}"
  local id_like="${ID_LIKE:-}"

  case "$DISTRO_ID" in
    steamos)
      DISTRO_FAMILY="steamos" ;;
    arch|manjaro|endeavouros|garuda|cachyos|artix)
      DISTRO_FAMILY="arch" ;;
    ubuntu|debian|linuxmint|pop|elementary|zorin|kali|raspbian|neon|mx)
      DISTRO_FAMILY="debian" ;;
    fedora|nobara)
      DISTRO_FAMILY="fedora" ;;
    rhel|centos|rocky|alma)
      DISTRO_FAMILY="rhel" ;;
    opensuse*|sles)
      DISTRO_FAMILY="suse" ;;
    void)
      DISTRO_FAMILY="void" ;;
    nixos)
      DISTRO_FAMILY="nix" ;;
    *)
      # Fallback to ID_LIKE
      case "$id_like" in
        *arch*)            DISTRO_FAMILY="arch" ;;
        *ubuntu*|*debian*) DISTRO_FAMILY="debian" ;;
        *fedora*|*rhel*)   DISTRO_FAMILY="fedora" ;;
        *suse*)            DISTRO_FAMILY="suse" ;;
      esac
      ;;
  esac
}

# ---------------------------------------------------------------------------
# SteamOS helpers
# ---------------------------------------------------------------------------
steamos_unlock() {
  if command -v steamos-readonly &>/dev/null; then
    info "Unlocking SteamOS read-only filesystem …"
    sudo steamos-readonly disable
    STEAMOS_WAS_RO=true
  fi
}

steamos_lock() {
  if [[ "$STEAMOS_WAS_RO" == true ]]; then
    info "Re-locking SteamOS read-only filesystem …"
    sudo steamos-readonly enable
    STEAMOS_WAS_RO=false
  fi
}

# ---------------------------------------------------------------------------
# Pacman keyring repair (Arch / SteamOS)
#
# SteamOS ships keys from "Gitlab CI Package Builder" (steeamos.cloud) that
# can expire or become untrusted after long periods without an OS update.
# ---------------------------------------------------------------------------
fix_pacman_keys() {
  info "Initialising pacman keyring …"
  sudo pacman-key --init

  info "Populating trusted keys …"
  sudo pacman-key --populate archlinux
  sudo pacman-key --populate holo 2>/dev/null || true

  # Upgrade keyring packages first so subsequent installs pass PGP checks.
  # Use -Sy (sync DB only) — the full upgrade happens below for regular Arch
  # but is skipped on SteamOS where Valve manages the base system.
  info "Syncing package database …"
  sudo pacman -Sy --noconfirm
  info "Updating keyring packages …"
  sudo pacman -S --noconfirm --needed archlinux-keyring 2>/dev/null || true
  sudo pacman -S --noconfirm --needed holo-keyring      2>/dev/null || true
}

# ---------------------------------------------------------------------------
# Try to install a list of packages; skip any that don't exist in the repos.
# ---------------------------------------------------------------------------
pacman_install_available() {
  local available=() pkg
  for pkg in "$@"; do
    if pacman -Si "$pkg" &>/dev/null; then
      available+=("$pkg")
    else
      warn "Package '$pkg' not found in repos — skipping."
    fi
  done
  if [[ ${#available[@]} -gt 0 ]]; then
    sudo pacman -S --noconfirm --needed "${available[@]}"
  fi
}

# ---------------------------------------------------------------------------
# Apt install with fallback package names.
# Tries each name and installs the first one that exists.
# ---------------------------------------------------------------------------
apt_install_first() {
  local pkg
  for pkg in "$@"; do
    if apt-cache show "$pkg" &>/dev/null 2>&1; then
      sudo apt-get install -y "$pkg"
      return 0
    fi
  done
  warn "None of [$*] found in apt — skipping."
  return 1
}

# ---------------------------------------------------------------------------
# Dependency installation per distro family
# ---------------------------------------------------------------------------
install_deps() {
  info "Installing runtime dependencies for $DISTRO_NAME …"

  case "$DISTRO_FAMILY" in

    steamos)
      steamos_unlock
      fix_pacman_keys
      # SteamOS: install only what's needed, no full -Syu (Valve manages base)
      pacman_install_available webkit2gtk-4.1 gtk3 libayatana-appindicator fuse2
      steamos_lock
      ;;

    arch)
      fix_pacman_keys
      # Regular Arch: full upgrade is the safe path to avoid partial upgrades
      info "Performing full system upgrade …"
      sudo pacman -Syu --noconfirm
      pacman_install_available webkit2gtk-4.1 gtk3 libayatana-appindicator fuse2
      ;;

    debian)
      sudo apt-get update -y
      apt_install_first libwebkit2gtk-4.1-0 libwebkit2gtk-4.1-0t64
      apt_install_first libgtk-3-0 libgtk-3-0t64
      apt_install_first libayatana-appindicator3-1 libappindicator3-1
      apt_install_first libfuse2 libfuse2t64
      ;;

    fedora)
      sudo dnf install -y \
        webkit2gtk4.1 gtk3 libayatana-appindicator-gtk3 fuse-libs \
        2>/dev/null || {
          warn "Some packages may not be available; trying individually …"
          for pkg in webkit2gtk4.1 gtk3 libayatana-appindicator-gtk3 fuse-libs; do
            sudo dnf install -y "$pkg" 2>/dev/null || warn "'$pkg' not found."
          done
        }
      ;;

    rhel)
      sudo dnf install -y epel-release 2>/dev/null || true
      sudo dnf install -y \
        webkit2gtk4.1 gtk3 fuse-libs 2>/dev/null || true
      ;;

    suse)
      sudo zypper --non-interactive install \
        libwebkit2gtk-4_1-0 gtk3 \
        typelib-1_0-AyatanaAppIndicator3-0_1 libfuse2 \
        2>/dev/null || {
          warn "Some packages may not be available — trying individually."
          for pkg in libwebkit2gtk-4_1-0 gtk3 typelib-1_0-AyatanaAppIndicator3-0_1 libfuse2; do
            sudo zypper --non-interactive install "$pkg" 2>/dev/null || warn "'$pkg' not found."
          done
        }
      ;;

    void)
      sudo xbps-install -Sy webkit2gtk libayatana-appindicator fuse 2>/dev/null || true
      ;;

    nix)
      warn "NixOS detected — install dependencies via configuration.nix or nix-shell."
      warn "Required: webkitgtk_4_1, gtk3, libayatana-appindicator, fuse"
      ;;

    *)
      warn "Unsupported distro: $DISTRO_NAME"
      echo ""
      echo "Please install the following packages manually:"
      echo "  - webkit2gtk 4.1  (provides WebKit for Tauri)"
      echo "  - gtk3            (GTK toolkit)"
      echo "  - libayatana-appindicator (tray icon support)"
      echo "  - fuse2/libfuse2  (AppImage support)"
      echo ""
      ask_yn "Continue anyway? [y/N]" "n" || exit 1
      ;;
  esac
}

# ---------------------------------------------------------------------------
# FUSE availability check
# ---------------------------------------------------------------------------
check_fuse() {
  if [[ "$EXTRACT_MODE" == true ]]; then
    return 1
  fi
  if [[ -e /dev/fuse ]] || modinfo fuse &>/dev/null 2>&1; then
    return 0
  fi
  warn "FUSE not available — will extract AppImage instead."
  return 1
}

# ---------------------------------------------------------------------------
# Install: download AppImage, create .desktop, PATH link
# ---------------------------------------------------------------------------
do_install() {
  mkdir -p "$INSTALL_DIR" "$BIN_DIR" "$DESKTOP_DIR" "$ICON_DIR"

  local appimage_path="$INSTALL_DIR/$APPIMAGE_NAME"

  # Track for cleanup on failure
  CLEANUP_FILES+=("$appimage_path")

  info "Downloading $APPIMAGE_NAME …"
  fetch "$CDN_BASE/$APPIMAGE_NAME" "$appimage_path"
  verify_download "$appimage_path" 100000
  chmod +x "$appimage_path"

  local file_size
  file_size="$(wc -c < "$appimage_path")"
  ok "Downloaded $(( file_size / 1048576 )) MB → $appimage_path"

  # Decide: FUSE (run AppImage directly) or extract
  local exec_path
  if check_fuse; then
    exec_path="$appimage_path"
    info "Using AppImage directly (FUSE available)."
  else
    EXTRACT_MODE=true
    info "Extracting AppImage (no FUSE) …"
    local extract_dir="$INSTALL_DIR/squashfs-root"
    rm -rf "$extract_dir"

    # Extract in a subshell to avoid changing the working directory
    (cd "$INSTALL_DIR" && "./$APPIMAGE_NAME" --appimage-extract >/dev/null 2>&1) \
      || die "Failed to extract AppImage. The file may be corrupted — delete $appimage_path and retry."

    local main_bin=""
    if [[ -f "$extract_dir/AppRun" ]]; then
      main_bin="$extract_dir/AppRun"
    elif [[ -d "$extract_dir/usr/bin" ]]; then
      main_bin="$(find "$extract_dir/usr/bin" -maxdepth 1 -type f -executable 2>/dev/null | head -1)"
    fi
    if [[ -z "$main_bin" ]]; then
      die "Could not find executable inside extracted AppImage."
    fi
    chmod +x "$main_bin"
    exec_path="$main_bin"
    ok "Extracted to $extract_dir"
  fi

  # Extract icon
  local icon_path="$ICON_DIR/whitemane-launcher.png"
  local search_root=""

  if [[ "$EXTRACT_MODE" == true && -d "$INSTALL_DIR/squashfs-root" ]]; then
    search_root="$INSTALL_DIR/squashfs-root"
  else
    local tmpdir
    tmpdir="$(mktemp -d)"
    (cd "$tmpdir" && "$appimage_path" --appimage-extract "*.desktop" >/dev/null 2>&1 || true)
    (cd "$tmpdir" && "$appimage_path" --appimage-extract "*.png" >/dev/null 2>&1 || true)
    (cd "$tmpdir" && "$appimage_path" --appimage-extract "*.svg" >/dev/null 2>&1 || true)
    (cd "$tmpdir" && "$appimage_path" --appimage-extract "usr/share/icons/*" >/dev/null 2>&1 || true)
    search_root="$tmpdir/squashfs-root"
  fi

  if [[ -n "$search_root" && -d "$search_root" ]]; then
    # Prefer 128x128 or 256x256 icons, fall back to any .png
    local found_icon=""
    for pattern in "256x256" "128x128" "64x64"; do
      found_icon="$(find "$search_root" -path "*${pattern}*" -name "*.png" 2>/dev/null | head -1)"
      [[ -n "$found_icon" ]] && break
    done
    if [[ -z "$found_icon" ]]; then
      found_icon="$(find "$search_root" -name "*.png" -not -name ".*" 2>/dev/null | head -1)"
    fi
    if [[ -n "$found_icon" ]]; then
      cp "$found_icon" "$icon_path" 2>/dev/null || true
    fi
  fi

  [[ -n "${tmpdir:-}" && -d "${tmpdir:-}" ]] && rm -rf "$tmpdir"

  # Symlink into PATH
  ln -sf "$exec_path" "$BIN_LINK"

  # Desktop entry
  cat > "$DESKTOP_FILE" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Whitemane Launcher
Comment=Whitemane game launcher
Exec=$exec_path %U
Icon=$icon_path
Terminal=false
Categories=Game;
StartupWMClass=whitemane-launcher
DESKTOP

  command -v update-desktop-database &>/dev/null \
    && update-desktop-database "$DESKTOP_DIR" 2>/dev/null || true

  # Warn if ~/.local/bin is not in PATH
  case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *)
      warn "$BIN_DIR is not in your PATH."
      warn "Add to your shell profile (~/.bashrc or ~/.zshrc):"
      echo ""
      echo "  export PATH=\"\$HOME/.local/bin:\$PATH\""
      echo ""
      ;;
  esac

  # Clear cleanup list on success
  CLEANUP_FILES=()
}

# ---------------------------------------------------------------------------
# Uninstall
# ---------------------------------------------------------------------------
do_uninstall() {
  info "Removing Whitemane Launcher …"
  rm -rf "$INSTALL_DIR"
  rm -f "$BIN_LINK"
  rm -f "$DESKTOP_FILE"
  rm -f "$ICON_DIR/whitemane-launcher.png"
  command -v update-desktop-database &>/dev/null \
    && update-desktop-database "$DESKTOP_DIR" 2>/dev/null || true
  ok "Uninstalled."
}

# ---------------------------------------------------------------------------
# macOS install / uninstall
# ---------------------------------------------------------------------------
do_install_macos() {
  local arch
  arch="$(uname -m)"
  if [[ "$arch" != "arm64" ]]; then
    die "Only Apple Silicon (arm64) is supported on macOS (detected: $arch)."
  fi

  local dmg_path="${TMPDIR:-/tmp}/$DMG_NAME"
  CLEANUP_FILES+=("$dmg_path")

  info "Downloading $DMG_NAME …"
  fetch "$CDN_BASE/$DMG_NAME" "$dmg_path"
  verify_download "$dmg_path" 1000000

  local file_size
  file_size="$(wc -c < "$dmg_path")"
  ok "Downloaded $(( file_size / 1048576 )) MB → $dmg_path"

  # Strip quarantine on the DMG itself so hdiutil doesn't refuse it on
  # tightened Gatekeeper setups.
  xattr -d com.apple.quarantine "$dmg_path" 2>/dev/null || true

  info "Mounting DMG …"
  # NOTE: deliberately not passing -quiet — on recent macOS hdiutil it
  # silences stdout entirely, leaving nothing to parse for the mount
  # point. Parse "/Volumes/<anything-to-end-of-line>" to survive volume
  # names with spaces (e.g. "Whitemane Beta").
  local mount_point
  mount_point="$(hdiutil attach -nobrowse -noautoopen "$dmg_path" 2>/dev/null \
    | grep -oE '/Volumes/.*$' | tail -1)"
  if [[ -z "$mount_point" || ! -d "$mount_point" ]]; then
    die "Could not determine DMG mount point."
  fi
  info "Mounted at $mount_point"

  local source_app
  source_app="$(find "$mount_point" -maxdepth 1 -type d -name '*.app' 2>/dev/null | head -1)"
  if [[ -z "$source_app" ]]; then
    hdiutil detach -quiet "$mount_point" 2>/dev/null || true
    die "No .app bundle found inside DMG."
  fi

  local app_basename dest_app
  app_basename="$(basename "$source_app")"
  dest_app="/Applications/$app_basename"

  if [[ -d "$dest_app" ]]; then
    info "Replacing existing $dest_app …"
    rm -rf "$dest_app" 2>/dev/null || sudo rm -rf "$dest_app"
  fi

  info "Copying $app_basename to /Applications …"
  cp -R "$source_app" "/Applications/" 2>/dev/null || sudo cp -R "$source_app" "/Applications/"

  info "Unmounting DMG …"
  hdiutil detach -quiet "$mount_point" 2>/dev/null \
    || hdiutil detach -force "$mount_point" >/dev/null 2>&1 \
    || warn "Failed to unmount $mount_point (you may detach it manually)."

  info "Clearing quarantine + extended attributes on $app_basename …"
  xattr -cr "$dest_app" 2>/dev/null || sudo xattr -cr "$dest_app"

  rm -f "$dmg_path"
  CLEANUP_FILES=()

  ok "Installed: $dest_app"
  info "Launch it from Spotlight, Launchpad, or:"
  echo ""
  echo "  open \"$dest_app\""
  echo ""
}

do_uninstall_macos() {
  info "Removing Whitemane app bundle(s) from /Applications …"
  local removed=false app
  # Match both stable ("Whitemane.app") and any flavour-prefixed variant
  # that this script might have installed.
  for app in "/Applications/Whitemane.app" "/Applications/Whitemane "*.app; do
    if [[ -d "$app" ]]; then
      info "Removing $app …"
      rm -rf "$app" 2>/dev/null || sudo rm -rf "$app"
      removed=true
    fi
  done
  if [[ "$removed" == true ]]; then
    ok "Uninstalled."
  else
    warn "No Whitemane installation found in /Applications."
  fi
}

# ---------------------------------------------------------------------------
# OS detection (top-level branch — Linux vs macOS)
# ---------------------------------------------------------------------------
OS_KIND="linux"

detect_os() {
  case "$(uname -s)" in
    Darwin) OS_KIND="macos" ;;
    Linux)  OS_KIND="linux" ;;
    *)      die "Unsupported OS: $(uname -s) (only Linux and macOS are supported)." ;;
  esac
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
main() {
  echo ""
  printf "${BOLD}  Whitemane Launcher — Setup${NC}\n"
  echo "  ─────────────────────────────────"
  echo ""

  detect_os

  if [[ "$OS_KIND" == "macos" ]]; then
    if [[ "$UNINSTALL" == true ]]; then
      do_uninstall_macos
      exit 0
    fi
    check_not_root
    do_install_macos
    echo ""
    echo "  ─────────────────────────────────"
    ok "Setup complete!"
    echo ""
    exit 0
  fi

  if [[ "$UNINSTALL" == true ]]; then
    do_uninstall
    exit 0
  fi

  check_not_root
  check_arch
  detect_distro
  info "Detected: $DISTRO_NAME (family: $DISTRO_FAMILY)"

  if [[ "$SKIP_DEPS" == false ]]; then
    check_sudo
    install_deps
    ok "Dependencies ready."
  else
    info "Skipping dependency installation (--no-deps)."
  fi

  echo ""
  do_install

  echo ""
  echo "  ─────────────────────────────────"
  ok "Setup complete!"
  echo ""
  info "Launch: ${BOLD}whitemane-launcher${NC}"
  info "Or find '${BOLD}Whitemane Launcher${NC}' in your app menu."

  if [[ "$DISTRO_FAMILY" == "steamos" ]]; then
    echo ""
    warn "SteamOS: System updates may remove installed packages."
    warn "If the launcher stops working after an update, re-run this script."
  fi

  echo ""
}

main "$@"
