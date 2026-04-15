#!/bin/bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

export CXXFLAGS="${CXXFLAGS:-"-I$(xcrun --show-sdk-path)/usr/include/c++/v1"}"
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-11.0}"

DEV_CONFIG="tauri/src-tauri/tauri.dev.conf.json"
DEV_PRODUCT_NAME="Minutes Dev"
DEV_BUNDLE_ID="com.useminutes.desktop.dev"
LOCAL_SIGNING_IDENTITY_DEFAULT="Minutes Dev Local Signing"
BUILD_APP="target/release/bundle/macos/${DEV_PRODUCT_NAME}.app"
INSTALL_DIR="${INSTALL_DIR:-$HOME/Applications}"
INSTALL_APP="${INSTALL_DIR}/${DEV_PRODUCT_NAME}.app"
SIGNING_IDENTITY="${MINUTES_DEV_SIGNING_IDENTITY:-${APPLE_SIGNING_IDENTITY:-}}"
SIGN_MODE="adhoc"

quit_running_dev_app() {
  osascript -e "tell application \"${DEV_PRODUCT_NAME}\" to quit" >/dev/null 2>&1 || true
  for _ in {1..20}; do
    if ! pgrep -f "${INSTALL_APP}/Contents/MacOS/minutes-app" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.5
  done
  pkill -f "${INSTALL_APP}/Contents/MacOS/minutes-app" >/dev/null 2>&1 || true
}

OPEN_AFTER_INSTALL=1
RESET_SCREEN_RECORDING_PERMISSION="${MINUTES_DEV_RESET_SCREEN_RECORDING:-1}"
for arg in "$@"; do
  case "$arg" in
    --no-open)
      OPEN_AFTER_INSTALL=0
      ;;
    --reset-screen-recording-permission)
      RESET_SCREEN_RECORDING_PERMISSION=1
      ;;
    --keep-screen-recording-permission)
      RESET_SCREEN_RECORDING_PERMISSION=0
      ;;
    *)
      echo "Unknown option: $arg" >&2
      echo "Usage: ./scripts/install-dev-app.sh [--no-open] [--reset-screen-recording-permission] [--keep-screen-recording-permission]" >&2
      exit 1
      ;;
  esac
done

if [[ -z "$SIGNING_IDENTITY" ]]; then
  if security find-identity -v -p codesigning | grep -Fq "\"$LOCAL_SIGNING_IDENTITY_DEFAULT\""; then
    SIGNING_IDENTITY="$LOCAL_SIGNING_IDENTITY_DEFAULT"
  fi
fi

if [[ -n "$SIGNING_IDENTITY" ]]; then
  if ! security find-identity -v -p codesigning | grep -Fq "$SIGNING_IDENTITY"; then
    echo "Signing identity not found: $SIGNING_IDENTITY" >&2
    echo "Set MINUTES_DEV_SIGNING_IDENTITY (preferred) or APPLE_SIGNING_IDENTITY to a valid codesigning identity in your keychain." >&2
    exit 1
  fi
  SIGN_MODE="identity"
fi

echo "=== Building CLI (release) ==="
cargo build --release -p minutes-cli

echo "=== Building calendar helper ==="
swiftc -O \
  -Xlinker -sectcreate -Xlinker __TEXT -Xlinker __info_plist \
  -Xlinker scripts/calendar-helper-Info.plist \
  scripts/calendar-events.swift -o target/release/calendar-events

echo "=== Building ${DEV_PRODUCT_NAME}.app ==="
cargo tauri build --bundles app --config "$DEV_CONFIG" --no-sign

echo "=== Embedding calendar helper in dev bundle ==="
APP_RESOURCES="${BUILD_APP}/Contents/Resources"
mkdir -p "$APP_RESOURCES"
cp -f target/release/calendar-events "$APP_RESOURCES/calendar-events"

if [[ "$SIGN_MODE" == "identity" ]]; then
  echo "=== Signing ${DEV_PRODUCT_NAME}.app with configured identity ==="
  codesign --force --deep --options runtime \
    --entitlements tauri/src-tauri/entitlements.plist \
    --sign "$SIGNING_IDENTITY" \
    "$BUILD_APP"
else
  echo "=== Signing ${DEV_PRODUCT_NAME}.app ad-hoc ==="
  echo "No MINUTES_DEV_SIGNING_IDENTITY / APPLE_SIGNING_IDENTITY configured."
  echo "Using ad-hoc signing so the app remains runnable for contributors."
  echo "TCC-sensitive features may still require re-granting permissions after rebuilds."
  codesign --force --deep --sign - "$BUILD_APP"
fi

echo "=== Installing ${DEV_PRODUCT_NAME}.app to ${INSTALL_DIR} ==="
mkdir -p "$INSTALL_DIR"
quit_running_dev_app
rm -rf "$INSTALL_APP"
cp -rf "$BUILD_APP" "$INSTALL_APP"

echo "=== Normalizing helper identities inside installed app ==="
HELPER_SIGN_ARGS=(--force --sign -)
if [[ "$SIGN_MODE" == "identity" ]]; then
  HELPER_SIGN_ARGS=(
    --force
    --options runtime
    --entitlements tauri/src-tauri/entitlements.plist
    --sign "$SIGNING_IDENTITY"
  )
fi

codesign "${HELPER_SIGN_ARGS[@]}" -i "$DEV_BUNDLE_ID" \
  "$INSTALL_APP/Contents/MacOS/system_audio_record"
codesign "${HELPER_SIGN_ARGS[@]}" -i "$DEV_BUNDLE_ID" \
  "$INSTALL_APP/Contents/MacOS/mic_check"

echo "=== Resealing installed ${DEV_PRODUCT_NAME}.app ==="
if [[ "$SIGN_MODE" == "identity" ]]; then
  codesign --force --options runtime \
    --entitlements tauri/src-tauri/entitlements.plist \
    --sign "$SIGNING_IDENTITY" \
    "$INSTALL_APP"
else
  codesign --force --sign - "$INSTALL_APP"
fi

if [[ "$RESET_SCREEN_RECORDING_PERMISSION" == "1" ]]; then
  echo "=== Resetting Screen & System Audio Recording permissions for ${DEV_PRODUCT_NAME} ==="
  tccutil reset ScreenCapture "$DEV_BUNDLE_ID" >/dev/null 2>&1 || true
  tccutil reset AudioCapture "$DEV_BUNDLE_ID" >/dev/null 2>&1 || true
fi

echo "=== Running native hotkey diagnostic from installed dev app ==="
set +e
./scripts/diagnose-desktop-hotkey.sh "$INSTALL_APP"
DIAG_EXIT=$?
set -e

echo ""
echo "Installed app: $INSTALL_APP"
echo "Bundle id: $DEV_BUNDLE_ID"
echo "Signing mode: $SIGN_MODE"
echo "Hotkey diagnostic exit code: $DIAG_EXIT"
echo "  0 = CGEventTap started successfully"
echo "  2 = Input Monitoring / macOS identity is still blocking the hotkey"
if [[ "$RESET_SCREEN_RECORDING_PERMISSION" == "1" ]]; then
  echo "Screen & System Audio Recording permissions have been reset for this dev install."
  echo "macOS may not prompt for this service. Re-enable Minutes Dev in System Settings > Privacy & Security > Screen & System Audio Recording before testing."
else
  echo "Screen & System Audio Recording permissions were left unchanged for this dev install."
  echo "Pass --reset-screen-recording-permission to force a clean grant on this install."
fi
echo ""
echo "For TCC-sensitive testing, launch only this installed dev app."
echo "Avoid the repo symlink (./Minutes.app), raw target bundles, or ad-hoc builds."
if [[ "$SIGN_MODE" == "adhoc" ]]; then
  echo ""
  echo "Tip: export MINUTES_DEV_SIGNING_IDENTITY to a consistent local signing identity"
  echo "if you want more stable macOS permission behavior across rebuilds."
fi

if [[ "$OPEN_AFTER_INSTALL" == "1" ]]; then
  echo ""
  echo "=== Launching ${DEV_PRODUCT_NAME}.app ==="
  open -a "$INSTALL_APP"
fi
