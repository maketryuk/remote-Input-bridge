#!/bin/bash
# Builds the macOS receiver into a menu-bar .app bundle, and optionally installs it.
#
# The bundle is required for two reasons: LSUIElement (no Dock icon) lives in Info.plist, and
# macOS grants Accessibility permission to a bundle identity rather than a loose binary.
#
#   ./scripts/build-mac-app.sh                 build only
#   ./scripts/build-mac-app.sh --install       build and install to /Applications
#   ./scripts/build-mac-app.sh --install --run build, install and launch
#   ./scripts/build-mac-app.sh --debug         build the debug configuration
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RECEIVER="$ROOT/mac-receiver"
CONFIGURATION=release
INSTALL=0
RUN=0
VERSION=0.1.0
DESTINATION=/Applications

for argument in "$@"; do
    case "$argument" in
        --install) INSTALL=1 ;;
        --run) RUN=1 ;;
        --debug) CONFIGURATION=debug ;;
        --release) CONFIGURATION=release ;;
        -h|--help) sed -n '2,12p' "${BASH_SOURCE[0]}"; exit 0 ;;
        *) echo "unknown argument: $argument" >&2; exit 2 ;;
    esac
done

APP="$RECEIVER/build/RemoteInputBridge.app"
ICON="$RECEIVER/build/AppIcon.icns"

echo "==> building ($CONFIGURATION)"
cd "$RECEIVER"
swift build -c "$CONFIGURATION"
BINARY="$(swift build -c "$CONFIGURATION" --show-bin-path)/RemoteInputBridge"

# The icon is generated from source (scripts/make-icon.swift) and cached, so a rebuild does not
# pay for it unless the generator changed.
if [ ! -f "$ICON" ] || [ "$ROOT/scripts/make-icon.swift" -nt "$ICON" ]; then
    echo "==> generating the app icon"
    ICONSET="$RECEIVER/build/AppIcon.iconset"
    rm -rf "$ICONSET"
    swift "$ROOT/scripts/make-icon.swift" "$ICONSET" >/dev/null
    iconutil -c icns "$ICONSET" -o "$ICON"
fi

echo "==> assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BINARY" "$APP/Contents/MacOS/RemoteInputBridge"
cp "$ICON" "$APP/Contents/Resources/AppIcon.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>            <string>Remote Input Bridge</string>
    <key>CFBundleDisplayName</key>     <string>Remote Input Bridge</string>
    <key>CFBundleIdentifier</key>      <string>studio.lince.remoteinputbridge</string>
    <key>CFBundleExecutable</key>      <string>RemoteInputBridge</string>
    <key>CFBundleIconFile</key>        <string>AppIcon</string>
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

# Signing identity. An ad-hoc signature is a content hash, so it changes on every rebuild - and
# because macOS ties an Accessibility grant to the signature, the grant silently stops applying
# after each rebuild even though the checkbox stays on.
#
# To make the grant survive rebuilds, create a self-signed code signing certificate once
# (Keychain Access > Certificate Assistant > Create a Certificate: self-signed, type
# "Code Signing") and point this at it:
#
#     RIB_SIGN_IDENTITY="Remote Input Bridge Local" ./scripts/build-mac-app.sh --install
IDENTITY="${RIB_SIGN_IDENTITY:-}"
if [ -z "$IDENTITY" ]; then
    # Prefer a real identity when one is available: it keeps the Accessibility grant across
    # rebuilds, which an ad-hoc signature cannot do.
    IDENTITY="$(security find-identity -v -p codesigning 2>/dev/null \
        | sed -n 's/.*"\(Apple Development: [^"]*\)".*/\1/p' | head -1)"
    IDENTITY="${IDENTITY:--}"
fi
if codesign --force --sign "$IDENTITY" --timestamp=none "$APP" >/dev/null 2>&1; then
    if [ "$IDENTITY" = "-" ]; then
        echo "    signed ad-hoc - macOS may ask for Accessibility permission again"
        echo "    (set RIB_SIGN_IDENTITY to a stable certificate to avoid that)"
    else
        echo "    signed with $IDENTITY"
    fi
else
    echo "    (codesign failed; the app still runs, permission prompts may repeat)"
fi

if [ "$INSTALL" = 1 ]; then
    TARGET="$DESTINATION/RemoteInputBridge.app"
    echo "==> installing to $TARGET"
    if [ -d "$TARGET" ]; then
        # Quit the running copy first: replacing a bundle under a live process is asking for it.
        pkill -f "$TARGET/Contents/MacOS/RemoteInputBridge" 2>/dev/null || true
        sleep 1
        rm -rf "$TARGET"
    fi
    if ! cp -R "$APP" "$TARGET" 2>/dev/null; then
        echo "    /Applications is not writable; installing with sudo"
        sudo cp -R "$APP" "$TARGET"
    fi
    # A rebuilt bundle has a new ad-hoc identity, which macOS treats as a different app: the old
    # Accessibility grant stays visible in System Settings but no longer applies. Clearing the
    # entry means the user is prompted once instead of silently getting a dead cursor.
    if [ "$IDENTITY" = "-" ]; then
        tccutil reset Accessibility studio.lince.remoteinputbridge >/dev/null 2>&1 || true
        echo "    cleared the stale Accessibility entry - grant it again after launching"
    fi
    # Tell Launch Services about it so it shows up in Launchpad and Spotlight immediately.
    /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister \
        -f "$TARGET" 2>/dev/null || true
    APP="$TARGET"
    echo "    installed"
fi

if [ "$RUN" = 1 ]; then
    echo "==> launching"
    open "$APP"
fi

echo "==> done: $APP"
echo "    launch:  open '$APP'"
echo "    logs:    '$APP/Contents/MacOS/RemoteInputBridge' --log DEBUG"
