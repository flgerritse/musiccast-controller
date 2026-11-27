#!/usr/bin/env bash
# Build a release binary, wrap it in a signed .app bundle and package it as a DMG.
#
#   ./make_mac_dmg.sh            build dist/Controller-for-MusicCast-<version>.dmg
#   ./make_mac_dmg.sh --clean    same, then run `cargo clean` afterwards
#
# The version comes from Cargo.toml. The app is ad-hoc signed (no Developer ID),
# so on another Mac it must be opened once via right-click → Open.
set -euo pipefail
cd "$(dirname "$0")"

NAME="Controller for MusicCast"
BIN="network-speaker-control"
VERSION=$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)
APP="dist/$NAME.app"
DMG="dist/Controller-for-MusicCast-$VERSION.dmg"

echo "==> Building release $VERSION"
cargo build --release

echo "==> Assembling $APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" dist/AppIcon.iconset
cp "target/release/$BIN" "$APP/Contents/MacOS/$BIN"

for s in 16 32 128 256 512; do
  sips -z $s $s assets/app-icon.png --out "dist/AppIcon.iconset/icon_${s}x${s}.png" >/dev/null
  d=$((s * 2))
  sips -z $d $d assets/app-icon.png --out "dist/AppIcon.iconset/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns dist/AppIcon.iconset -o "$APP/Contents/Resources/AppIcon.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>$NAME</string>
  <key>CFBundleDisplayName</key><string>$NAME</string>
  <key>CFBundleExecutable</key><string>$BIN</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundleIdentifier</key><string>local.controller.musiccast</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSLocalNetworkUsageDescription</key><string>Finds and controls MusicCast speakers on your home network.</string>
</dict>
</plist>
PLIST

echo "==> Signing (ad-hoc)"
xattr -cr "$APP"
codesign --force --sign - "$APP"
codesign --verify "$APP"

echo "==> Creating $DMG"
SRC=$(mktemp -d)
trap 'rm -rf "$SRC"' EXIT
ditto "$APP" "$SRC/$NAME.app"
ln -s /Applications "$SRC/Applications"
hdiutil create -volname "$NAME" -srcfolder "$SRC" -ov -format UDZO "$DMG" >/dev/null 2>&1  # silences hdiutil deprecation warning
hdiutil verify "$DMG" >/dev/null

if [[ "${1:-}" == "--clean" ]]; then
  echo "==> cargo clean"
  cargo clean
fi

echo "Done: $DMG ($(du -h "$DMG" | cut -f1))"
