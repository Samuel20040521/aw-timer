#!/bin/bash
# Check a built aw-timer binary for what the README promises:
# runs on macOS 11+, and the macOS 14.2+ CoreAudio symbols cpal references
# are weak imports (see build.rs). Used by CI, the release build and
# scripts/ci/local.sh.
set -euo pipefail

BIN="${1:?usage: check-binary.sh <path to aw-timer>}"
# Absolute, so that running "${BIN}" never searches PATH (a bare
# "aw-timer" would be looked up there, not in the current directory).
BIN="$(cd "$(dirname "${BIN}")" && pwd)/$(basename "${BIN}")"

# Every architecture slice must target macOS 11.0.
minos="$(otool -l "${BIN}" | grep -A3 LC_BUILD_VERSION | awk '/minos/ {print $2}' | sort -u)"
if [[ "${minos}" != "11.0" ]]; then
    echo "::error::minimum macOS is '${minos//$'\n'/, }', expected 11.0 (set MACOSX_DEPLOYMENT_TARGET=11.0)"
    exit 1
fi

if nm -m "${BIN}" | grep "ProcessTap" | grep -qv "weak external"; then
    echo "::error::CoreAudio ProcessTap symbols are strongly linked (macOS 14.2+ only)"
    exit 1
fi

"${BIN}" --help > /dev/null
echo "ok: $(lipo -archs "${BIN}"), minos ${minos}, ProcessTap weak"
