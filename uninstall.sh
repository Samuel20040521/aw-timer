#!/bin/bash
# Remove the aw-timer LaunchAgent and installed binary. Keeps your config.
set -euo pipefail

LABEL="local.aw-timer"
launchctl bootout "gui/$(id -u)/${LABEL}" 2>/dev/null || true
rm -f "${HOME}/Library/LaunchAgents/${LABEL}.plist" "${HOME}/.local/bin/aw-timer"
rm -rf "${HOME}/Library/Application Support/aw-timer"
echo "已移除 aw-timer（設定檔保留在 ~/.config/aw-timer/）"
