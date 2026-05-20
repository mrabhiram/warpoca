# WarpOCA Windows Build Handoff

Use this on a Windows machine with Codex, Visual Studio Build Tools, and Inno
Setup available. The generated installer is the file to send to a teammate for
testing.

## Build Command

From a PowerShell terminal in the repo:

```powershell
.\script\windows\package_warpoca.ps1 -ARCH x64
```

The expected output is:

```text
script\windows\Output\WarpOCASetup.exe
```

For ARM64 Windows:

```powershell
.\script\windows\package_warpoca.ps1 -ARCH arm64
```

The expected output is:

```text
script\windows\Output\WarpOCASetup-arm64.exe
```

## Codex Prompt For A Windows Builder

Paste this into Codex on a Windows machine after cloning the repo:

```text
You are in the WarpOCA repo on Windows. Build the Windows installer for WarpOCA.

Run:
.\script\windows\package_warpoca.ps1 -ARCH x64

If bootstrap asks to install prerequisites, allow it. When the build completes,
verify that script\windows\Output\WarpOCASetup.exe exists. Then launch the
installer, install WarpOCA, confirm the app opens without Warp sign-in, confirm
the bundled helper at {app}\Helpers\byob_proxy.exe starts automatically on
127.0.0.1:1337, and confirm Agent Mode uses Codex auth from
%USERPROFILE%\.codex\auth.json.

If the build fails, summarize the first real compiler, linker, MSVC, Windows
SDK, or Inno Setup error and do not change unrelated code.
```

## Tester Checklist

1. Install Codex CLI and run `codex login`.
2. Confirm `%USERPROFILE%\.codex\auth.json` exists.
3. Install `WarpOCASetup.exe`.
4. Launch WarpOCA.
5. Confirm it does not open Warp authentication.
6. In PowerShell, run:

```powershell
netstat -ano | findstr 1337
```

7. In WarpOCA, run `/agent who are you?`.
8. Create a rule in the UI and confirm a Markdown file appears under:

```text
%APPDATA%\WarpOCA\rules
```

## Notes

- A Mac cannot produce this installer with the current MSVC build path. The
  Windows target needs Visual Studio Build Tools and the Windows SDK because
  native dependencies such as `aws-lc-sys` include Windows headers like
  `windows.h`.
- The Windows installer channel is `warpoca`.
- The app binary is installed as `WarpOCA.exe`.
- The proxy helper is installed as `Helpers\byob_proxy.exe`.
- The CLI wrapper installed into PATH is `warpoca.cmd`.
