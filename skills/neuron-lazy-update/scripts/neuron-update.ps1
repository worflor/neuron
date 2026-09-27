# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
# Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

# Install, update, or roll back a neuron release install on Windows.
# Deliberately ASCII-only and Windows PowerShell 5.1 compatible: that is what end users have.
#
# Output protocol, meant to be read by an agent:
#   key: value         facts
#   FLAG: CODE - text  something worth telling the user; codes marked STOP mean do not continue
#   RESULT: code       always the last line; see update.md for what each code means
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File neuron-update.ps1 -Action check
#   powershell -NoProfile -ExecutionPolicy Bypass -File neuron-update.ps1 -Action apply
#   powershell -NoProfile -ExecutionPolicy Bypass -File neuron-update.ps1 -Action rollback
#
# It never deletes anything in the install folder. It overwrites only the files a release
# ships, and backs those up first.

[CmdletBinding()]
param(
    [ValidateSet('check', 'apply', 'rollback')]
    [string]$Action = 'check',
    [string]$InstallDir,
    [string]$Version,
    [string]$ZipPath,
    [string]$SumsPath,
    [switch]$NoRelaunch,
    [switch]$AllowDowngrade
)

$ErrorActionPreference = 'Stop'
$Repo = 'worflor/neuron'
$Tasks = @('Neuron', 'Neuron (elevated tray)')
$BackupRoot = '.neuron-update-backup'
$DefaultDir = Join-Path $env:LOCALAPPDATA 'Programs\neuron'
$ReleaseViaGh = $false
$CurrentUserSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value

function Say($k, $v) { Write-Output ("{0}: {1}" -f $k, $v) }
function Flag($code, $text) { Write-Output ("FLAG: {0} - {1}" -f $code, $text) }
function Finish($code) {
    Write-Output ("RESULT: {0}" -f $code)
    if ($code -in @('blocked', 'error', 'needs-admin')) { exit 1 }
    exit 0
}

# "v1.2.3-rc1" / "neuron 1.2.3" -> [version]1.2.3
function Parse-Version($s) {
    if ($s -match '(\d+)\.(\d+)\.(\d+)') { return [version]("{0}.{1}.{2}" -f $Matches[1], $Matches[2], $Matches[3]) }
    return $null
}

# Native commands write to stderr on ordinary misses (no such task); under 'Stop' that throws in 5.1.
function Get-TaskExeDirs {
    foreach ($taskName in $Tasks) {
        $ErrorActionPreference = 'Continue'
        $xml = schtasks /query /tn $taskName /xml 2>$null | Out-String
        if ($LASTEXITCODE -eq 0 -and $xml -match '<Command>([^<]+)</Command>') {
            Split-Path -Parent $Matches[1]
        }
    }
}

function Get-InstalledVersion($dir) {
    $ErrorActionPreference = 'Continue'
    $src = Join-Path $dir 'SOURCE.txt'
    if (Test-Path $src) {
        $first = Get-Content $src -TotalCount 1
        if ($first -match 'neuron (v\S+)') { return $Matches[1] }
    }
    $cli = Join-Path $dir 'neuron.exe'
    if (Test-Path $cli) {
        $out = & $cli --version 2>$null | Out-String
        $v = Parse-Version $out
        if ($v) { return "v$v" }
    }
    return $null
}

function Test-SourceBuild($dir) {
    $p = $dir
    while ($p) {
        if ((Split-Path -Leaf $p) -eq 'target' -and (Test-Path (Join-Path (Split-Path -Parent $p) 'Cargo.toml'))) { return $true }
        $parent = Split-Path -Parent $p
        if ($parent -eq $p) { break }
        $p = $parent
    }
    return $false
}

function Test-Writable($dir) {
    try {
        if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }
        $probe = Join-Path $dir (".write-test-" + [guid]::NewGuid())
        Set-Content -Path $probe -Value 'x'
        Remove-Item $probe -Force
        return $true
    } catch { return $false }
}

function Same-Dir($a, $b) {
    if (-not $a -or -not $b) { return $false }
    return ([IO.Path]::GetFullPath($a).TrimEnd('\') -ieq [IO.Path]::GetFullPath($b).TrimEnd('\'))
}

# Copies every file under $from into $to, preserving relative paths, overwriting.
# Explicit per-file copy: Copy-Item -Recurse nests directories that already exist.
function Copy-Tree($from, $to) {
    $root = [IO.Path]::GetFullPath($from).TrimEnd('\')
    Get-ChildItem -Path $root -Recurse -File -Force | ForEach-Object {
        $rel = $_.FullName.Substring($root.Length).TrimStart('\')
        $dest = Join-Path $to $rel
        $parent = Split-Path -Parent $dest
        if (-not (Test-Path $parent)) { New-Item -ItemType Directory -Path $parent -Force | Out-Null }
        Copy-Item -LiteralPath $_.FullName -Destination $dest -Force
    }
}

function Get-ZipEntrySha256($zipPath, $leafName) {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [IO.Compression.ZipFile]::OpenRead($zipPath)
    try {
        $entries = @($archive.Entries | Where-Object {
            $_.Name -eq $leafName -and $_.FullName -notmatch '(^|/)\.\.?(/|$)'
        })
        if ($entries.Count -ne 1) { return $null }
        $stream = $entries[0].Open()
        $sha = [Security.Cryptography.SHA256]::Create()
        try {
            return [BitConverter]::ToString($sha.ComputeHash($stream)).Replace('-', '')
        } finally {
            $sha.Dispose()
            $stream.Dispose()
        }
    } finally {
        $archive.Dispose()
    }
}

function Get-PublishedHash($sumsPath, $fileName) {
    foreach ($line in (Get-Content -LiteralPath $sumsPath)) {
        $parts = ($line -replace [char]0xFEFF, '').Trim() -split '\s+'
        if ($parts.Count -ge 2 -and $parts[-1] -eq $fileName) {
            return $parts[0].ToLower()
        }
    }
    return $null
}

function Get-NeuronProcesses($dir) {
    $list = @()
    foreach ($p in (Get-Process -Name 'neuron-app', 'neuron' -ErrorAction SilentlyContinue)) {
        $path = $null
        try { $path = $p.Path } catch { }
        $list += [pscustomobject]@{
            Id      = $p.Id
            Name    = $p.ProcessName
            Path    = $path
            InDir   = ($path -and (Same-Dir (Split-Path -Parent $path) $dir))
            Unknown = (-not $path)
        }
    }
    return , $list
}

# A running exe is locked against writes. This is the ground truth for "running from $dir":
# process paths read empty for elevated instances, so Get-Process alone can't tell.
function Test-Locked($path) {
    if (-not (Test-Path -LiteralPath $path)) { return $false }
    try {
        $fs = [IO.File]::Open($path, 'Open', 'ReadWrite', 'None')
        $fs.Close()
        return $false
    } catch { return $true }
}

function Test-Running($dir) {
    return ((Test-Locked (Join-Path $dir 'neuron-app.exe')) -or (Test-Locked (Join-Path $dir 'neuron.exe')))
}

function Test-CurrentProcessElevated {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Quote-PowerShellLiteral($value) {
    return "'" + ([string]$value).Replace("'", "''") + "'"
}

function Get-OptionalScheduledTask($name) {
    try {
        return Get-ScheduledTask -TaskName $name -ErrorAction Stop
    } catch {
        if ($_.CategoryInfo.Category -eq 'ObjectNotFound') { return $null }
        throw
    }
}

# An older release registered the user-writable tray executable at HighestAvailable. A normal
# updater cannot reliably replace that task, so refuse the file update before stopping or copying
# anything. Its next logon would otherwise launch the new app and RAW macros elevated.
function Assert-SafeStartupTask {
    foreach ($taskName in $Tasks) {
        try {
            $taskDef = Get-ScheduledTask -TaskName $taskName -ErrorAction Stop
        } catch {
            if ($_.CategoryInfo.Category -eq 'ObjectNotFound') { continue }
            Flag 'STARTUP_TASK_QUERY_FAILED' "cannot verify the Neuron startup task '$taskName': $($_.Exception.Message)"
            Finish 'blocked'
        }
        if ($taskDef.Principal.RunLevel -ne 'Limited') {
            Flag 'UNSAFE_STARTUP_TASK' "the startup task '$taskName' can run Neuron elevated. Replace it with a Limited task or remove it from an administrator PowerShell before updating; then rerun this updater from a normal PowerShell."
            Finish 'needs-admin'
        }
    }
}

function Assert-ChromaBrokerOwner {
    $programFiles64 = if ($env:ProgramW6432) { $env:ProgramW6432 } else { $env:ProgramFiles }
    $brokerDir = Join-Path $programFiles64 'NeuronChromaBroker'
    $receipt = Join-Path $brokerDir 'broker-installed.sha256'
    $pending = Join-Path $brokerDir 'broker-install.pending'
    $receiptLines = if (Test-Path -LiteralPath $receipt -PathType Leaf) {
        @(Get-Content -LiteralPath $receipt -TotalCount 5)
    } else { @() }
    $pendingLines = if (Test-Path -LiteralPath $pending -PathType Leaf) {
        @(Get-Content -LiteralPath $pending -TotalCount 5)
    } else { @() }
    $valid = {
        param($lines)
        return ($lines.Count -eq 4 -and
            $lines[0].Trim() -match '^[0-9a-fA-F]{64}$' -and
            $lines[1].Trim() -eq 'SYSTEM startup' -and
            $lines[2].Trim() -eq 'task-acl-v1' -and
            [bool]$lines[3].Trim())
    }
    $record = if (& $valid $receiptLines) {
        @{ Path = $receipt; Lines = $receiptLines }
    } elseif (& $valid $pendingLines) {
        @{ Path = $pending; Lines = $pendingLines }
    } else { $null }
    if (-not $record) {
        $task = Get-OptionalScheduledTask 'Neuron Chroma broker'
        if ((Test-Path -LiteralPath $brokerDir) -or $task) {
            Flag 'CHROMA_BROKER_OWNER_UNKNOWN' 'existing machine-wide Chroma broker state has no ownership receipt; repair or remove it from an administrator PowerShell before updating'
            Finish 'blocked'
        }
        return
    }
    if ($record.Lines[3].Trim() -ine $CurrentUserSid) {
        Flag 'CHROMA_BROKER_OTHER_USER' 'the machine-wide Chroma broker belongs to another Windows user; use that account to update or uninstall Neuron'
        Finish 'blocked'
    }
}

function Ensure-ChromaBroker($packageDir, $expectedSourceHash, $expectedScriptHash) {
    $source = Join-Path $packageDir 'neuron-chroma-broker.exe'
    $script = Join-Path $packageDir 'install-chroma-broker.ps1'
    $programFiles64 = if ($env:ProgramW6432) { $env:ProgramW6432 } else { $env:ProgramFiles }
    $protected = Join-Path $programFiles64 'NeuronChromaBroker\neuron-chroma-broker.exe'
    $receipt = Join-Path $programFiles64 'NeuronChromaBroker\broker-installed.sha256'
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) { return }
    if (-not (Test-Path -LiteralPath $script -PathType Leaf)) {
        Flag 'CHROMA_BROKER_SETUP_FAILED' 'the package has a broker but no installer script'
        return
    }
    try {
        $sourceHash = (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash
        $scriptHash = (Get-FileHash -LiteralPath $script -Algorithm SHA256).Hash
        if ($sourceHash -ine $expectedSourceHash -or $scriptHash -ine $expectedScriptHash) {
            Flag 'CHROMA_BROKER_SETUP_FAILED' 'the extracted broker setup files differ from the verified release archive'
            return
        }
        $receiptLines = if (Test-Path -LiteralPath $receipt -PathType Leaf) {
            @(Get-Content -LiteralPath $receipt -TotalCount 4)
        } else { @() }
        if ((Get-FileHash -LiteralPath $protected -Algorithm SHA256).Hash -ieq $sourceHash -and
            $receiptLines.Count -ge 4 -and
            $receiptLines[0].Trim() -ieq $sourceHash -and
            $receiptLines[1].Trim() -eq 'SYSTEM startup' -and
            $receiptLines[2].Trim() -eq 'task-acl-v1' -and
            $receiptLines[3].Trim() -ieq $CurrentUserSid -and
            (Test-ChromaBrokerTask $protected)) { return }

        # The elevated process reads the writable script once, hashes those exact bytes, then
        # executes that in-memory copy. A check followed by `-File` would leave a replacement race.
        $bootstrap = @(
            '$ErrorActionPreference=''Stop'''
            ('$path=' + (Quote-PowerShellLiteral $script))
            ('$expected=' + (Quote-PowerShellLiteral $expectedScriptHash))
            '$bytes=[IO.File]::ReadAllBytes($path)'
            '$actual=[BitConverter]::ToString([Security.Cryptography.SHA256]::Create().ComputeHash($bytes)).Replace(''-'','''')'
            'if ($actual -ine $expected) { exit 86 }'
            '$code=[Text.Encoding]::UTF8.GetString($bytes)'
            ('&([ScriptBlock]::Create($code)) -BinaryPath ' + (Quote-PowerShellLiteral $source) +
                ' -ExpectedSha256 ' + $expectedSourceHash +
                ' -OwnerSid ' + (Quote-PowerShellLiteral $CurrentUserSid))
        ) -join ';'
        $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($bootstrap))
        $arguments = "-NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand $encoded"
        $systemDir = if ([Environment]::Is64BitOperatingSystem -and -not [Environment]::Is64BitProcess) { 'SysWOW64' } else { 'System32' }
        $windowsDir = [Environment]::GetFolderPath([Environment+SpecialFolder]::Windows)
        $windowsPowerShell = Join-Path $windowsDir "$systemDir\WindowsPowerShell\v1.0\powershell.exe"
        $process = Start-Process -FilePath $windowsPowerShell -ArgumentList $arguments -Verb RunAs -WindowStyle Hidden -Wait -PassThru -ErrorAction Stop
        $installedReceipt = if (Test-Path -LiteralPath $receipt -PathType Leaf) {
            @(Get-Content -LiteralPath $receipt -TotalCount 4)
        } else { @() }
        if ($process.ExitCode -ne 0 -or
            -not (Test-Path -LiteralPath $protected -PathType Leaf) -or
            (Get-FileHash -LiteralPath $protected -Algorithm SHA256).Hash -ine $expectedSourceHash -or
            $installedReceipt.Count -lt 4 -or
            $installedReceipt[0].Trim() -ine $expectedSourceHash -or
            $installedReceipt[1].Trim() -ne 'SYSTEM startup' -or
            $installedReceipt[2].Trim() -ne 'task-acl-v1' -or
            $installedReceipt[3].Trim() -ine $CurrentUserSid -or
            -not (Test-ChromaBrokerTask $protected)) {
            Flag 'CHROMA_BROKER_SETUP_FAILED' 'the broker was not installed; rerun the installer or updater to retry native Chroma setup'
        }
    } catch {
        Flag 'CHROMA_BROKER_SETUP_FAILED' "native Chroma setup was not completed: $($_.Exception.Message)"
    }
}

function Test-ChromaBrokerTask($expectedExe) {
    try {
        $task = Get-ScheduledTask -TaskName 'Neuron Chroma broker' -ErrorAction Stop
        $principal = [string]$task.Principal.UserId
        if ($principal -notin @('SYSTEM', 'S-1-5-18') -or
            $task.Principal.LogonType -ne 'ServiceAccount' -or
            $task.Principal.RunLevel -ne 'Highest' -or
            -not $task.Settings.Enabled -or
            $task.Actions.Count -ne 1 -or
            $task.Actions[0].Execute -ine $expectedExe -or
            $task.Actions[0].Arguments -or
            $task.Actions[0].WorkingDirectory -ine (Split-Path -Parent $expectedExe) -or
            $task.Triggers.Count -ne 1 -or
            $task.Triggers[0].CimClass.CimClassName -ne 'MSFT_TaskBootTrigger' -or
            -not (Test-ChromaBrokerTaskAcl)) { return $false }
        return $true
    } catch {
        return $false
    }
}

function Test-ChromaBrokerTaskAcl {
    try {
        $service = New-Object -ComObject 'Schedule.Service'
        $service.Connect()
        $sddl = $service.GetFolder('\').GetTask('Neuron Chroma broker').GetSecurityDescriptor(0x7)
        $descriptor = [Security.AccessControl.CommonSecurityDescriptor]::new($false, $false, $sddl)
        $protected = ($descriptor.ControlFlags -band
            [Security.AccessControl.ControlFlags]::DiscretionaryAclProtected) -ne 0
        if ($descriptor.Owner.Value -ne 'S-1-5-32-544' -or
            -not $protected -or
            -not $descriptor.DiscretionaryAcl.IsCanonical -or
            $descriptor.DiscretionaryAcl.Count -ne 3) { return $false }
        $rights = @{
            'S-1-5-18' = 0x001F01FF
            'S-1-5-32-544' = 0x001F01FF
            'S-1-5-32-545' = 0x00120089
        }
        foreach ($ace in $descriptor.DiscretionaryAcl) {
            $sid = $ace.SecurityIdentifier.Value
            if (-not $rights.ContainsKey($sid) -or
                $ace.AceQualifier -ne [Security.AccessControl.AceQualifier]::AccessAllowed -or
                $ace.AccessMask -ne $rights[$sid]) { return $false }
        }
        return $true
    } catch {
        return $false
    }
}

# Stops neuron running from $dir. Returns 'stopped', 'not-running', 'needs-admin' or 'unknown'.
function Stop-Neuron($dir, $taskDirs) {
    $ErrorActionPreference = 'Continue'
    if (-not (Test-Running $dir)) { return 'not-running' }

    # Current builds listen on a same-session event and quit through the tray path, which flushes
    # pending settings and returns devices to firmware ownership. Older builds have no event and
    # fall through to the bounded task/process stop below.
    try {
        $shutdown = [Threading.EventWaitHandle]::OpenExisting('Local\WofloLabs.Neuron.Shutdown')
        $shutdown.Set() | Out-Null
        $shutdown.Dispose()
        $deadline = (Get-Date).AddSeconds(3)
        while ((Get-Date) -lt $deadline -and (Test-Running $dir)) {
            Start-Sleep -Milliseconds 100
        }
        if (-not (Test-Running $dir)) { return 'stopped' }
    } catch { }

    # An elevated, task-launched instance can only be stopped through the task from an
    # unelevated shell.
    if ($taskDirs | Where-Object { Same-Dir $_ $dir }) {
        foreach ($taskName in $Tasks) { schtasks /end /tn $taskName 2>$null | Out-Null }
        Start-Sleep -Seconds 2
    }
    foreach ($p in ((Get-NeuronProcesses $dir) | Where-Object { $_.InDir })) {
        try { Stop-Process -Id $p.Id -Force -ErrorAction Stop } catch { return 'needs-admin' }
    }
    Start-Sleep -Seconds 1
    if (-not (Test-Running $dir)) { return 'stopped' }
    if ((Get-NeuronProcesses $dir) | Where-Object { $_.InDir }) { return 'needs-admin' }
    return 'unknown'
}

function Start-Neuron($dir, $taskDirs) {
    if ($taskDirs | Where-Object { Same-Dir $_ $dir }) {
        foreach ($taskName in $Tasks) {
            $taskDef = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
            if ($taskDef -and $taskDef.Principal.RunLevel -ne 'Limited') {
                Flag 'UNSAFE_STARTUP_TASK' "the autostart task '$taskName' can launch Neuron elevated; replace it with a Limited task or remove it before the next sign-in"
            }
        }
    }
    if (Test-CurrentProcessElevated) {
        Flag 'RELAUNCH_SKIPPED' 'the updater is elevated; start Neuron from a normal PowerShell so it does not inherit administrator privileges'
        return
    }
    if ($taskDirs | Where-Object { Same-Dir $_ $dir }) {
        Start-Process -FilePath (Join-Path $dir 'neuron-app.exe') -ArgumentList '--tray' -WorkingDirectory $dir
    } else {
        Start-Process -FilePath (Join-Path $dir 'neuron-app.exe') -WorkingDirectory $dir
    }
    Say 'relaunched' 'neuron-app.exe (limited token)'
}

function Get-Release($tag) {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    $route = if ($tag) { "repos/$Repo/releases/tags/$tag" } else { "repos/$Repo/releases?per_page=20" }
    $data = $null
    if (Get-Command gh -ErrorAction SilentlyContinue) {
        $ErrorActionPreference = 'Continue'
        $raw = & gh api $route 2>&1 | Out-String
        if ($LASTEXITCODE -eq 0) {
            try {
                $data = $raw | ConvertFrom-Json
                $script:ReleaseViaGh = $true
            } catch { }
        }
    }
    if (-not $data) {
        try {
            $data = Invoke-RestMethod -Uri "https://api.github.com/$route" -Headers @{ 'User-Agent' = 'neuron-lazy-update' } -TimeoutSec 30 -ErrorAction Stop
        } catch { return $null }
    }
    if ($tag) { return $data }
    return $data | Where-Object { -not $_.draft } | Select-Object -First 1
}

# ---------------------------------------------------------------------------------------------

$taskDirs = @(Get-TaskExeDirs | Sort-Object -Unique)
if (-not $InstallDir) {
    if ($taskDirs.Count -eq 1) { $InstallDir = $taskDirs[0] }
    elseif ($taskDirs.Count -gt 1) {
        Flag 'TASK_AMBIGUOUS' "Neuron autostart tasks point at more than one folder: $($taskDirs -join ', ')"
        Finish 'blocked'
    }
    elseif (Test-Path (Join-Path $DefaultDir 'neuron-app.exe')) { $InstallDir = $DefaultDir }
    else { $InstallDir = $DefaultDir }
}
$InstallDir = [IO.Path]::GetFullPath($InstallDir)
$installed = (Test-Path (Join-Path $InstallDir 'neuron-app.exe'))
$installerManaged = $installed -and (Test-Path (Join-Path $InstallDir 'unins000.exe') -PathType Leaf)

Say 'install_dir' $InstallDir
Say 'installed' $installed
foreach ($taskDir in $taskDirs) {
    Say 'autostart_task_dir' $taskDir
    if (-not (Same-Dir $taskDir $InstallDir)) {
        Flag 'TASK_ELSEWHERE' "an autostart task launches a neuron in $taskDir, not this folder. Check which install the user means."
    }
}

$current = $null
if ($installed) {
    if (Test-SourceBuild $InstallDir) {
        Say 'kind' 'source build (inside a cargo target folder)'
        Flag 'SOURCE_BUILD' 'this is a developer build. Update it with git pull and a rebuild, not a release zip.'
        Finish 'source-build'
    }
    $current = Get-InstalledVersion $InstallDir
    Say 'installed_version' $(if ($current) { $current } else { 'unknown' })
    if (-not (Test-Path (Join-Path $InstallDir 'SOURCE.txt'))) {
        Flag 'NO_SOURCE_TXT' 'no SOURCE.txt, so this was not installed from a release zip. Version detection is best effort.'
    }
}

# The task check must precede rollback and apply: a warning after the files have been replaced
# cannot prevent the old elevated task from starting the replacement at the next sign-in.
if ($Action -ne 'check') { Assert-SafeStartupTask }
if ($Action -ne 'check') { Assert-ChromaBrokerOwner }

# ---- rollback ------------------------------------------------------------------------------
if ($Action -eq 'rollback') {
    if ($installerManaged) {
        Flag 'INSTALLER_ROLLBACK_REQUIRED' 'this copy is managed by Windows Setup; reinstall the wanted setup version so its uninstall and broker lifecycle stay consistent'
        Finish 'blocked'
    }
    $bdir = Join-Path $InstallDir $BackupRoot
    $backups = @(Get-ChildItem -LiteralPath $bdir -Directory -Force -ErrorAction SilentlyContinue)
    $datedBackups = foreach ($candidate in $backups) {
        if ($candidate.Name -match '^.+-(?<stamp>\d{8}-\d{6})$') {
            $stamp = $Matches['stamp']
            $created = [datetime]::MinValue
            $validStamp = [datetime]::TryParseExact(
                $stamp,
                'yyyyMMdd-HHmmss',
                [Globalization.CultureInfo]::InvariantCulture,
                [Globalization.DateTimeStyles]::None,
                [ref]$created
            )
            if ($validStamp) {
                [pscustomobject]@{ Path = $candidate.FullName; Timestamp = $created }
            }
        }
    }
    $latest = @($datedBackups | Sort-Object Timestamp -Descending | Select-Object -First 1)
    if ($latest.Count -eq 0) { Flag 'NO_BACKUP' "no usable timestamped backup found in $bdir"; Finish 'blocked' }
    $latest = $latest[0]
    Say 'restoring' $latest.Path
    if (-not (Test-Writable $InstallDir)) { Flag 'NOT_WRITABLE' 'cannot write to the install folder'; Finish 'needs-admin' }
    $wasRunning = Test-Running $InstallDir
    $stop = Stop-Neuron $InstallDir $taskDirs
    if ($stop -eq 'needs-admin') { Flag 'CANNOT_STOP' 'neuron is running and could not be stopped from this shell'; Finish 'needs-admin' }
    if ($stop -eq 'unknown') { Flag 'STOP_UNCONFIRMED' 'a neuron process is still running and its location cannot be read. Ask the user to quit neuron from the tray, then rerun.'; Finish 'blocked' }
    Copy-Tree $latest.Path $InstallDir
    Say 'installed_version' (Get-InstalledVersion $InstallDir)
    if ($wasRunning -and -not $NoRelaunch) { Start-Neuron $InstallDir $taskDirs }
    Finish 'rolled-back'
}

# ---- resolve the target release ------------------------------------------------------------
$work = Join-Path $env:TEMP ("neuron-update-" + (Get-Date -Format 'yyyyMMdd-HHmmss'))
New-Item -ItemType Directory -Path $work -Force | Out-Null

if ($ZipPath) {
    if (-not (Test-Path $ZipPath)) { Flag 'ZIP_MISSING' "no file at $ZipPath"; Finish 'error' }
    $zip = [IO.Path]::GetFullPath($ZipPath)
    $sums = if ($SumsPath) { [IO.Path]::GetFullPath($SumsPath) } else { Join-Path (Split-Path -Parent $zip) 'SHA256SUMS.txt' }
    $target = $null
    $setup = Join-Path (Split-Path -Parent $zip) ((Split-Path -Leaf $zip) -replace '\.zip$', '-setup.exe')
    Say 'source' "local file $zip"
} else {
    $rel = Get-Release $Version
    if (-not $rel) {
        Flag 'NO_RELEASE_INFO' "could not read releases from GitHub (offline, rate-limited, the repo is private, or tag '$Version' does not exist)"
        Finish 'error'
    }
    $target = $rel.tag_name
    Say 'latest_version' $target
    $zipAsset = $rel.assets | Where-Object { $_.name -like 'neuron-*-windows-x86_64.zip' } | Select-Object -First 1
    $setupAsset = $rel.assets | Where-Object { $_.name -like 'neuron-*-windows-x86_64-setup.exe' } | Select-Object -First 1
    $sumAsset = $rel.assets | Where-Object { $_.name -eq 'SHA256SUMS.txt' } | Select-Object -First 1
    if (-not $zipAsset -or -not $sumAsset) { Flag 'ASSETS_MISSING' "release $target has no windows zip or no SHA256SUMS.txt"; Finish 'error' }

    if ($Action -eq 'check') {
        if (-not $installed) { Finish 'not-installed' }
        $cv = Parse-Version $current; $tv = Parse-Version $target
        if ($cv -and $tv -and $cv -ge $tv) { Finish 'up-to-date' }
        Finish 'update-available'
    }

    $zip = Join-Path $work $zipAsset.name
    $setup = if ($setupAsset) { Join-Path $work $setupAsset.name } else { $null }
    $sums = Join-Path $work 'SHA256SUMS.txt'
    if ($ReleaseViaGh) {
        $ErrorActionPreference = 'Continue'
        $patterns = @('--pattern', $zipAsset.name, '--pattern', 'SHA256SUMS.txt')
        if ($setupAsset) { $patterns += @('--pattern', $setupAsset.name) }
        $download = & gh release download $target --repo $Repo --dir $work @patterns --clobber 2>&1 | Out-String
        if ($LASTEXITCODE -ne 0) { Flag 'DOWNLOAD_FAILED' "gh could not download release $target`: $($download.Trim())"; Finish 'error' }
    } else {
        Invoke-WebRequest -Uri $zipAsset.browser_download_url -OutFile $zip -UseBasicParsing -Headers @{ 'User-Agent' = 'neuron-lazy-update' }
        Invoke-WebRequest -Uri $sumAsset.browser_download_url -OutFile $sums -UseBasicParsing -Headers @{ 'User-Agent' = 'neuron-lazy-update' }
        if ($setupAsset) {
            Invoke-WebRequest -Uri $setupAsset.browser_download_url -OutFile $setup -UseBasicParsing -Headers @{ 'User-Agent' = 'neuron-lazy-update' }
        }
    }
}

if ($Action -eq 'check') {
    Say 'note' 'offline check: the target version is read from the zip during apply'
    Finish $(if ($installed) { 'update-available' } else { 'not-installed' })
}

# ---- verify the download -------------------------------------------------------------------
if (-not (Test-Path $sums)) { Flag 'NO_CHECKSUMS' 'SHA256SUMS.txt not found next to the zip (STOP)'; Finish 'blocked' }
$zipName = Split-Path -Leaf $zip
$expected = Get-PublishedHash $sums $zipName
$actual = (Get-FileHash -Algorithm SHA256 -Path $zip).Hash.ToLower()
Say 'sha256' $actual
if (-not $expected) { Flag 'NOT_IN_CHECKSUMS' "$zipName is not listed in SHA256SUMS.txt (STOP)"; Finish 'blocked' }
if ($expected -ne $actual) { Flag 'HASH_MISMATCH' 'the zip does not match its published checksum (STOP). Do not install it.'; Finish 'blocked' }
Say 'checksum' 'ok'
$setupExpected = $null
if ($setup -and (Test-Path -LiteralPath $setup -PathType Leaf)) {
    $setupName = Split-Path -Leaf $setup
    $setupExpected = Get-PublishedHash $sums $setupName
    if (-not $setupExpected) { Flag 'SETUP_NOT_IN_CHECKSUMS' "$setupName is not listed in SHA256SUMS.txt (STOP)"; Finish 'blocked' }
    $setupActual = (Get-FileHash -LiteralPath $setup -Algorithm SHA256).Hash.ToLower()
    if ($setupActual -ne $setupExpected) { Flag 'SETUP_HASH_MISMATCH' 'the setup executable does not match its published checksum (STOP)'; Finish 'blocked' }
    Say 'setup_checksum' 'ok'
}
$packageBrokerHash = Get-ZipEntrySha256 $zip 'neuron-chroma-broker.exe'
$packageInstallerHash = Get-ZipEntrySha256 $zip 'install-chroma-broker.ps1'
if (($packageBrokerHash -and -not $packageInstallerHash) -or
    ($packageInstallerHash -and -not $packageBrokerHash)) {
    Flag 'BAD_ZIP' 'the verified archive does not contain exactly one Chroma broker and setup script'
    Finish 'error'
}

# Only the locally built stable beta releases through 0.1.2 lack Actions attestations.
$unattestedLocalBuild = $target -match '^v\d+\.\d+\.\d+$' -and (Parse-Version $target) -le [version]'0.1.2'
if (-not $ZipPath -and (Get-Command gh -ErrorAction SilentlyContinue)) {
    $ErrorActionPreference = 'Continue'
    $att = & gh attestation verify $zip --repo $Repo 2>&1 | Out-String
    if ($LASTEXITCODE -eq 0) { Say 'attestation' 'ok' }
    elseif ($unattestedLocalBuild -and $att -match 'HTTP 404: Not Found.*\/attestations\/sha256:') { Flag 'ATTESTATION_UNAVAILABLE' "$target has no provenance attestation. The checksum matched, but build provenance could not be verified." }
    elseif ($att -match 'auth login|not logged') { Flag 'ATTESTATION_SKIPPED' 'gh is installed but not logged in, so provenance was not checked. The checksum still matched.' }
    else { Flag 'ATTESTATION_FAILED' "gh attestation verify failed (STOP): $($att.Trim())"; Finish 'blocked' }
} elseif (-not $ZipPath) {
    Say 'attestation' 'skipped (gh not installed; checksum matched)'
}

# ---- unpack and compare versions -----------------------------------------------------------
$unzip = Join-Path $work 'unzipped'
Expand-Archive -Path $zip -DestinationPath $unzip -Force
$payload = Get-ChildItem -Path $unzip -Recurse -Filter 'neuron-app.exe' | Select-Object -First 1
if (-not $payload) { Flag 'BAD_ZIP' 'the zip has no neuron-app.exe'; Finish 'error' }
$payloadDir = $payload.DirectoryName
$newVersion = Get-InstalledVersion $payloadDir
Say 'new_version' $(if ($newVersion) { $newVersion } else { 'unknown' })
$newParsed = Parse-Version $newVersion
if (-not $packageBrokerHash -and $newParsed -and $newParsed -gt [version]'0.1.1') {
    Flag 'BAD_ZIP' "$newVersion is missing the Chroma broker payload introduced after v0.1.1"
    Finish 'error'
}
if ($installerManaged -and $newParsed -and $newParsed -le [version]'0.1.1') {
    Flag 'LEGACY_INSTALLER_DOWNGRADE' 'installer-managed builds before v0.1.2 predate the limited-tray broker boundary; use a portable copy for historical testing instead of restoring that startup model'
    Finish 'blocked'
}
if ($installerManaged -and (-not $setup -or -not (Test-Path -LiteralPath $setup -PathType Leaf))) {
    Flag 'SETUP_REQUIRED' 'this copy is managed by Windows Setup, but the matching verified setup executable is unavailable'
    Finish 'blocked'
}

if ($installed -and $current -and $newVersion) {
    $cv = Parse-Version $current; $nv = Parse-Version $newVersion
    if ($cv -and $nv -and $nv -lt $cv -and -not $AllowDowngrade) {
        Flag 'DOWNGRADE' "$newVersion is older than the installed $current. Rerun with -AllowDowngrade only if the user asked for that."
        Finish 'blocked'
    }
    if ($cv -and $nv -and $nv -eq $cv) { Flag 'SAME_VERSION' "$newVersion is already installed; reinstalling the same files" }
}

# ---- install -------------------------------------------------------------------------------
if ($installerManaged) {
    $wasRunning = Test-Running $InstallDir
    # Setup owns the installed tray lifecycle. It records whether the tray was running before it
    # resolves UAC, so a declined or failed broker update restores the original process instead of
    # leaving Neuron closed after an update that changed no app files.
    $setupArgs = @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', ('/DIR="' + $InstallDir + '"'))
    try {
        $setupProcess = Start-Process -FilePath $setup -ArgumentList $setupArgs -Wait -PassThru -ErrorAction Stop
    } catch {
        Flag 'SETUP_FAILED' $_.Exception.Message
        Finish 'error'
    }
    if ($setupProcess.ExitCode -ne 0) {
        Flag 'SETUP_FAILED' "setup exited with code $($setupProcess.ExitCode)"
        Finish 'error'
    }
    if (Test-Path -LiteralPath (Join-Path $InstallDir 'portable.flag')) {
        Flag 'INSTALL_MODE_INVALID' 'setup left a portable marker in an installer-managed directory'
        Finish 'error'
    }
    $after = Get-InstalledVersion $InstallDir
    Say 'installed_version' $after
    if ($newVersion -and $after -ne $newVersion) {
        Flag 'VERIFY_FAILED' "expected $newVersion after setup, found $after"
        Finish 'error'
    }
    if ($wasRunning -and $NoRelaunch) {
        $stop = Stop-Neuron $InstallDir $taskDirs
        Say 'stop' $stop
        if ($stop -eq 'needs-admin') { Flag 'CANNOT_STOP' 'the updated tray is running elevated and could not be stopped from this shell. Quit it from the tray.'; Finish 'needs-admin' }
        if ($stop -eq 'unknown') { Flag 'STOP_UNCONFIRMED' 'the updated neuron process is still running and its location cannot be read. Quit it from the tray.'; Finish 'blocked' }
    }
    Remove-Item -Path $work -Recurse -Force -ErrorAction SilentlyContinue
    Finish 'updated'
}

if (-not (Test-Writable $InstallDir)) {
    Flag 'NOT_WRITABLE' "cannot write to $InstallDir. Use a per-user folder such as $DefaultDir."
    Finish 'needs-admin'
}

$wasRunning = $false
$backup = $null
if ($installed) {
    $wasRunning = Test-Running $InstallDir
    $stop = Stop-Neuron $InstallDir $taskDirs
    Say 'stop' $stop
    if ($stop -eq 'needs-admin') { Flag 'CANNOT_STOP' 'neuron is running elevated and could not be stopped from this shell. Ask the user to quit it from the tray, then rerun.'; Finish 'needs-admin' }
    if ($stop -eq 'unknown') { Flag 'STOP_UNCONFIRMED' 'a neuron process is still running and its location cannot be read. Ask the user to quit neuron from the tray, then rerun.'; Finish 'blocked' }

    $stamp = "{0}-{1}" -f $(if ($current) { $current } else { 'unknown' }), (Get-Date -Format 'yyyyMMdd-HHmmss')
    $backup = Join-Path (Join-Path $InstallDir $BackupRoot) $stamp
    New-Item -ItemType Directory -Path $backup -Force | Out-Null
    $root = $payloadDir.TrimEnd('\')
    Get-ChildItem -Path $root -Recurse -File | ForEach-Object {
        $rel = $_.FullName.Substring($root.Length).TrimStart('\')
        $old = Join-Path $InstallDir $rel
        if (Test-Path -LiteralPath $old) {
            $dest = Join-Path $backup $rel
            $parent = Split-Path -Parent $dest
            if (-not (Test-Path $parent)) { New-Item -ItemType Directory -Path $parent -Force | Out-Null }
            Copy-Item -LiteralPath $old -Destination $dest -Force
        }
    }
    Say 'backup' $backup
    $n = @(Get-ChildItem -Path (Join-Path $InstallDir $BackupRoot) -Directory).Count
    if ($n -gt 3) { Flag 'OLD_BACKUPS' "$n update backups are kept in $BackupRoot. Older ones can be deleted if the user wants the space." }
}

try {
    Copy-Tree $payloadDir $InstallDir
} catch {
    Flag 'COPY_FAILED' $_.Exception.Message
    if ($backup) { Copy-Tree $backup $InstallDir; Say 'restored_from' $backup }
    Finish 'error'
}

# ---- verify --------------------------------------------------------------------------------
$after = Get-InstalledVersion $InstallDir
Say 'installed_version' $after
if ($newVersion -and $after -ne $newVersion) {
    Flag 'VERIFY_FAILED' "expected $newVersion after install, found $after"
    if ($backup) { Copy-Tree $backup $InstallDir; Say 'restored_from' $backup }
    Finish 'error'
}
$ErrorActionPreference = 'Continue'
$cliOut = & (Join-Path $InstallDir 'neuron.exe') --version 2>$null | Out-String
Say 'cli_reports' $cliOut.Trim()
if ($newVersion -and (Parse-Version $cliOut) -ne (Parse-Version $newVersion)) {
    Flag 'CLI_VERSION_MISMATCH' "neuron.exe reports '$($cliOut.Trim())' but SOURCE.txt says $newVersion"
}

Ensure-ChromaBroker $payloadDir $packageBrokerHash $packageInstallerHash
if ($wasRunning -and -not $NoRelaunch) { Start-Neuron $InstallDir $taskDirs }
Remove-Item -Path $work -Recurse -Force -ErrorAction SilentlyContinue
Finish $(if ($installed) { 'updated' } else { 'installed' })
