#!/bin/bash
# Build aw-timer (Rust) and install it as a per-user LaunchAgent.
# Replaces the Python version if installed (same label and config file).
# Use ${VAR} braces before CJK text: bash may otherwise read the multibyte
# bytes as part of the variable name and abort under `set -u`.
set -euo pipefail

SRC="$(cd "$(dirname "$0")" && pwd)"
LABEL="local.aw-timer"
APP_DIR="${HOME}/Library/Application Support/aw-timer"
CONF_DIR="${HOME}/.config/aw-timer"
BIN_LINK="${HOME}/.local/bin/aw-timer"
PLIST="${HOME}/Library/LaunchAgents/${LABEL}.plist"
LOG="${HOME}/Library/Logs/aw-timer.log"
# A minimal app bundle: macOS only routes notification clicks back to apps.
APP="${APP_DIR}/aw-timer.app"
EXE="${APP}/Contents/MacOS/aw-timer"

# Release archives ship a prebuilt universal binary next to this script;
# a source checkout builds one with cargo.
if [[ -x "${SRC}/aw-timer" && ! -f "${SRC}/Cargo.toml" ]]; then
    BUILT="${SRC}/aw-timer"
else
    command -v cargo >/dev/null || { echo "需要 Rust（cargo）才能從原始碼編譯：https://rustup.rs" >&2; exit 1; }
    cargo build --release --manifest-path "${SRC}/Cargo.toml"
    BUILT="${SRC}/target/release/aw-timer"
fi

mkdir -p "${APP_DIR}" "${CONF_DIR}" "$(dirname "${BIN_LINK}")" \
         "$(dirname "${PLIST}")" "$(dirname "${LOG}")"
# Stop the running agent before replacing its executable.
launchctl bootout "gui/$(id -u)/${LABEL}" 2>/dev/null || true
rm -rf "${APP}"
mkdir -p "${APP}/Contents/MacOS"
install -m 755 "${BUILT}" "${EXE}"
cp "${SRC}/Info.plist" "${APP}/Contents/Info.plist"
# Sign the bundle (ad hoc) so its Info.plist is bound to the signature;
# macOS needs it for the Calendar and notification permission prompts.
codesign --force --sign - "${APP}" 2>/dev/null || true
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister \
    -f "${APP}" 2>/dev/null || true
# Leftovers of older installs: the bare binary and the Python package.
rm -rf "${APP_DIR}/aw-timer" "${APP_DIR}/aw_timer"

# Never overwrite a config the user has already edited.
if [[ ! -f "${CONF_DIR}/config.toml" ]]; then
    cp "${SRC}/default_config.toml" "${CONF_DIR}/config.toml"
    echo "建立設定檔：${CONF_DIR}/config.toml"
fi

rm -f "${BIN_LINK}"
ln -s "${EXE}" "${BIN_LINK}"

cat > "${PLIST}" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>${LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>${EXE}</string><string>run</string>
    </array>
    <key>RunAtLoad</key><true/>
    <!-- Restart on crash, but stay quit after "結束 aw-timer" in the menu. -->
    <key>KeepAlive</key>
    <dict><key>SuccessfulExit</key><false/></dict>
    <key>ThrottleInterval</key><integer>30</integer>
    <!-- The menu bar item needs the logged-in GUI session. -->
    <key>LimitLoadToSessionType</key><string>Aqua</string>
    <key>ProcessType</key><string>Interactive</string>
    <key>StandardOutPath</key><string>${LOG}</string>
    <key>StandardErrorPath</key><string>${LOG}</string>
</dict>
</plist>
EOF

launchctl bootstrap "gui/$(id -u)" "${PLIST}"
echo "已安裝並啟動 ${LABEL}（log：${LOG}）"
echo "指令：aw-timer status | report | test-notify | restart"
