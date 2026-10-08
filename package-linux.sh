#!/usr/bin/env bash
# Build the Linux downloads into target/linux:
#   MieryCommander-<version>-x86_64.AppImage   one file, runs on most distributions
#   MieryCommander-<version>-linux-x86_64.tar.gz  binary + ./install-desktop.sh
#
#   ./package-linux.sh            build both
#   ./package-linux.sh --no-build use the existing target/release binary
# appimagetool is taken from $APPIMAGETOOL, the PATH or downloaded once.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
cd "$here"

build=1
for arg in "$@"; do
    case "$arg" in
        --no-build) build=0 ;;
        -h|--help) sed -n '2,8p' "$0"; exit 0 ;;
        *) echo "Unbekannte Option / unknown option: $arg"; exit 1 ;;
    esac
done

version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
arch="$(uname -m)"
out="$here/target/linux"
[ "$build" = 1 ] && cargo build --release
bin="$here/target/release/miery_commander"
rm -rf "$out"
mkdir -p "$out"

# --- tar.gz ------------------------------------------------------------------
pkg="MieryCommander-$version-linux-$arch"
mkdir -p "$out/$pkg/assets"
cp "$bin" install-desktop.sh LICENSE README.md "$out/$pkg/"
cp assets/icon.png "$out/$pkg/assets/"
tar -C "$out" -czf "$out/$pkg.tar.gz" "$pkg"
rm -rf "$out/$pkg"
echo "tar.gz: $out/$pkg.tar.gz"

# --- AppImage ----------------------------------------------------------------
appdir="$out/AppDir"
mkdir -p "$appdir/usr/bin" "$appdir/usr/share/applications" "$appdir/usr/share/icons/hicolor/512x512/apps"
cp "$bin" "$appdir/usr/bin/miery_commander"
cp assets/icon.png "$appdir/usr/share/icons/hicolor/512x512/apps/miery-commander.png"
cp assets/icon.png "$appdir/miery-commander.png"
cat > "$appdir/usr/share/applications/miery-commander.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=MieryCommander
GenericName=File manager
GenericName[de]=Dateimanager
Exec=miery_commander %f
Icon=miery-commander
Terminal=false
Categories=System;FileTools;FileManager;
Keywords=files;file manager;commander;FTP;SFTP;SMB;
Keywords[de]=Dateien;Dateimanager;Commander;FTP;SFTP;SMB;
StartupWMClass=miery-commander
StartupNotify=true
X-AppImage-Version=$version
DESKTOP
cp "$appdir/usr/share/applications/miery-commander.desktop" "$appdir/"
ln -s usr/bin/miery_commander "$appdir/AppRun"

tool="${APPIMAGETOOL:-$(command -v appimagetool || true)}"
if [ -z "$tool" ]; then
    tool="$here/target/appimagetool-$arch.AppImage"
    if [ ! -x "$tool" ]; then
        echo "Lade appimagetool / downloading appimagetool…"
        curl -fsSL -o "$tool" "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-$arch.AppImage"
        chmod +x "$tool"
    fi
fi
image="$out/MieryCommander-$version-$arch.AppImage"
# Extract-and-run: works without FUSE (e.g. in CI containers).
APPIMAGE_EXTRACT_AND_RUN=1 ARCH="$arch" "$tool" --no-appstream "$appdir" "$image"
rm -rf "$appdir"
echo "AppImage: $image"
