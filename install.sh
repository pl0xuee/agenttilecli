#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"

BIN_NAME="agenttilecli"
APP_ID="dev.agenttilecli.AgentTileCli"

if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo (Rust) is not installed. Install it from https://rustup.rs and try again." >&2
    exit 1
fi

PKG_HINT="  Arch/CachyOS:   sudo pacman -S gtk4 vte4
  Fedora:         sudo dnf install gtk4-devel vte291-gtk4-devel
  Debian/Ubuntu:  sudo apt install libgtk-4-dev libvte-2.91-gtk4-dev"

if ! command -v pkg-config >/dev/null 2>&1; then
    echo "error: pkg-config is not installed." >&2
    echo "       Arch/CachyOS:   sudo pacman -S pkgconf" >&2
    echo "       Fedora:         sudo dnf install pkg-config" >&2
    echo "       Debian/Ubuntu:  sudo apt install pkg-config" >&2
    exit 1
fi

if ! pkg-config --atleast-version=4.12 gtk4 2>/dev/null; then
    echo "error: GTK4 >= 4.12 development files not found (pkg-config gtk4)." >&2
    echo "       Install your distro's GTK4 dev package and try again, e.g.:" >&2
    echo "$PKG_HINT" >&2
    exit 1
fi

if ! pkg-config --atleast-version=0.65 vte-2.91-gtk4 2>/dev/null; then
    echo "error: VTE4 >= 0.65 (the GTK4-flavored VTE terminal widget) not found (pkg-config vte-2.91-gtk4)." >&2
    echo "       Install your distro's GTK4-flavored VTE dev package and try again, e.g.:" >&2
    echo "$PKG_HINT" >&2
    echo "       Note: this package is fairly recent upstream, so older distro releases" >&2
    echo "       (e.g. Debian 12 bookworm) may not carry it at all." >&2
    exit 1
fi

echo "Building release binary..."
cargo build --release

BIN_DIR="$HOME/.local/bin"
APPS_DIR="$HOME/.local/share/applications"
ICON_DIR="$HOME/.local/share/icons/hicolor/scalable/apps"
mkdir -p "$BIN_DIR" "$APPS_DIR" "$ICON_DIR"

install -m 755 "target/release/$BIN_NAME" "$BIN_DIR/$BIN_NAME"
install -m 644 assets/icon.svg "$ICON_DIR/$BIN_NAME.svg"

# The desktop file's id must match the GTK application id (APP_ID) so that
# Wayland compositors (KWin, GNOME Shell) can resolve a persistent taskbar
# icon by app_id. A mismatched id means the icon only shows while the
# window is actually open and vanishes once it's closed.
rm -f "$APPS_DIR/$BIN_NAME.desktop"
cat > "$APPS_DIR/$APP_ID.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=AgentTileCLI
Comment=Dynamic tiling window manager for AI CLI sessions
Exec=$BIN_DIR/$BIN_NAME
Icon=$BIN_NAME
Terminal=false
Categories=Development;TerminalEmulator;
StartupNotify=true
StartupWMClass=$APP_ID
EOF

if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    # -t forces a rebuild even though ~/.local/share/icons/hicolor has no
    # index.theme of its own (it's just a user override tree, not a full
    # theme). Without -t, gtk-update-icon-cache silently no-ops here, which
    # leaves a stale icon-theme.cache around — and once that cache exists,
    # the icon theme spec requires consumers (KDE's KIconLoader, used for
    # pinned/closed taskbar entries and KRunner search) to trust it and
    # ignore icons that aren't listed, even if the file is on disk.
    gtk-update-icon-cache -q -f -t "$HOME/.local/share/icons/hicolor" >/dev/null 2>&1 || true
fi
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$APPS_DIR" >/dev/null 2>&1 || true
fi
# KDE Plasma keeps its own application menu index separate from the above;
# without this, a freshly installed .desktop file may not show up in the
# menu (or fail to launch via a bare command name) until next login.
if command -v kbuildsycoca6 >/dev/null 2>&1; then
    kbuildsycoca6 >/dev/null 2>&1 || true
elif command -v kbuildsycoca5 >/dev/null 2>&1; then
    kbuildsycoca5 >/dev/null 2>&1 || true
fi

echo "Installed to $BIN_DIR/$BIN_NAME"
case ":$PATH:" in
    *":$BIN_DIR:"*) echo "Run it with: $BIN_NAME" ;;
    *) echo "Note: $BIN_DIR is not on your PATH. Add it to your shell profile, or run $BIN_DIR/$BIN_NAME directly." ;;
esac
