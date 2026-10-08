#!/usr/bin/env bash
# Build MieryCommander.app on macOS and install it into /Applications.
#
#   ./install-macos.sh               build + install into /Applications (or ~/Applications)
#   ./install-macos.sh --dmg         additionally create a .dmg to share
#   ./install-macos.sh --universal   one app for Intel and Apple Silicon
#   ./install-macos.sh --no-install  only build (app/dmg end up in target/macos)
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
cd "$here"

want_dmg=0
universal=0
install=1
for arg in "$@"; do
    case "$arg" in
        --dmg) want_dmg=1 ;;
        --universal) universal=1 ;;
        --no-install) install=0 ;;
        -h|--help) sed -n '2,8p' "$0"; exit 0 ;;
        *) echo "Unbekannte Option / unknown option: $arg"; exit 1 ;;
    esac
done

if [ "$(uname)" != "Darwin" ]; then
    echo "Dieses Skript ist für macOS. / This script is for macOS (Linux: ./install-desktop.sh)."
    exit 1
fi

# --- Rust --------------------------------------------------------------------
if ! command -v cargo >/dev/null 2>&1; then
    [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
fi
if ! command -v cargo >/dev/null 2>&1; then
    echo "Rust fehlt. / Rust is missing."
    if command -v brew >/dev/null 2>&1; then
        read -r -p "Mit Homebrew installieren? / Install with Homebrew? [J/n] " answer
        case "${answer:-j}" in [nN]*) exit 1 ;; esac
        brew install rust
    else
        echo "Bitte Rust installieren: https://rustup.rs  /  Please install Rust: https://rustup.rs"
        exit 1
    fi
fi

# Xcode command line tools (C/C++ compiler for the RAR support).
if ! xcode-select -p >/dev/null 2>&1; then
    echo "Die Xcode Command Line Tools werden benötigt. / Xcode command line tools are required."
    xcode-select --install || true
    echo "Nach der Installation das Skript erneut starten. / Run this script again afterwards."
    exit 1
fi

version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
out="$here/target/macos"
app="$out/MieryCommander.app"

# --- Build -------------------------------------------------------------------
if [ "$universal" = 1 ]; then
    if command -v rustup >/dev/null 2>&1; then
        rustup target add aarch64-apple-darwin x86_64-apple-darwin
    fi
    cargo build --release --target aarch64-apple-darwin
    cargo build --release --target x86_64-apple-darwin
    mkdir -p "$out"
    lipo -create -output "$out/miery_commander" \
        target/aarch64-apple-darwin/release/miery_commander \
        target/x86_64-apple-darwin/release/miery_commander
    binary="$out/miery_commander"
else
    cargo build --release
    binary="target/release/miery_commander"
fi

# --- App bundle --------------------------------------------------------------
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$binary" "$app/Contents/MacOS/miery_commander"

# Icon: all sizes from the 1024 px PNG with macOS' own tools.
iconset="$out/AppIcon.iconset"
rm -rf "$iconset"
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
    sips -z "$size" "$size" assets/icon-1024.png --out "$iconset/icon_${size}x${size}.png" >/dev/null
    double=$((size * 2))
    sips -z "$double" "$double" assets/icon-1024.png --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/AppIcon.icns"
rm -rf "$iconset"

cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>MieryCommander</string>
    <key>CFBundleDisplayName</key><string>MieryCommander</string>
    <key>CFBundleIdentifier</key><string>io.github.evil8810.miery-commander</string>
    <key>CFBundleExecutable</key><string>miery_commander</string>
    <key>CFBundleIconFile</key><string>AppIcon</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>$version</string>
    <key>CFBundleDevelopmentRegion</key><string>en</string>
    <key>CFBundleLocalizations</key><array><string>en</string><string>de</string></array>
    <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSHumanReadableCopyright</key><string>MIT License</string>
</dict>
</plist>
EOF

# Ad-hoc signature: required on Apple Silicon, no Apple developer account needed.
codesign --force --deep --sign - "$app"
echo "App gebaut / app built: $app"

# --- DMG ---------------------------------------------------------------------
if [ "$want_dmg" = 1 ]; then
    arch_name="$([ "$universal" = 1 ] && echo universal || uname -m)"
    dmg="$out/MieryCommander-$version-macos-$arch_name.dmg"
    staging="$out/dmg"
    rm -rf "$staging" "$dmg"
    mkdir -p "$staging"
    cp -R "$app" "$staging/"
    ln -s /Applications "$staging/Applications"
    hdiutil create -volname "MieryCommander" -srcfolder "$staging" -ov -format UDZO "$dmg" >/dev/null
    rm -rf "$staging"
    echo "DMG: $dmg"
fi

# --- Install -----------------------------------------------------------------
if [ "$install" = 1 ]; then
    dest="/Applications"
    [ -w "$dest" ] || dest="$HOME/Applications"
    mkdir -p "$dest"
    rm -rf "$dest/MieryCommander.app"
    cp -R "$app" "$dest/"
    echo "Installiert / installed: $dest/MieryCommander.app"
    echo "Starten über Launchpad oder Spotlight („MieryCommander“). / Start it from Launchpad or Spotlight."
fi
