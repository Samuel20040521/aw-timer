#!/bin/bash
# Run the CI checks on this Mac before pushing, so CI runs are not spent
# finding mistakes. Steps that need tools you don't have are reported as
# SKIP, never as passed:
#   - MSRV and x86_64 need rustup:  brew install rustup && rustup-init
#                                   rustup toolchain install 1.88
#                                   rustup target add x86_64-apple-darwin
#   - shellcheck / actionlint:      brew install shellcheck actionlint
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "${ROOT}"
MSRV="$(awk -F'"' '/^rust-version/ {print $2}' Cargo.toml)"
summary=()
pass() { summary+=("PASS  $1"); }
skip() { summary+=("SKIP  $1 ($2)"); }
step() { printf '\n==> %s\n' "$1"; }

step "test"
cargo test --release --quiet
pass "cargo test"

step "clippy"
cargo clippy --release --quiet -- -D warnings
pass "clippy -D warnings"

step "MSRV ${MSRV}"
if command -v rustup > /dev/null && rustup toolchain list | grep -q "^${MSRV}"; then
    cargo "+${MSRV}" check --quiet
    pass "cargo +${MSRV} check"
else
    skip "MSRV ${MSRV}" "needs rustup with the ${MSRV} toolchain"
fi

step "release build for macOS 11 (separate target dir)"
export MACOSX_DEPLOYMENT_TARGET=11.0
export CARGO_TARGET_DIR="${ROOT}/target/ci"
cargo build --release --quiet --target aarch64-apple-darwin
BIN="${CARGO_TARGET_DIR}/aarch64-apple-darwin/release/aw-timer"
if rustc --print target-libdir --target x86_64-apple-darwin > /dev/null 2>&1 \
    && [[ -d "$(rustc --print target-libdir --target x86_64-apple-darwin)" ]]; then
    cargo build --release --quiet --target x86_64-apple-darwin
    lipo -create -output "${CARGO_TARGET_DIR}/aw-timer-universal" \
        "${BIN}" "${CARGO_TARGET_DIR}/x86_64-apple-darwin/release/aw-timer"
    BIN="${CARGO_TARGET_DIR}/aw-timer-universal"
    pass "x86_64 build"
else
    skip "x86_64 build" "needs rustup target add x86_64-apple-darwin"
fi
codesign --force --sign - "${BIN}"
scripts/ci/check-binary.sh "${BIN}"
pass "binary checks (minos 11.0, weak ProcessTap)"

step "app bundle"
scripts/ci/check-bundle.sh "${BIN}"
pass "app bundle"

step "shell scripts"
scripts/ci/check-shell.sh
if command -v shellcheck > /dev/null; then pass "shell (bash -n, shellcheck)"; else pass "shell (bash -n)"; skip "shellcheck" "not installed"; fi

step "workflow files"
if command -v actionlint > /dev/null; then
    actionlint
    pass "actionlint"
else
    skip "actionlint" "not installed"
fi

printf '\n==> summary\n'
printf '%s\n' "${summary[@]}"
