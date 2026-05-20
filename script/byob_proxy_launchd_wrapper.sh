#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

export HOME="${HOME:-$("$SHELL" -lc 'printf %s "$HOME"' 2>/dev/null || true)}"
export PATH="/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin"

echo "[$(date -u +%Y-%m-%dT%H:%M:%SZ)] starting WarpOCA proxy from $SCRIPT_DIR" >&2
exec "$SCRIPT_DIR/byob_proxy"
