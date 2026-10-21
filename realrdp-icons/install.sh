#!/bin/sh
# Installeert de RealRDP-iconen voor de huidige gebruiker
set -e
cd "$(dirname "$0")"
DEST="${XDG_DATA_HOME:-$HOME/.local/share}/icons"
mkdir -p "$DEST" && cp -r hicolor "$DEST/"
mkdir -p "${XDG_DATA_HOME:-$HOME/.local/share}/applications"
cp realrdp.desktop "${XDG_DATA_HOME:-$HOME/.local/share}/applications/"
# Het programma zelf, onder de naam die Exec= en StartupWMClass= verwachten
cargo build --release --manifest-path ../Cargo.toml
mkdir -p "$HOME/.local/bin" && cp ../target/release/RealRDP "$HOME/.local/bin/realrdp"
update-desktop-database "${XDG_DATA_HOME:-$HOME/.local/share}/applications" 2>/dev/null || true
xdg-mime install --novendor realrdp-mime.xml 2>/dev/null || true
xdg-mime default realrdp.desktop application/x-rdp 2>/dev/null || true
gtk-update-icon-cache -f -t "$DEST/hicolor" 2>/dev/null || true
echo "RealRDP-iconen geïnstalleerd."
