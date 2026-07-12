#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"

if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo (Rust) is not installed. Install it from https://rustup.rs and try again." >&2
    exit 1
fi

if ! pkg-config --exists gtk4 2>/dev/null; then
    echo "error: GTK4 development files not found (pkg-config gtk4)." >&2
    echo "       Install your distro's gtk4 package (dev headers) and try again." >&2
    exit 1
fi

if ! pkg-config --exists vte-2.91-gtk4 2>/dev/null; then
    echo "error: VTE4 (the GTK4-flavored VTE terminal widget) not found (pkg-config vte-2.91-gtk4)." >&2
    echo "       Install your distro's vte4 package (sometimes named vte3-gtk4) and try again." >&2
    exit 1
fi

echo "Building release binary..."
cargo build --release

BIN_DIR="$HOME/.local/bin"
APPS_DIR="$HOME/.local/share/applications"
mkdir -p "$BIN_DIR" "$APPS_DIR"

install -m 755 target/release/aitile "$BIN_DIR/aitile"

cat > "$APPS_DIR/aitile.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=aitile
Comment=Dynamic tiling window manager for AI CLI sessions
Exec=$BIN_DIR/aitile
Icon=utilities-terminal
Terminal=false
Categories=Development;TerminalEmulator;
StartupNotify=true
EOF

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

echo "Installed to $BIN_DIR/aitile"
case ":$PATH:" in
    *":$BIN_DIR:"*) echo "Run it with: aitile" ;;
    *) echo "Note: $BIN_DIR is not on your PATH. Add it to your shell profile, or run $BIN_DIR/aitile directly." ;;
esac
