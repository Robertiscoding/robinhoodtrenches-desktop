#!/bin/sh
# Wrap the built .app in a compressed DMG with an /Applications drop target.
# Tauri's own dmg step scripts Finder via AppleScript, which needs GUI
# automation rights; hdiutil needs nothing and is what ships here.
set -eu
cd "$(dirname "$0")/.."
APP=src-tauri/target/release/bundle/macos/robinhoodtrenches.app
OUT=src-tauri/target/release/bundle/dmg
VERSION=$(sed -n 's/.*"version": "\([^"]*\)".*/\1/p' src-tauri/tauri.conf.json | head -1)
ARCH=$(uname -m)
[ -d "$APP" ] || { echo "build the app first: npm run build" >&2; exit 1; }
STAGE=$(mktemp -d)
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
mkdir -p "$OUT"
DMG="$OUT/robinhoodtrenches_${VERSION}_${ARCH}.dmg"
rm -f "$DMG"
hdiutil create -volname robinhoodtrenches -srcfolder "$STAGE" -ov -format UDZO -quiet "$DMG"
rm -rf "$STAGE"
ls -lh "$DMG"
