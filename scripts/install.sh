#!/usr/bin/env bash
#
# Build, install and verify Printables Manager.
#
#   scripts/install.sh              # release build → install → verify
#   scripts/install.sh --clean      # also wipe previous build output first
#   scripts/install.sh --no-build   # install the dmg that is already there
#
# Replaces the hand-typed command chains: quitting the running app, removing the
# installed copy, mounting at a fixed path so detach can never fail, signing and
# verification all happen here, in order, and any failure stops the script.

set -euo pipefail

APP_NAME="Printables Manager"
BUNDLE_ID="dev.printablesmanager.app"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TAURI_DIR="$ROOT/src-tauri"
DMG="$TAURI_DIR/target/release/bundle/dmg/${APP_NAME}_2.0.0_aarch64.dmg"
MOUNT="/tmp/pm-install"
SIGNING_IDENTITY="49C2A7CDD2C1C84BDE7F719CB4B359E61AF7227C"
INSTALLED="/Applications/${APP_NAME}.app"

CLEAN=0
BUILD=1
for arg in "$@"; do
  case "$arg" in
    --clean) CLEAN=1 ;;
    --no-build) BUILD=0 ;;
    -h|--help) sed -n '2,12p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

step() { printf '\033[1;34m==>\033[0m %s\n' "$1"; }
fail() { printf '\033[1;31mERROR:\033[0m %s\n' "$1" >&2; exit 1; }

# Always leave no disk image behind, even if something fails mid-way.
cleanup() {
  if mount | grep -q "on ${MOUNT} "; then
    hdiutil detach "$MOUNT" -quiet -force || true
  fi
  rmdir "$MOUNT" 2>/dev/null || true
}
trap cleanup EXIT

export PATH="/opt/homebrew/opt/rustup/bin:$PATH"
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
command -v cargo >/dev/null || fail "cargo not found"
command -v npx >/dev/null || fail "npx not found"

# ---------------------------------------------------------------- stale state
step "Removing stale mounts from previous runs"
for vol in /Volumes/*rintables*; do
  [ -e "$vol" ] || continue
  hdiutil detach "$vol" -force -quiet 2>/dev/null && echo "   detached $vol" || true
done

if [ "$CLEAN" = "1" ]; then
  step "Cleaning build output"
  rm -rf "$TAURI_DIR/target/release/bundle"
fi

# ---------------------------------------------------------------------- build
if [ "$BUILD" = "1" ]; then
  step "Frontend syntax"
  node --check "$ROOT/ui/dist/app.js"

  step "Rust tests"
  (cd "$TAURI_DIR" && cargo test --quiet)

  step "Building release + dmg"
  (cd "$ROOT" && npx --yes @tauri-apps/cli@2 build --bundles dmg)
else
  step "Skipping build (--no-build)"
fi

[ -f "$DMG" ] || fail "no dmg at $DMG"

# -------------------------------------------------------------------- install
step "Quitting any running copy"
if pgrep -f "$INSTALLED/Contents/MacOS/" >/dev/null; then
  osascript -e "tell application \"$APP_NAME\" to quit" >/dev/null 2>&1 || true
  for _ in 1 2 3 4 5; do
    pgrep -f "$INSTALLED/Contents/MacOS/" >/dev/null || break
    sleep 1
  done
  pkill -f "$INSTALLED/Contents/MacOS/" 2>/dev/null || true
fi

step "Removing the existing app"
rm -rf "$INSTALLED"

step "Installing from dmg"
cleanup
mkdir -p "$MOUNT"
hdiutil attach "$DMG" -nobrowse -quiet -mountpoint "$MOUNT"
ditto "$MOUNT/${APP_NAME}.app" "$INSTALLED"
cleanup
xattr -dr com.apple.quarantine "$INSTALLED" 2>/dev/null || true

# --------------------------------------------------------------------- verify
step "Verifying the bundle"
[ -d "$INSTALLED" ] || fail "app was not installed"
VERSION="$(defaults read "$INSTALLED/Contents/Info.plist" CFBundleShortVersionString)"
BUNDLE="$(defaults read "$INSTALLED/Contents/Info.plist" CFBundleIdentifier)"
[ "$BUNDLE" = "$BUNDLE_ID" ] || fail "bundle id is $BUNDLE, expected $BUNDLE_ID"

# An ad-hoc signature gets a new CDHash on every build, and macOS then treats the
# app as a stranger: Keychain access and external-volume access are re-asked every
# single time. Re-sign with the stable Developer identity after installing.
AUTHORITY="$(codesign -dvv "$INSTALLED" 2>&1 | awk -F'=' '/^Authority/{print $2; exit}' || true)"
if [ -z "$AUTHORITY" ]; then
  step "Re-signing with the stable Developer identity"
  codesign --force --deep --options runtime \
    --entitlements "$TAURI_DIR/entitlements.plist" \
    -s "$SIGNING_IDENTITY" "$INSTALLED"
  AUTHORITY="$(codesign -dvv "$INSTALLED" 2>&1 | awk -F'=' '/^Authority/{print $2; exit}' || true)"
fi
[ -n "$AUTHORITY" ] || fail "app is still ad-hoc signed - permission prompts would return on every build"
codesign --verify --deep --strict "$INSTALLED" 2>/dev/null || fail "code signature invalid"
echo "   version    $VERSION"
echo "   bundle     $BUNDLE"
echo "   signature  ${AUTHORITY:-(ad-hoc)}"
echo "   installed  $INSTALLED"

if mount | grep -q "on ${MOUNT} "; then fail "install volume still mounted"; fi

step "MCP server present"
[ -x "$INSTALLED/Contents/MacOS/printables-manager-mcp" ] || fail "MCP binary missing from the bundle"
echo "   $(printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | "$INSTALLED/Contents/MacOS/printables-manager-mcp" \
  | python3 -c 'import sys,json;print(len(json.load(sys.stdin)["result"]["tools"]),"tools")')"

step "Self-test"
"$INSTALLED/Contents/MacOS/printables-manager" --selftest 2>&1 | sed 's/^/   /'

step "Done"
