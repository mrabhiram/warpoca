# WarpOCA Windows Distribution Plan

This is the Windows path for bringing the macOS WarpOCA packaging model over to
teammates who run Windows.

## Current Starting Point

- The upstream fork already has a Windows build and installer pipeline:
  - `script/windows/bootstrap.ps1`
  - `script/windows/bundle.ps1`
  - `script/windows/windows-installer.iss`
  - `script/windows/prepare_bundled_resources.ps1`
- The installer is Inno Setup based and installs a channel-specific Warp binary,
  ConPTY/OpenConsole assets, DirectX shader runtime files, bundled resources, and
  shell integration.
- `byob_proxy` is a Rust crate, so it should build as `byob_proxy.exe` for
  `x86_64-pc-windows-msvc` and `aarch64-pc-windows-msvc` once the Windows Rust
  toolchain is installed.

## Implemented Code Changes

1. Added a Windows channel name for WarpOCA.
   - `script/windows/bundle.ps1` now accepts `-CHANNEL warpoca`.
   - It builds `warp-oss` and packages it as `WarpOCA.exe`.
   - Installer output is `WarpOCASetup.exe` or `WarpOCASetup-arm64.exe`.

2. Built and installed the proxy helper.
   - The Windows bundle script builds `byob_proxy.exe` for the selected target.
   - `script/windows/windows-installer.iss` installs it to `{app}\Helpers`.

3. Made proxy startup cross-platform.
   - Windows helper path: `{app}\Helpers\byob_proxy.exe`.
   - Windows app data: `%APPDATA%\WarpOCA`.
   - Windows Codex auth: `%USERPROFILE%\.codex\auth.json`, with `CODEX_HOME`
     override.
   - The health check remains `127.0.0.1:1337`.

4. Made local Rules and Knowledge paths cross-platform.
   - macOS: `~/Library/Application Support/WarpOCA`.
   - Windows: `%APPDATA%\WarpOCA`.
   - Linux/dev fallback: `$XDG_DATA_HOME/WarpOCA` or `~/.local/share/WarpOCA`.

5. Added a one-command Windows packaging wrapper.
   - `script/windows/package_warpoca.ps1 -ARCH x64`.
   - See `docs/warpoca-windows-build-handoff.md`.

## Still Needed For Internal Distribution

- Run the build on Windows with Visual Studio Build Tools, Windows SDK, and Inno
  Setup.
- Sign the installer and binaries with Authenticode via the existing
  `SIGN_TOOL_CMD` hook in `script/windows/bundle.ps1` if the installer will be
  shared broadly.

## Validation Checklist

Run on a Windows builder or teammate machine:

```powershell
.\script\windows\bootstrap.ps1
.\script\windows\bundle.ps1 -CHANNEL warpoca -ARCH x64
```

Then verify:

- `WarpOCASetup.exe` installs without requiring admin rights.
- Launching WarpOCA does not open Warp sign-in.
- `byob_proxy.exe` starts automatically and listens on `127.0.0.1:1337`.
- Agent Mode reads `%USERPROFILE%\.codex\auth.json` after `codex login`.
- Next Command works for a known case such as `brew search nvtop` equivalent on
  Windows, for example `winget search ripgrep` to `winget install BurntSushi.ripgrep.MSVC`.
- Rules created in the UI are written under `%APPDATA%\WarpOCA\rules`.
- Markdown files under `%APPDATA%\WarpOCA\knowledge` are included in agent
  context.

## Risk

This should be moderate work, not a rewrite. The main uncertainty is whether the
current Warp Windows UI path compiles cleanly with the same feature set as the
macOS OSS build. The proxy itself is portable Rust; most of the work is path
handling, installer wiring, helper process startup, and Windows-specific smoke
testing.
