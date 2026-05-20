#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

MODE="${1:-run}"
PROXY_URL="${WARP_CUSTOM_AI_PROXY:-http://127.0.0.1:1337}"
PROXY_HEALTH_URL="${PROXY_URL%/}/healthz"
PROXY_INSTALL_DIR="${BYOB_PROXY_INSTALL_DIR:-$HOME/Library/Application Support/WarpOCA}"
PROXY_LOG_DIR="$PROXY_INSTALL_DIR/logs"
PROXY_LOG="$PROXY_LOG_DIR/warpoca_proxy.log"
PROXY_LABEL="com.oracle.warpoca.proxy"
PROXY_PLIST="$PROXY_INSTALL_DIR/$PROXY_LABEL.plist"
PROXY_BIN="$PROXY_INSTALL_DIR/byob_proxy"
PROXY_WRAPPER="$PROXY_INSTALL_DIR/byob_proxy_launchd_wrapper.sh"

usage() {
  echo "usage: $0 [run|proxy|logs|verify|-- <warp args>]" >&2
}

proxy_is_healthy() {
  curl -fsS "$PROXY_HEALTH_URL" >/dev/null 2>&1
}

write_proxy_launchd_plist() {
  cat >"$PROXY_PLIST" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>$PROXY_LABEL</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/bash</string>
    <string>$PROXY_WRAPPER</string>
  </array>
  <key>WorkingDirectory</key>
  <string>$PROXY_INSTALL_DIR</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>HOME</key>
    <string>$HOME</string>
    <key>PATH</key>
    <string>/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>
  <key>KeepAlive</key>
  <true/>
  <key>RunAtLoad</key>
  <true/>
  <key>StandardOutPath</key>
  <string>$PROXY_LOG</string>
  <key>StandardErrorPath</key>
  <string>$PROXY_LOG</string>
</dict>
</plist>
PLIST
}

start_proxy() {
  if proxy_is_healthy; then
    echo "BYOB proxy already healthy at $PROXY_URL"
    return
  fi

  mkdir -p "$PROXY_LOG_DIR"
  echo "Building BYOB proxy..."
  cargo build -p byob_proxy
  cp "$ROOT_DIR/target/debug/byob_proxy" "$PROXY_BIN"
  cp "$ROOT_DIR/script/byob_proxy_launchd_wrapper.sh" "$PROXY_WRAPPER"
  chmod +x "$PROXY_BIN" "$PROXY_WRAPPER"

  echo "Starting BYOB proxy at $PROXY_URL"
  write_proxy_launchd_plist
  launchctl bootout "gui/$(id -u)/$PROXY_LABEL" >/dev/null 2>&1 || true
  launchctl bootstrap "gui/$(id -u)" "$PROXY_PLIST"
  launchctl kickstart -k "gui/$(id -u)/$PROXY_LABEL"

  for _ in {1..120}; do
    if proxy_is_healthy; then
      echo "BYOB proxy is healthy. Log: $PROXY_LOG"
      return
    fi
    sleep 1
  done

  echo "BYOB proxy did not become healthy. Last log lines:" >&2
  tail -80 "$PROXY_LOG" >&2 || true
  exit 1
}

run_warp() {
  export WARP_CUSTOM_AI_PROXY="$PROXY_URL"
  export WARP_SKIP_COMMON_SKILLS_INSTALL="${WARP_SKIP_COMMON_SKILLS_INSTALL:-1}"
  exec ./script/run "$@"
}

case "$MODE" in
  run)
    start_proxy
    run_warp "${@:2}"
    ;;
  proxy|--proxy-only)
    start_proxy
    echo "BYOB proxy ready at $PROXY_URL"
    ;;
  logs|--logs)
    start_proxy
    tail -f "$PROXY_LOG"
    ;;
  verify|--verify)
    start_proxy
    export WARP_CUSTOM_AI_PROXY="$PROXY_URL"
    export WARP_SKIP_COMMON_SKILLS_INSTALL="${WARP_SKIP_COMMON_SKILLS_INSTALL:-1}"
    ./script/run --dont-open "${@:2}"
    test -d target/debug/bundle/osx/WarpOCA.app \
      -o -d target/release/bundle/osx/WarpOCA.app \
      -o -d target/debug/bundle/osx/WarpLocal.app \
      -o -d target/release/bundle/osx/WarpLocal.app
    ;;
  --help|-h)
    usage
    ;;
  --)
    shift
    start_proxy
    run_warp "$@"
    ;;
  *)
    start_proxy
    run_warp "$@"
    ;;
esac
