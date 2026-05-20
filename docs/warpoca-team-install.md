# WarpOCA Team Install

WarpOCA is packaged as a normal macOS app bundle. Teammates should not need
Rust, Xcode, Cargo, or a local build of Warp.

## Build The Team Zip

From this repo, run:

```bash
./script/package_warpoca.sh
```

The distributable is written to:

```text
dist/WarpOCA-macos-arm64.zip
```

## Install

1. Unzip `WarpOCA-macos-arm64.zip`.
2. Move `WarpOCA.app` to `/Applications`.
3. Make sure Codex CLI is logged in on the Mac:

```bash
codex login
```

4. Launch `WarpOCA.app`.

WarpOCA does not prompt for an Oracle Code Assist API key. It uses the Codex
CLI auth file at:

```text
~/.codex/auth.json
```

If that file is missing, Agent Mode will show a setup message asking the user
to run `codex login`.

## Why The Local Proxy Exists

Warp's agent UI sends and receives Warp-specific protobuf-over-SSE messages.
Oracle Code Assist uses an OpenAI-compatible JSON/SSE API. The bundled local
proxy is the protocol adapter between those two worlds.

```text
WarpOCA.app
  -> Warp protobuf/SSE
  -> bundled local proxy on 127.0.0.1:1337
  -> Oracle Code Assist Responses API
```

Users should not launch or manage the proxy themselves. The app starts the
bundled helper automatically and restarts it if it exits.

## Local Feature Coverage

WarpOCA keeps Warp's terminal UX local-first:

- Agent Mode and passive suggestions route through Oracle Code Assist.
- Next Command routes through the local proxy, with built-in local fallbacks for common terminal flows like `brew search nvtop` -> `brew install nvtop`.
- Agent prompt suggestions, partial Agent Mode query completion, and relevant-file ranking route through the local proxy instead of Warp cloud.
- Workflows and notebooks use Warp's native UI and local SQLite-backed object model. Creates and updates are acknowledged locally; Warp Drive sharing/sync is intentionally not included.
