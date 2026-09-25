# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
# Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

# release.ps1 - build Neuron in release mode and make it the resident (startup) instance.
#
# Neuron's historical task name is "Neuron (elevated tray)", but its run level is Limited.
# The app and RAW macros must not start elevated from a user-writable build directory.
#
# Rebuild target\release\neuron-app.exe, then use an enabled task or launch directly when
# autostart is off. Never create an autostart task just to run a release build.
#
# Usage:
#   .\release.ps1                 # build release, restart the tray instance
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

function Test-EnabledLogonTask($task) {
    if (-not $task -or -not $task.Settings.Enabled) { return $false }
    return @($task.Triggers | Where-Object {
        $_.CimClass.CimClassName -eq 'MSFT_TaskLogonTrigger' -and $_.Enabled
    }).Count -gt 0
}

function Test-CurrentProcessElevated {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Test-LimitedStartupTask($task, $exe) {
    if (-not (Test-EnabledLogonTask $task)) { return $false }
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [string]$task.Principal.UserId
    try {
        $principalSid = if ($principal -match '^S-1-') { $principal } else {
            ([Security.Principal.NTAccount]$principal).Translate([Security.Principal.SecurityIdentifier]).Value
        }
    } catch { return $false }
    return @($task.Actions).Count -eq 1 -and
        $task.Actions[0].Execute -ieq $exe -and
        $task.Actions[0].Arguments -eq '--tray' -and
        $task.Actions[0].WorkingDirectory -ieq (Split-Path -Parent $exe) -and
        $task.Principal.RunLevel -eq 'Limited' -and
        $task.Principal.LogonType -eq 'Interactive' -and
        $principalSid -eq $identity.User.Value -and
        $task.Settings.ExecutionTimeLimit -eq 'PT0S'
}

# Refuse before stopping the resident app: a direct launch from an administrator shell would
# inherit that shell's elevated token even though the autostart task is absent or disabled.
if (-not $NoRelaunch -and (Test-CurrentProcessElevated)) {
    $preflightTask = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    if (-not (Test-EnabledLogonTask $preflightTask)) {
        throw 'autostart is off; run release.ps1 from a normal PowerShell to relaunch Neuron without elevation'
    }
}

# 1. Stop any running instance through its task when available, then any strays directly.
#    The exe must be unlocked or the link step fails. Teardown is asynchronous, so poll
#    for exit instead of guessing a sleep.
Write-Step 'Stopping any running Neuron instances'
if (Get-Process neuron-app, neuron -ErrorAction SilentlyContinue) {
    if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
        Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    }
    $deadline = (Get-Date).AddSeconds(5)
    while ((Get-Date) -lt $deadline -and (Get-Process neuron-app, neuron -ErrorAction SilentlyContinue)) {
        Start-Sleep -Milliseconds 250
    }
    $running = Get-Process neuron-app, neuron -ErrorAction SilentlyContinue
    if ($running) {
        # Not the task's instance (a debug or manual launch) - stop it directly.
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

# 3. Preserve the user's autostart setting. Repair an enabled task before using it, but launch
#    directly when no enabled logon task exists. Never run a stale elevated task.
if (-not $NoRelaunch) {
    $task = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    $useTask = Test-EnabledLogonTask $task
    if ($useTask) {
        $user = [Security.Principal.WindowsIdentity]::GetCurrent().Name
        $dir = Split-Path -Parent $AppExe
        if (-not (Test-LimitedStartupTask $task $AppExe)) {
            Write-Step 'Repairing the limited startup task'
            $action = New-ScheduledTaskAction -Execute $AppExe -Argument '--tray' -WorkingDirectory $dir
            $trigger = New-ScheduledTaskTrigger -AtLogOn -User $user
            $trigger.Delay = 'PT15S'
            $limited = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited
            $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit (New-TimeSpan -Seconds 0) -MultipleInstances IgnoreNew -StartWhenAvailable -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
            try {
                Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger -Principal $limited -Settings $settings -Force -ErrorAction Stop | Out-Null
            } catch {
                throw "could not replace the old startup task with a limited task: $_. Remove the old task with administrator permission, then retry."
            }
            $task = Get-ScheduledTask -TaskName $TaskName -ErrorAction Stop
            if (-not (Test-LimitedStartupTask $task $AppExe)) {
                throw 'startup task repair did not produce the expected limited task; refusing to launch it'
            }
        }
        Write-Step "Launching via limited scheduled task '$TaskName'"
        Start-ScheduledTask -TaskName $TaskName -ErrorAction Stop
    } else {
        if (Test-CurrentProcessElevated) {
            throw 'autostart is off; run release.ps1 from a normal PowerShell to relaunch Neuron without elevation'
        }
        if ($task) {
            # Manual mode no longer needs a dormant task as an elevation vehicle.
            try {
                Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false -ErrorAction Stop
            } catch {
                throw "could not remove the inactive legacy task: $_. Remove it with administrator permission, then retry."
            }
        }
        Write-Step 'Launching directly (autostart is off)'
        Start-Process -FilePath $AppExe -ArgumentList '--tray' -WorkingDirectory (Split-Path -Parent $AppExe)
    }
    # The launch is unelevated, so its executable path must be readable and match the build.
    $deadline = (Get-Date).AddSeconds(15)
    do {
        Start-Sleep -Milliseconds 500
        $p = Get-CimInstance Win32_Process -Filter "Name='neuron-app.exe'" -ErrorAction SilentlyContinue
    } until ($p -or ((Get-Date) -gt $deadline))
    if (-not $p) {
        throw 'no neuron-app process appeared after launch'
    }
    $foreign = @(@($p.ExecutablePath) | Where-Object { -not $_ -or $_ -ine $AppExe })
    if ($foreign.Count -gt 0) {
        throw "could not verify the release process path: $($foreign -join ', ')"
    }
    Write-Host "    running (pid $(($p.ProcessId) -join ', ')) from the release build"
} else {
    Write-Step 'Not relaunching (-NoRelaunch)'
}

Write-Step 'Done.'
