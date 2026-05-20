#!/usr/bin/env powershell
#
# Build the internal WarpOCA Windows installer.

Param (
    [ValidateSet('x64', 'arm64')]
    [String]$ARCH = '',

    [Alias('skip-bootstrap')]
    [Switch]$SKIP_BOOTSTRAP = $False,

    [Alias('sign-tool-cmd')]
    [String]$SIGN_TOOL_CMD = ''
)

$ErrorActionPreference = 'Stop'
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
Set-Location $RepoRoot

if (-not $SKIP_BOOTSTRAP) {
    & "$PSScriptRoot\bootstrap.ps1"
}

$BundleArgs = @(
    '-CHANNEL', 'warpoca'
)

if ($ARCH) {
    $BundleArgs += @('-ARCH', $ARCH)
}

if ($SIGN_TOOL_CMD) {
    $BundleArgs += @('-sign-tool-cmd', $SIGN_TOOL_CMD)
}

& "$PSScriptRoot\bundle.ps1" @BundleArgs
