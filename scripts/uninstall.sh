#!/bin/bash
# Remove ThermalGuard from this Mac: the privileged helper, its LaunchDaemon,
# the app, the user's login item, preferences and logs.
#
# Run with root rights, once:  sudo bash scripts/uninstall.sh
set -uo pipefail

LABEL="com.thermalguard.daemon"
PLIST="/Library/LaunchDaemons/${LABEL}.plist"
HELPER="/Library/PrivilegedHelperTools/${LABEL}"
LOG="/var/log/thermalguard.log"
ERR="/var/log/thermalguard.err"
RUN_DIR="/var/run/thermalguard"
SOCK="/var/run/thermalguard.sock"
APP="/Applications/ThermalGuard.app"
CONSOLE_USER="${SUDO_USER:-$(stat -f %Su /dev/console)}"
CONSOLE_HOME="$(dscl . -read "/Users/${CONSOLE_USER}" NFSHomeDirectory 2>/dev/null | awk '{print $2}')"
[ -n "${CONSOLE_HOME:-}" ] || CONSOLE_HOME="/Users/${CONSOLE_USER}"

if [ "$(id -u)" -ne 0 ]; then
  echo "run this with root rights: sudo bash scripts/uninstall.sh" >&2
  exit 1
fi

echo "==> stop and unload the daemon"
launchctl bootout "system/${LABEL}" 2>/dev/null || true
pkill -f "${HELPER}" 2>/dev/null || true
pkill -x thermalguardd 2>/dev/null || true

echo "==> remove the helper and its LaunchDaemon"
rm -f "$PLIST" "$HELPER"
rm -f "$SOCK"
rm -rf "$RUN_DIR"

echo "==> remove logs"
rm -f "$LOG" "$LOG.rot" "$ERR"

echo "==> remove the app"
rm -rf "$APP"

echo "==> remove the login item, preferences and remaining user files"
rm -f "${CONSOLE_HOME}/Library/LaunchAgents/com.thermalguard.app.plist"
rm -f "${CONSOLE_HOME}/Library/Preferences/com.thermalguard.app.plist"
sudo -u "$CONSOLE_USER" defaults delete com.thermalguard.app 2>/dev/null || true
rm -rf "${CONSOLE_HOME}/Library/Application Support/ThermalGuard"
rm -rf "${CONSOLE_HOME}/Library/Caches/com.thermalguard.app"

echo "==> verify"
left=0
for p in "$PLIST" "$HELPER" "$LOG" "$ERR" "$SOCK" "$RUN_DIR" "$APP" \
         "${CONSOLE_HOME}/Library/LaunchAgents/com.thermalguard.app.plist" \
         "${CONSOLE_HOME}/Library/Preferences/com.thermalguard.app.plist"; do
  if [ -e "$p" ]; then
    echo "still present: $p"
    left=$((left + 1))
  fi
done
if launchctl print "system/${LABEL}" >/dev/null 2>&1; then
  echo "still present: launchd job system/${LABEL}"
  left=$((left + 1))
fi

if [ "$left" -eq 0 ]; then
  echo "ThermalGuard is fully removed."
else
  echo "$left item(s) could not be removed." >&2
  exit 1
fi
