# release.ps1 — build Neuron in release mode and make it the resident (startup) instance.
#
# Neuron runs as a tray-resident app launched at login via the HKCU "Run" key. During
# development that key can end up pointing at target\debug (a stale, unoptimized build).
# This script builds the optimized release binaries, repoints the login entry at the
# release exe, and restarts the tray instance so "the one that's always running" is current.
#
# Usage:
#   .\release.ps1                 # build release, repoint startup -> release, relaunch tray
#   .\release.ps1 -SkipBuild      # just swap/relaunch using the existing release binary
#   .\release.ps1 -NoStartup      # build + relaunch, but DON'T touch the login (Run) key
#   .\release.ps1 -NoRelaunch     # build (and repoint) but leave the app closed

[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [switch]$NoStartup,
    [switch]$NoRelaunch
)

$ErrorActionPreference = 'Stop'
$RepoRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$AppExe   = Join-Path $RepoRoot 'target\release\neuron-app.exe'
$RunKey   = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$RunName  = 'Neuron'
$LaunchArgs = '--tray'

function Write-Step($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }

# 1. Stop any running instance. The debug and release exes are different files, but we want
#    exactly one resident instance afterward, so stop every neuron-app/neuron process first.
Write-Step 'Stopping any running Neuron instances'
$running = Get-Process neuron-app, neuron -ErrorAction SilentlyContinue
if ($running) {
    $running | Stop-Process -Force
    Start-Sleep -Milliseconds 400   # let the OS release the file lock before we relink
    Write-Host "    stopped: $($running.Id -join ', ')"
} else {
    Write-Host '    (none running)'
}

# 2. Build the optimized binaries. Both crates enable the verify-gated idle-power-write
#    feature in their own manifests, so a plain per-package release build is complete.
if (-not $SkipBuild) {
    Write-Step 'Building release binaries (neuron-app + neuron cli)'
    Push-Location $RepoRoot
    try {
        & cargo build --release -p neuron-app -p neuron-cli
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit $LASTEXITCODE)" }
    } finally {
        Pop-Location
    }
} else {
    Write-Step 'Skipping build (-SkipBuild)'
}

if (-not (Test-Path $AppExe)) { throw "release binary not found at $AppExe" }

# 3. Repoint the login entry at the release binary so the next boot uses it too.
if (-not $NoStartup) {
    Write-Step "Pointing startup ($RunName) at the release build"
    $desired = '"{0}" {1}' -f $AppExe, $LaunchArgs
    Set-ItemProperty -Path $RunKey -Name $RunName -Value $desired
    Write-Host "    $RunName = $desired"
} else {
    Write-Step 'Leaving startup (Run key) untouched (-NoStartup)'
}

# 4. Relaunch the tray instance from the fresh release binary.
if (-not $NoRelaunch) {
    Write-Step 'Launching the release tray instance'
    Start-Process -FilePath $AppExe -ArgumentList $LaunchArgs -WorkingDirectory $RepoRoot
    Write-Host '    launched.'
} else {
    Write-Step 'Not relaunching (-NoRelaunch)'
}

Write-Step 'Done.'
