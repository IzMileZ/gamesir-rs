#!/usr/bin/env bash
# Instala o actualiza gamesir-rs en ~/.local: binario, ícono y lanzador .desktop.
# Tras cambiar el código y recompilar, basta con volver a ejecutar este script
# (o `./install.sh --no-build` si el binario de target/release ya es el bueno).
set -euo pipefail
cd "$(dirname "$0")"

if [ "${1:-}" != "--no-build" ]; then
    cargo build --release
fi

BIN_DIR="$HOME/.local/bin"
ICON_DIR="$HOME/.local/share/icons/hicolor/scalable/apps"
APP_DIR="$HOME/.local/share/applications"
mkdir -p "$BIN_DIR" "$ICON_DIR" "$APP_DIR"

cp target/release/gamesir-rs "$BIN_DIR/gamesir-rs"
cp assets/gamesir-rs.svg "$ICON_DIR/gamesir-rs.svg"
sed "s|__BINDIR__|$BIN_DIR|g" assets/gamesir-rs.desktop >"$APP_DIR/gamesir-rs.desktop"

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$APP_DIR"
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -qf "$HOME/.local/share/icons/hicolor" 2>/dev/null || true
fi

echo "[ok] binario : $BIN_DIR/gamesir-rs"
echo "[ok] ícono   : $ICON_DIR/gamesir-rs.svg"
echo "[ok] lanzador: $APP_DIR/gamesir-rs.desktop (GameSir Tegenaria Mapper)"
