#!/bin/sh
# Installs the ThermalGuard privileged helper. Runs as root via the app's
# single admin prompt.
set -eu

APP_PATH="${1:-}"
HELPER_DST="/Library/PrivilegedHelperTools/com.thermalguard.daemon"
PLIST_DST="/Library/LaunchDaemons/com.thermalguard.daemon.plist"
LABEL="com.thermalguard.daemon"

if [ -z "$APP_PATH" ]; then
  echo "usage: install-helper.sh /path/to/ThermalGuard.app" >&2
  exit 64
fi

# This script runs as root, so validate its input strictly: it must be an .app
# directory, with no newlines or quote characters that could break the plist we
# generate below, and the binary must be a regular file inside it.
case "$APP_PATH" in
  *.app) ;;
  *) echo "refusing: not an app bundle: $APP_PATH" >&2; exit 64 ;;
esac
case "$APP_PATH" in
  *'"'*|*\\*) echo "refusing: unsafe characters in path" >&2; exit 64 ;;
esac
# A newline cannot be matched portably inside a case pattern, so test for it
# directly: it would break the plist generated further down.
if [ "$(printf '%s' "$APP_PATH" | tr -d '\n')" != "$APP_PATH" ]; then
  echo "refusing: unsafe characters in path" >&2
  exit 64
fi
if [ ! -d "$APP_PATH/Contents" ]; then
  echo "refusing: missing Contents directory" >&2
  exit 66
fi

# Refuse to install from an app whose signature does not validate, so a
# tampered bundle cannot get its payload copied in as root.
if [ -x /usr/bin/codesign ]; then
  if ! /usr/bin/codesign --verify --strict "$APP_PATH" >/dev/null 2>&1; then
    echo "refusing: app signature did not validate: $APP_PATH" >&2
    exit 66
  fi
fi

SRC="$APP_PATH/Contents/Resources/thermalguardd"
if [ ! -f "$SRC" ] || [ -L "$SRC" ]; then
  echo "helper binary not found at $SRC (or is a symlink)" >&2
  exit 66
fi
# The payload must actually be a Mach-O executable before it is copied as root.
# Compare the 4-byte magic as hex (od -c would print escapes, not hex).
MAGIC=$(od -An -tx1 -N4 "$SRC" | tr -d ' \n')
case "$MAGIC" in
  cffaedfe|feedfacf|cefaedfe|cafebabe|bebafeca) ;;
  *) echo "refusing: payload is not a Mach-O binary (magic '$MAGIC')" >&2; exit 66 ;;
esac

mkdir -p /Library/PrivilegedHelperTools
install -m 755 -o root -g wheel "$SRC" "$HELPER_DST"

cat > "$PLIST_DST" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>${LABEL}</string>
    <key>ProgramArguments</key><array><string>${HELPER_DST}</string></array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>ThrottleInterval</key><integer>10</integer>
    <key>Umask</key><integer>77</integer>
    <key>ProcessType</key><string>Interactive</string>
    <key>StandardErrorPath</key><string>/var/log/thermalguard.err</string>
</dict>
</plist>
PLIST

chown root:wheel "$PLIST_DST"
chmod 644 "$PLIST_DST"

# A socket left behind by a dead process would shadow the fresh daemon.
rm -f /var/run/thermalguard.sock

# Idempotent: re-running the installer replaces the daemon and reloads it, and
# because the job lives in /Library/LaunchDaemons it is started by launchd at
# every boot without asking for a password again.
launchctl bootout "system/${LABEL}" 2>/dev/null || true
launchctl bootstrap system "$PLIST_DST" || true
launchctl enable "system/${LABEL}" 2>/dev/null || true
launchctl kickstart -k "system/${LABEL}" 2>/dev/null || true

echo "installed"
