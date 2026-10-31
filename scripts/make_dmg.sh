#!/bin/bash
# Package ThermalGuard.app into a drag-to-install DMG.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD="$ROOT/build"
APP="$BUILD/ThermalGuard.app"
STAGE="$BUILD/dmg-stage"
DMG="$ROOT/ThermalGuard-1.1.0.dmg"

if [ ! -d "$APP" ]; then
  echo "app bundle missing; run scripts/build.sh first" >&2
  exit 1
fi

rm -rf "$STAGE" "$DMG"
mkdir -p "$STAGE"
cp -R "$APP" "$STAGE/ThermalGuard.app"
ln -s /Applications "$STAGE/Applications"

cat > "$STAGE/READ ME FIRST.txt" <<'TXT'
ThermalGuard — stop MacBook Air 2020 overheating

1. Drag ThermalGuard.app into Applications.
2. First launch: right-click the app > Open (it is ad-hoc signed, not notarised).
3. Click the thermometer in the menu bar, then "Install Thermal Helper…".
   You will be asked for your admin password once. This installs a small root
   daemon that reads the SMC temperature sensor and applies the power cap.
4. Pick a mode:
     Off         - monitoring only
     Balanced    - caps power only when the CPU passes ~82°C
     Cool & Quiet- caps power always, demotes background hogs early
     Max Cool    - most aggressive; use for video calls / exports
5. Optional: "Start ThermalGuard at login".

What it does NOT do: it never kills or suspends your apps, and it never writes
to the fan on models whose SMC ignores fan targets (it detects that and says so).
TXT

hdiutil create -volname "ThermalGuard" \
  -srcfolder "$STAGE" \
  -fs HFS+ \
  -format UDZO \
  -ov \
  "$DMG" >/dev/null

rm -rf "$STAGE"
echo "created: $DMG"
ls -lh "$DMG"
