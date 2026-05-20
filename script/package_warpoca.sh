#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

DIST_DIR="$ROOT_DIR/dist"
APP_PATH="$ROOT_DIR/target/release/bundle/osx/WarpOCA.app"
DIST_APP_PATH="$DIST_DIR/WarpOCA.app"
ZIP_PATH="$DIST_DIR/WarpOCA-macos-arm64.zip"

mkdir -p "$DIST_DIR"

PATH="$ROOT_DIR/target/byob-metal-xcrun:$PATH" ./script/build_and_run.sh verify --release

if [ ! -d "$APP_PATH" ]; then
  echo "Expected app bundle not found: $APP_PATH" >&2
  exit 1
fi

rm -f "$ZIP_PATH"
rm -rf "$DIST_APP_PATH"
ditto "$APP_PATH" "$DIST_APP_PATH"
ditto -c -k --keepParent "$APP_PATH" "$ZIP_PATH"

echo "Packaged $ZIP_PATH"
echo "Copied app bundle to $DIST_APP_PATH"
echo "Team install: unzip, move WarpOCA.app to /Applications, run codex login once if needed, then launch WarpOCA.app."
