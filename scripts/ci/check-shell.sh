#!/bin/bash
# Syntax-check every shell script, and lint them with shellcheck when it
# is installed (CI's Ubuntu runner has it; locally: brew install shellcheck).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
scripts=("${ROOT}/install.sh" "${ROOT}/uninstall.sh" "${ROOT}"/scripts/ci/*.sh)

for s in "${scripts[@]}"; do
    bash -n "${s}"
done
echo "ok: bash -n on ${#scripts[@]} scripts"

if command -v shellcheck > /dev/null; then
    shellcheck --severity=warning "${scripts[@]}"
    echo "ok: shellcheck"
elif [[ -n "${CI:-}" ]]; then
    echo "::error::shellcheck is missing on the CI runner"
    exit 1
else
    echo "SKIP: shellcheck not installed (brew install shellcheck)"
fi
