#!/bin/bash
# Build aw-timer.app the way install.sh does and check that macOS will
# accept it: valid Info.plist, a valid ad-hoc signature, and the Info.plist
# bound to that signature (needed for the Calendar and notification
# permission prompts).
set -euo pipefail

BIN="${1:?usage: check-bundle.sh <path to aw-timer>}"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT
APP="${WORK}/aw-timer.app"

plutil -lint "${ROOT}/Info.plist"
mkdir -p "${APP}/Contents/MacOS"
install -m 755 "${BIN}" "${APP}/Contents/MacOS/aw-timer"
cp "${ROOT}/Info.plist" "${APP}/Contents/Info.plist"
codesign --force --sign - "${APP}"
codesign --verify --strict --verbose=1 "${APP}"

info="$(codesign -dv "${APP}" 2>&1)"
grep -q "^Identifier=local.aw-timer$" <<< "${info}" || { echo "::error::bundle identifier is not local.aw-timer"; exit 1; }
grep -q "^Info.plist entries=" <<< "${info}" || { echo "::error::Info.plist is not bound to the signature"; exit 1; }

# The binary must also carry its own copy (build.rs, __TEXT,__info_plist)
# for `report -w` and other runs outside the bundle.
embedded="$(otool -X -s __TEXT __info_plist "${BIN}" 2>/dev/null | wc -l | tr -d ' ')"
[[ "${embedded}" -gt 0 ]] || { echo "::error::binary has no embedded Info.plist section"; exit 1; }

"${APP}/Contents/MacOS/aw-timer" --help > /dev/null
echo "ok: aw-timer.app signed, Info.plist bound"
