#!/usr/bin/env bash
# Installs MieryCommander into the desktop menu of the current user (Linux):
# start menu entry + floppy icon, so window and taskbar show the right icon.
# No root needed – everything goes to ~/.local/share.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
bin="$here/target/release/miery_commander"
data="${XDG_DATA_HOME:-$HOME/.local/share}"

if [ ! -x "$bin" ]; then
    echo "Programm noch nicht gebaut – baue jetzt: cargo build --release"
    (cd "$here" && cargo build --release)
fi

# Icon in several sizes (the app id "miery-commander" must match the .desktop name).
for size in 512 256 128 64 48 32; do
    dir="$data/icons/hicolor/${size}x${size}/apps"
    mkdir -p "$dir"
    if [ "$size" = 512 ] || ! command -v magick >/dev/null; then
        cp "$here/assets/icon.png" "$dir/miery-commander.png"
    else
        magick "$here/assets/icon.png" -resize "${size}x${size}" "$dir/miery-commander.png"
    fi
done

mkdir -p "$data/applications"
cat > "$data/applications/miery-commander.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=MieryCommander
GenericName=File manager
GenericName[de]=Dateimanager
Exec="$bin" %f
Icon=miery-commander
Terminal=false
Categories=System;FileTools;FileManager;
Keywords=files;file manager;commander;FTP;SFTP;SMB;
Keywords[de]=Dateien;Dateimanager;Commander;FTP;SFTP;SMB;
StartupWMClass=miery-commander
StartupNotify=true
EOF

# Refresh menus and icon caches (whatever exists on this desktop).
command -v update-desktop-database >/dev/null && update-desktop-database "$data/applications" || true
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q -t "$data/icons/hicolor" || true
command -v kbuildsycoca6 >/dev/null && kbuildsycoca6 --noincremental >/dev/null 2>&1 || true

echo "Installiert: $data/applications/miery-commander.desktop"
