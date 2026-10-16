# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
# Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

# release.ps1 - build and run the repository copy without changing another copy's startup.
#
# The app and RAW macros must not start elevated from a user-writable build directory. The old
# task name is recognized only so this script can migrate it to the current `Neuron` task.
#
# Rebuild the release binaries in Cargo's selected target directory, then use an enabled task
# or launch directly when autostart is off. Never create an autostart task just to run a release
# build.
#
# Usage:
#   .\release.ps1                 # build release, restart the tray instance
#   .\release.ps1 -SkipBuild      # just restart the tray from the existing release binary
#   .\release.ps1 -NoRelaunch     # build, but leave the app closed
#   .\release.ps1 -UseAtStartup   # explicitly make this build the Windows startup copy

[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [switch]$NoRelaunch,
    [switch]$UseAtStartup
)

$ErrorActionPreference = 'Stop'
$RepoRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$TargetRoot = if ($env:CARGO_TARGET_DIR) {
    if ([IO.Path]::IsPathRooted($env:CARGO_TARGET_DIR)) {
        [IO.Path]::GetFullPath($env:CARGO_TARGET_DIR)
    } else {
        [IO.Path]::GetFullPath((Join-Path $RepoRoot $env:CARGO_TARGET_DIR))
    }
} else {
    Join-Path $RepoRoot 'target'
}
$ReleaseDir = Join-Path $TargetRoot 'release'
$AppExe   = Join-Path $ReleaseDir 'neuron-app.exe'
$TaskName = 'Neuron'
$LegacyTaskName = 'Neuron (elevated tray)'

function Write-Step($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }

function Get-StartupTask {
    $current = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    $legacy = Get-ScheduledTask -TaskName $LegacyTaskName -ErrorAction SilentlyContinue
    if (Test-EnabledLogonTask $current) { return $current }
    if (Test-EnabledLogonTask $legacy) { return $legacy }
    if ($current) { return $current }
    return $legacy
}

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

function Get-ReleaseLaunchMode($task, $exe, $useAtStartup) {
    if ($useAtStartup) { return 'register' }
    if (Test-LimitedStartupTask $task $exe) { return 'task' }
    return 'direct'
}

function Get-ReleaseProcesses {
    foreach ($process in (Get-Process neuron-app, neuron -ErrorAction SilentlyContinue)) {
        if (-not $NoRelaunch -or ($process.Path -and (Split-Path -Parent $process.Path) -ieq $ReleaseDir)) {
            $process
        }
    }
}

function Get-ReleaseTrayProcesses {
    Get-ReleaseProcesses | Where-Object { $_.ProcessName -eq 'neuron-app' }
}

function Signal-ReleaseTrayProcesses($processes) {
    foreach ($process in $processes) {
        if (-not $process.Path) { continue }
        try {
            $current = Get-Process -Id $process.Id -ErrorAction Stop
            if (-not $current.Path -or (Split-Path -Parent $current.Path) -ine (Split-Path -Parent $process.Path)) { continue }
            $eventName = 'Local\WofloLabs.Neuron.Shutdown.' + $process.Id
            $shutdown = [Threading.EventWaitHandle]::OpenExisting($eventName)
            try { $shutdown.Set() | Out-Null } finally { $shutdown.Dispose() }
        } catch {
            # Older app builds have no per-process event; the selected-path timeout below handles them.
        }
    }
}

if ($UseAtStartup -and $NoRelaunch) { throw '-UseAtStartup and -NoRelaunch cannot be combined' }

# Refuse before stopping the resident app: a direct launch from an administrator shell would
# inherit that shell's elevated token even though the autostart task is absent or disabled.
if (-not $NoRelaunch -and (Test-CurrentProcessElevated)) {
    $preflightTask = Get-StartupTask
    if (-not (Test-LimitedStartupTask $preflightTask $AppExe)) {
        throw 'run release.ps1 from a normal PowerShell; the existing startup task does not safely launch this build'
    }
}

# 1. Signal selected tray processes and wait before relinking their executable. Startup tasks
#    remain registered; only the exact build selected for this run is stopped in -NoRelaunch mode.
Write-Step 'Stopping any running Neuron instances'
if (Get-ReleaseProcesses) {
    # Current builds have one scoped event per tray PID. If an older build has no event, the
    # bounded wait and path-selected process fallback below still unlock only selected binaries.
    $trayProcesses = @(Get-ReleaseTrayProcesses)
    Signal-ReleaseTrayProcesses $trayProcesses
    $deadline = (Get-Date).AddSeconds(3)
    while ((Get-Date) -lt $deadline -and (Get-ReleaseTrayProcesses)) {
        Start-Sleep -Milliseconds 100
    }
    $deadline = (Get-Date).AddSeconds(5)
    while ((Get-Date) -lt $deadline -and (Get-ReleaseProcesses)) {
        Start-Sleep -Milliseconds 250
    }
    $running = Get-ReleaseProcesses
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
    Write-Step 'Building release binaries (broker + app + CLI)'
    Push-Location $RepoRoot
    # Cargo reports progress and warnings on stderr. Under 'Stop', Windows PowerShell turns each
    # redirected stderr line into a terminating error, so a logged build died on its first warning;
    # the exit code is the verdict.
    $previousPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & cargo build --release -p neuron-host --features bridge --bin neuron-chroma-broker
        if ($LASTEXITCODE -ne 0) { throw "broker build failed (exit $LASTEXITCODE)" }
        $previousBrokerHash = $env:NEURON_BROKER_SHA256
        $previousInstallerHash = $env:NEURON_BROKER_INSTALLER_SHA256
        $env:NEURON_BROKER_SHA256 = (Get-FileHash -LiteralPath (Join-Path $ReleaseDir 'neuron-chroma-broker.exe') -Algorithm SHA256).Hash
        $env:NEURON_BROKER_INSTALLER_SHA256 = (Get-FileHash -LiteralPath (Join-Path $RepoRoot 'packaging\windows\install-chroma-broker.ps1') -Algorithm SHA256).Hash
        & cargo build --release -p neuron-app -p neuron-cli
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit $LASTEXITCODE)" }
    } finally {
        $ErrorActionPreference = $previousPreference
        if ($null -eq $previousBrokerHash) { Remove-Item Env:NEURON_BROKER_SHA256 -ErrorAction SilentlyContinue }
        else { $env:NEURON_BROKER_SHA256 = $previousBrokerHash }
        if ($null -eq $previousInstallerHash) { Remove-Item Env:NEURON_BROKER_INSTALLER_SHA256 -ErrorAction SilentlyContinue }
        else { $env:NEURON_BROKER_INSTALLER_SHA256 = $previousInstallerHash }
        Pop-Location
    }
} else {
    Write-Step 'Skipping build (-SkipBuild)'
}

if (-not (Test-Path $AppExe)) { throw "release binary not found at $AppExe" }

# 3. A one-session developer run does not claim another copy's startup task.
if (-not $NoRelaunch) {
    $task = Get-StartupTask
    $launchMode = Get-ReleaseLaunchMode $task $AppExe $UseAtStartup
    if ($launchMode -eq 'register') {
        $user = [Security.Principal.WindowsIdentity]::GetCurrent().Name
        $dir = Split-Path -Parent $AppExe
        if ($task.TaskName -ne $TaskName -or -not (Test-LimitedStartupTask $task $AppExe)) {
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
        if (Get-ScheduledTask -TaskName $LegacyTaskName -ErrorAction SilentlyContinue) {
            Unregister-ScheduledTask -TaskName $LegacyTaskName -Confirm:$false -ErrorAction Stop
        }
    }
    if ($launchMode -in @('task', 'register')) {
        Write-Step "Launching via limited scheduled task '$TaskName'"
        Start-ScheduledTask -TaskName $TaskName -ErrorAction Stop
    } else {
        if (Test-CurrentProcessElevated) {
            throw 'autostart is off; run release.ps1 from a normal PowerShell to relaunch Neuron without elevation'
        }
        Write-Step 'Launching repository build for this session (startup target preserved)'
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
