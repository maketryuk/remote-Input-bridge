#!/bin/bash
# Builds mac-receiver/.build/release/RemoteInputBridge into a menu-bar .app bundle.
#
# The bundle is required for two reasons: LSUIElement (no Dock icon) lives in Info.plist, and
# macOS grants Accessibility permission to a bundle identity rather than a loose binary.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RECEIVER="$ROOT/mac-receiver"
CONFIGURATION="${1:-release}"
APP="$RECEIVER/build/RemoteInputBridge.app"
VERSION="0.1.0"

echo "==> building ($CONFIGURATION)"
cd "$RECEIVER"
swift build -c "$CONFIGURATION"
BINARY="$(swift build -c "$CONFIGURATION" --show-bin-path)/RemoteInputBridge"

echo "==> assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BINARY" "$APP/Contents/MacOS/RemoteInputBridge"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>            <string>Remote Input Bridge</string>
    <key>CFBundleDisplayName</key>     <string>Remote Input Bridge</string>
    <key>CFBundleIdentifier</key>      <string>studio.lince.remoteinputbridge</string>
    <key>CFBundleExecutable</key>      <string>RemoteInputBridge</string>
    <key>CFBundlePackageType</key>     <string>APPL</string>
    <key>CFBundleShortVersionString</key> <string>$VERSION</string>
    <key>CFBundleVersion</key>         <string>$VERSION</string>
    <key>LSMinimumSystemVersion</key>  <string>13.0</string>
    <!-- Menu bar only: no Dock icon, no main window. -->
    <key>LSUIElement</key>             <true/>
    <key>NSHumanReadableCopyright</key><string>Local network input receiver</string>
</dict>
</plist>
PLIST

# Ad-hoc signature: enough for a local build. Note that an ad-hoc identity changes on every
# rebuild, so macOS may ask for Accessibility permission again after a rebuild. Sign with a real
# Developer ID to make the grant stick.
codesign --force --sign - --timestamp=none "$APP" >/dev/null 2>&1 || \
    echo "    (codesign failed; the app still runs, permission prompts may repeat)"

echo "==> done: $APP"
echo "    run:  open '$APP'"
echo "    logs: '$APP/Contents/MacOS/RemoteInputBridge'   (run in a terminal to see them)"
