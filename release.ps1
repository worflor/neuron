# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
# Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

# release.ps1 - build Neuron in release mode and make it the resident (startup) instance.
#
# Neuron runs as a tray-resident app launched at login by the Scheduled Task
# "Neuron (elevated tray)" (RunLevel Highest). Elevation is load-bearing: the native Chroma
# SHM server creates Global\ shared-memory objects, which need SeCreateGlobalPrivilege - an
# unelevated instance silently degrades to REST-only. Never recreate the old HKCU "Run" key
# launcher.
#
# The task already points at target\release\neuron-app.exe, so "release" = rebuild that file
# and bounce the task. Stopping also goes through the task: an unelevated shell's
# Stop-Process gets Access Denied against the elevated instance.
#
# Usage:
#   .\release.ps1                 # build release, restart the elevated tray instance
#   .\release.ps1 -SkipBuild      # just restart the tray from the existing release binary
#   .\release.ps1 -NoRelaunch     # build, but leave the app closed

[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [switch]$NoRelaunch
)

$ErrorActionPreference = 'Stop'
$RepoRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$AppExe   = Join-Path $RepoRoot 'target\release\neuron-app.exe'
$TaskName = 'Neuron (elevated tray)'

function Write-Step($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }

# 1. Stop any running instance - the elevated one via its task (the only handle an
#    unelevated shell has on it), then any stray debug/unelevated ones directly.
#    The exe must be unlocked or the link step fails. Teardown is asynchronous, so poll
#    for exit instead of guessing a sleep.
Write-Step 'Stopping any running Neuron instances'
if (Get-Process neuron-app, neuron -ErrorAction SilentlyContinue) {
    schtasks /end /tn $TaskName | Out-Null
    $deadline = (Get-Date).AddSeconds(5)
    while ((Get-Date) -lt $deadline -and (Get-Process neuron-app, neuron -ErrorAction SilentlyContinue)) {
        Start-Sleep -Milliseconds 250
    }
    $running = Get-Process neuron-app, neuron -ErrorAction SilentlyContinue
    if ($running) {
        # not the task's instance (a debug/unelevated stray) - stop it directly
        try { $running | Stop-Process -Force -ErrorAction Stop } catch {
            throw "a Neuron instance (pid $($running.Id -join ', ')) won't die from this shell - close it manually, then re-run"
        }
        Start-Sleep -Milliseconds 400   # let the OS release the file lock before we relink
        Write-Host "    stopped directly: $($running.Id -join ', ')"
    } else {
        Write-Host '    stopped via task'
    }
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

# 3. Relaunch the resident instance through the task so it comes back elevated.
#    The task's target is verified on both sides of the launch: the app itself can re-register
#    the task from its own exe path (the launch-mode selector), so a debug/dev instance may
#    have re-pointed it. Command check before, executable-path check after; fail loud on both.
if (-not $NoRelaunch) {
    Write-Step "Launching via scheduled task '$TaskName'"
    # no stderr redirect: under EAP=Stop, PS 5.1 wraps redirected native stderr into a
    # terminating NativeCommandError, which would mask the honest handling below.
    $taskXml = schtasks /query /tn $TaskName /xml | Out-String
    $needsRepair = $true
    $hadLogon = $true   # a missing task is recreated WITH autostart (the historic default)
    if ($taskXml -match '<Command>([^<]+)</Command>') {
        $target = [System.Net.WebUtility]::HtmlDecode($Matches[1]).Trim()
        $hadLogon = $taskXml -match '<LogonTrigger>'
        $needsRepair = ($target -ne $AppExe)
        if ($needsRepair) { Write-Host "    task points at '$target' - repointing at the release build" }
    } else {
        Write-Host "    task missing - creating it for the release build"
    }
    if ($needsRepair) {
        # Re-register at $AppExe, preserving the logon-trigger (autostart) state the task had.
        Write-Step 'Repairing the startup task'
        # XML-escape everything interpolated: Windows paths and account names can legally
        # contain '&' and friends, which would corrupt the task definition.
        $xUser = [System.Security.SecurityElement]::Escape("$env:USERDOMAIN\$env:USERNAME")
        $xExe  = [System.Security.SecurityElement]::Escape($AppExe)
        $xWork = [System.Security.SecurityElement]::Escape("$RepoRoot\target\release")
        $trigger = if ($hadLogon) {
            "  <Triggers>`n    <LogonTrigger>`n      <Delay>PT15S</Delay>`n      <UserId>$xUser</UserId>`n    </LogonTrigger>`n  </Triggers>"
        } else { '  <Triggers />' }
        $repairXml = @"
<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.3" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <URI>\$TaskName</URI>
  </RegistrationInfo>
  <Principals>
    <Principal id="Author">
      <UserId>$xUser</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <StartWhenAvailable>true</StartWhenAvailable>
    <UseUnifiedSchedulingEngine>true</UseUnifiedSchedulingEngine>
  </Settings>
$trigger
  <Actions Context="Author">
    <Exec>
      <Command>$xExe</Command>
      <Arguments>--tray</Arguments>
      <WorkingDirectory>$xWork</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"@
        $xmlPath = Join-Path $env:TEMP 'neuron-release-task.xml'
        $repairXml | Out-File $xmlPath -Encoding Unicode
        schtasks /create /tn $TaskName /xml $xmlPath /f | Out-Null
        if ($LASTEXITCODE -ne 0) {
            # HighestAvailable registration can need admin - one consented UAC elevation.
            Write-Host '    unelevated registration refused - requesting elevation (UAC)'
            Start-Process schtasks -ArgumentList '/create', '/tn', "`"$TaskName`"", '/xml', "`"$xmlPath`"", '/f' -Verb RunAs -Wait
        }
        Remove-Item $xmlPath -Force -ErrorAction SilentlyContinue
        $taskXml = schtasks /query /tn $TaskName /xml | Out-String
        if (-not ($taskXml -match '<Command>([^<]+)</Command>') -or
            ([System.Net.WebUtility]::HtmlDecode($Matches[1]).Trim() -ne $AppExe)) {
            throw "could not repoint the startup task at the release build - fix it manually: schtasks /query /tn `"$TaskName`" /v"
        }
        Write-Host '    task repaired'
    }
    schtasks /run /tn $TaskName | Out-Null
    # Poll for the spawn - task-scheduler latency varies. The resident-is-release invariant is
    # proven by the chain, not by reading the elevated process's path (Get-Process .Path and
    # WMI ExecutablePath both come back empty across the elevation boundary): every instance
    # was stopped above, the task's <Command> was verified/repaired to $AppExe, and a process
    # appeared after /run. Readable paths do exist for unelevated strays; those fail loud.
    $deadline = (Get-Date).AddSeconds(15)
    do {
        Start-Sleep -Milliseconds 500
        $p = Get-CimInstance Win32_Process -Filter "Name='neuron-app.exe'" -ErrorAction SilentlyContinue
    } until ($p -or ((Get-Date) -gt $deadline))
    if (-not $p) {
        throw "task ran but no neuron-app process appeared - check the task with: schtasks /query /tn `"$TaskName`" /v"
    }
    $foreign = @(@($p.ExecutablePath) | Where-Object { $_ -and ($_ -ne $AppExe) })
    if ($foreign.Count -gt 0) {
        throw "another neuron-app is running from '$($foreign -join ', ')' beside the release build - kill it and re-run"
    }
    Write-Host "    running (pid $(($p.ProcessId) -join ', ')) via the verified task target"
} else {
    Write-Step 'Not relaunching (-NoRelaunch)'
}

Write-Step 'Done.'
