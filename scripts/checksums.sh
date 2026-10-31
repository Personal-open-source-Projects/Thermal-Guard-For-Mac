#!/bin/bash
# Write MD5SUMS and SHA256SUMS for the release installers next to the repo root.
# Both files are uploaded as release assets, not committed.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

shopt -s nullglob
DMGS=(*.dmg)
if [ ${#DMGS[@]} -eq 0 ]; then
  echo "no .dmg in $ROOT; run scripts/make_dmg.sh first" >&2
  exit 1
fi

md5 -r "${DMGS[@]}" > MD5SUMS
shasum -a 256 "${DMGS[@]}" > SHA256SUMS

echo "wrote:"
echo "  $ROOT/MD5SUMS"
echo "  $ROOT/SHA256SUMS"
cat MD5SUMS SHA256SUMS
