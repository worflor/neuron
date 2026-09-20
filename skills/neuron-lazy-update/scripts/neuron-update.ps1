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
$Task = 'Neuron (elevated tray)'
$BackupRoot = '.neuron-update-backup'
$DefaultDir = Join-Path $env:LOCALAPPDATA 'Programs\neuron'

function Say($k, $v) { Write-Output ("{0}: {1}" -f $k, $v) }
function Flag($code, $text) { Write-Output ("FLAG: {0} - {1}" -f $code, $text) }
function Finish($code) {
    Write-Output ("RESULT: {0}" -f $code)
    if ($code -in @('blocked', 'error', 'needs-admin')) { exit 1 }
    exit 0
}

# "v0.1.0-mk1" / "neuron 0.1.0" -> [version]0.1.0
function Parse-Version($s) {
    if ($s -match '(\d+)\.(\d+)\.(\d+)') { return [version]("{0}.{1}.{2}" -f $Matches[1], $Matches[2], $Matches[3]) }
    return $null
}

# Native commands write to stderr on ordinary misses (no such task); under 'Stop' that throws in 5.1.
function Get-TaskExeDir {
    $ErrorActionPreference = 'Continue'
    $xml = schtasks /query /tn $Task /xml 2>$null | Out-String
    if ($LASTEXITCODE -eq 0 -and $xml -match '<Command>([^<]+)</Command>') {
        return (Split-Path -Parent $Matches[1])
    }
    return $null
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

# Stops neuron running from $dir. Returns 'stopped', 'not-running', 'needs-admin' or 'unknown'.
function Stop-Neuron($dir, $taskDir) {
    $ErrorActionPreference = 'Continue'
    if (-not (Test-Running $dir)) { return 'not-running' }

    # An elevated, task-launched instance can only be stopped through the task from an
    # unelevated shell.
    if (Same-Dir $taskDir $dir) {
        schtasks /end /tn $Task 2>$null | Out-Null
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

function Start-Neuron($dir, $taskDir) {
    $ErrorActionPreference = 'Continue'
    if (Same-Dir $taskDir $dir) {
        schtasks /run /tn $Task 2>$null | Out-Null
        Say 'relaunched' "via the scheduled task ($Task)"
    } else {
        Start-Process -FilePath (Join-Path $dir 'neuron-app.exe') -WorkingDirectory $dir
        Say 'relaunched' 'neuron-app.exe'
    }
}

function Get-Release($tag) {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    $url = if ($tag) { "https://api.github.com/repos/$Repo/releases/tags/$tag" } else { "https://api.github.com/repos/$Repo/releases/latest" }
    try {
        return Invoke-RestMethod -Uri $url -Headers @{ 'User-Agent' = 'neuron-lazy-update' } -TimeoutSec 30
    } catch {
        return $null
    }
}

# ---------------------------------------------------------------------------------------------

$taskDir = Get-TaskExeDir
if (-not $InstallDir) {
    if ($taskDir) { $InstallDir = $taskDir }
    elseif (Test-Path (Join-Path $DefaultDir 'neuron-app.exe')) { $InstallDir = $DefaultDir }
    else { $InstallDir = $DefaultDir }
}
$InstallDir = [IO.Path]::GetFullPath($InstallDir)
$installed = (Test-Path (Join-Path $InstallDir 'neuron-app.exe'))

Say 'install_dir' $InstallDir
Say 'installed' $installed
if ($taskDir) { Say 'autostart_task_dir' $taskDir }
if ($taskDir -and -not (Same-Dir $taskDir $InstallDir)) {
    Flag 'TASK_ELSEWHERE' "the autostart task launches a neuron in $taskDir, not this folder. Check which install the user means."
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

# ---- rollback ------------------------------------------------------------------------------
if ($Action -eq 'rollback') {
    $bdir = Join-Path $InstallDir $BackupRoot
    $latest = Get-ChildItem -Path $bdir -Directory -ErrorAction SilentlyContinue | Sort-Object Name -Descending | Select-Object -First 1
    if (-not $latest) { Flag 'NO_BACKUP' "nothing to roll back to in $bdir"; Finish 'blocked' }
    Say 'restoring' $latest.FullName
    if (-not (Test-Writable $InstallDir)) { Flag 'NOT_WRITABLE' 'cannot write to the install folder'; Finish 'needs-admin' }
    $wasRunning = Test-Running $InstallDir
    $stop = Stop-Neuron $InstallDir $taskDir
    if ($stop -eq 'needs-admin') { Flag 'CANNOT_STOP' 'neuron is running and could not be stopped from this shell'; Finish 'needs-admin' }
    if ($stop -eq 'unknown') { Flag 'STOP_UNCONFIRMED' 'a neuron process is still running and its location cannot be read. Ask the user to quit neuron from the tray, then rerun.'; Finish 'blocked' }
    Copy-Tree $latest.FullName $InstallDir
    Say 'installed_version' (Get-InstalledVersion $InstallDir)
    if ($wasRunning -and -not $NoRelaunch) { Start-Neuron $InstallDir $taskDir }
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
    $sumAsset = $rel.assets | Where-Object { $_.name -eq 'SHA256SUMS.txt' } | Select-Object -First 1
    if (-not $zipAsset -or -not $sumAsset) { Flag 'ASSETS_MISSING' "release $target has no windows zip or no SHA256SUMS.txt"; Finish 'error' }

    if ($Action -eq 'check') {
        if (-not $installed) { Finish 'not-installed' }
        $cv = Parse-Version $current; $tv = Parse-Version $target
        if ($cv -and $tv -and $cv -ge $tv) { Finish 'up-to-date' }
        Finish 'update-available'
    }

    $zip = Join-Path $work $zipAsset.name
    $sums = Join-Path $work 'SHA256SUMS.txt'
    Invoke-WebRequest -Uri $zipAsset.browser_download_url -OutFile $zip -UseBasicParsing -Headers @{ 'User-Agent' = 'neuron-lazy-update' }
    Invoke-WebRequest -Uri $sumAsset.browser_download_url -OutFile $sums -UseBasicParsing -Headers @{ 'User-Agent' = 'neuron-lazy-update' }
}

if ($Action -eq 'check') {
    Say 'note' 'offline check: the target version is read from the zip during apply'
    Finish $(if ($installed) { 'update-available' } else { 'not-installed' })
}

# ---- verify the download -------------------------------------------------------------------
if (-not (Test-Path $sums)) { Flag 'NO_CHECKSUMS' 'SHA256SUMS.txt not found next to the zip (STOP)'; Finish 'blocked' }
$zipName = Split-Path -Leaf $zip
$expected = $null
foreach ($line in (Get-Content $sums)) {
    $parts = ($line -replace [char]0xFEFF, '').Trim() -split '\s+'
    if ($parts.Count -ge 2 -and $parts[-1] -eq $zipName) { $expected = $parts[0].ToLower() }
}
$actual = (Get-FileHash -Algorithm SHA256 -Path $zip).Hash.ToLower()
Say 'sha256' $actual
if (-not $expected) { Flag 'NOT_IN_CHECKSUMS' "$zipName is not listed in SHA256SUMS.txt (STOP)"; Finish 'blocked' }
if ($expected -ne $actual) { Flag 'HASH_MISMATCH' 'the zip does not match its published checksum (STOP). Do not install it.'; Finish 'blocked' }
Say 'checksum' 'ok'

if (-not $ZipPath -and (Get-Command gh -ErrorAction SilentlyContinue)) {
    $ErrorActionPreference = 'Continue'
    $att = & gh attestation verify $zip --repo $Repo 2>&1 | Out-String
    if ($LASTEXITCODE -eq 0) { Say 'attestation' 'ok' }
    # Only v0.1.0's locally built asset treats missing provenance as nonfatal.
    elseif ($target -eq 'v0.1.0' -and $att -match 'HTTP 404: Not Found.*\/attestations\/sha256:') { Flag 'ATTESTATION_UNAVAILABLE' 'v0.1.0 has no provenance attestation. The checksum matched, but build provenance could not be verified.' }
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

if ($installed -and $current -and $newVersion) {
    $cv = Parse-Version $current; $nv = Parse-Version $newVersion
    if ($cv -and $nv -and $nv -lt $cv -and -not $AllowDowngrade) {
        Flag 'DOWNGRADE' "$newVersion is older than the installed $current. Rerun with -AllowDowngrade only if the user asked for that."
        Finish 'blocked'
    }
    if ($cv -and $nv -and $nv -eq $cv) { Flag 'SAME_VERSION' "$newVersion is already installed; reinstalling the same files" }
}

# ---- install -------------------------------------------------------------------------------
if (-not (Test-Writable $InstallDir)) {
    Flag 'NOT_WRITABLE' "cannot write to $InstallDir. Use a per-user folder such as $DefaultDir, or run as administrator."
    Finish 'needs-admin'
}

$wasRunning = $false
$backup = $null
if ($installed) {
    $wasRunning = Test-Running $InstallDir
    $stop = Stop-Neuron $InstallDir $taskDir
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

if ($wasRunning -and -not $NoRelaunch) { Start-Neuron $InstallDir $taskDir }
Remove-Item -Path $work -Recurse -Force -ErrorAction SilentlyContinue
Finish $(if ($installed) { 'updated' } else { 'installed' })
