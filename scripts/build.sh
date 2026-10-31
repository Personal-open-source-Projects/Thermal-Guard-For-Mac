#!/bin/bash
# Build ThermalGuard.app (Intel x86_64) from the Rust daemon + Swift UI.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD="$ROOT/build"
APP="$BUILD/ThermalGuard.app"
export PATH="$HOME/.cargo/bin:$PATH"

echo "==> 1/6 Rust daemon (release, x86_64-apple-darwin)"
cargo build --release --manifest-path "$ROOT/daemon/Cargo.toml"
DAEMON="$ROOT/daemon/target/release/thermalguardd"

echo "==> 2/6 icon"
python3 "$ROOT/scripts/make_icon.py"

echo "==> 3/6 Swift UI"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
xcrun -sdk macosx swiftc -O \
  -target x86_64-apple-macosx13.0 \
  -framework AppKit -framework Foundation -framework CoreMediaIO -framework SwiftUI \
  -o "$APP/Contents/MacOS/ThermalGuard" \
  "$ROOT/app/Sources/main.swift"

echo "==> 4/6 bundle"
cp "$ROOT/app/Info.plist" "$APP/Contents/Info.plist"
cp "$DAEMON" "$APP/Contents/Resources/thermalguardd"
cp "$ROOT/app/Resources/install-helper.sh" "$APP/Contents/Resources/install-helper.sh"
chmod 755 "$APP/Contents/Resources/thermalguardd" "$APP/Contents/Resources/install-helper.sh"
if [ -f "$BUILD/icon/AppIcon.icns" ]; then
  cp "$BUILD/icon/AppIcon.icns" "$APP/Contents/Resources/AppIcon.icns"
fi
printf 'APPL????' > "$APP/Contents/PkgInfo"

echo "==> 5/6 ad-hoc codesign (hardened runtime)"
# Hardened runtime even with an ad-hoc identity: it still turns on the
# platform's code-injection and debugger hardening for both processes.
codesign --force --sign - --options runtime --timestamp=none \
  --identifier com.thermalguard.app.daemon \
  "$APP/Contents/Resources/thermalguardd" >/dev/null 2>&1 || true
codesign --force --sign - --options runtime --timestamp=none \
  --identifier com.thermalguard.app "$APP" >/dev/null 2>&1 || true
codesign --verify --strict --verbose=1 "$APP" 2>&1 | tail -2
codesign -d --entitlements - "$APP" 2>/dev/null | head -1 || true

echo "==> 6/6 done"
echo "    $APP"
