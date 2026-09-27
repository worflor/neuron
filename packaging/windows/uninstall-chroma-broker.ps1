# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
# Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$OwnerSid
)

$ErrorActionPreference = 'Stop'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'uninstall-chroma-broker.ps1 requires an administrator PowerShell'
}
try { $owner = [Security.Principal.SecurityIdentifier]::new($OwnerSid) } catch {
    throw 'broker owner is not a valid Windows SID'
}
$OwnerSid = $owner.Value
$taskName = 'Neuron Chroma broker'
$programFiles64 = if ($env:ProgramW6432) { $env:ProgramW6432 } else {
    [Environment]::GetFolderPath([Environment+SpecialFolder]::ProgramFiles)
}
$dir = Join-Path $programFiles64 'NeuronChromaBroker'
$exe = Join-Path $dir 'neuron-chroma-broker.exe'
$receipt = Join-Path $dir 'broker-installed.sha256'
$pending = Join-Path $dir 'broker-install.pending'
$admins = [Security.Principal.SecurityIdentifier]::new('S-1-5-32-544')
$system = [Security.Principal.SecurityIdentifier]::new('S-1-5-18')

function New-BrokerTransactionMutex {
    $security = [Security.AccessControl.MutexSecurity]::new()
    $security.SetAccessRuleProtection($true, $false)
    $allow = [Security.AccessControl.AccessControlType]::Allow
    foreach ($sid in @($system, $admins)) {
        $security.AddAccessRule([Security.AccessControl.MutexAccessRule]::new(
            $sid, [Security.AccessControl.MutexRights]::FullControl, $allow))
    }
    $name = 'Global\NeuronChromaBroker.Transaction.v1'
    $created = $false
    $aclType = 'System.Threading.MutexAcl' -as [type]
    if ($aclType) {
        $args = @($false, $name, $false, $security)
        $mutex = $aclType.GetMethod('Create').Invoke($null, $args)
    } else {
        $mutex = [Threading.Mutex]::new($false, $name, [ref]$created, $security)
    }
    return $mutex
}

function Get-OptionalScheduledTask($name) {
    try {
        return Get-ScheduledTask -TaskName $name -ErrorAction Stop
    } catch {
        if ($_.CategoryInfo.Category -eq 'ObjectNotFound') { return $null }
        throw
    }
}

function Assert-ProtectedPath($path) {
    $item = Get-Item -LiteralPath $path -Force -ErrorAction Stop
    if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw "broker path is a reparse point: $path"
    }
    $acl = Get-Acl -LiteralPath $path
    $ownerSid = ([Security.Principal.NTAccount]$acl.Owner).Translate(
        [Security.Principal.SecurityIdentifier]
    ).Value
    if ($ownerSid -ne 'S-1-5-32-544' -or -not $acl.AreAccessRulesProtected) {
        throw "broker ownership record is not protected: $path"
    }
    foreach ($rule in $acl.GetAccessRules($true, $false, [Security.Principal.SecurityIdentifier])) {
        if ($rule.IdentityReference.Value -notin @('S-1-5-18', 'S-1-5-32-544', 'S-1-5-32-545') -or
            ($rule.IdentityReference.Value -eq 'S-1-5-32-545' -and
             ($rule.FileSystemRights -band [Security.AccessControl.FileSystemRights]::Write) -ne 0)) {
            throw "broker ownership record has an unexpected writable ACL: $path"
        }
    }
}

function Test-OwnershipRecord($lines) {
    return ($lines.Count -eq 4 -and
        $lines[0].Trim() -match '^[0-9a-fA-F]{64}$' -and
        $lines[1].Trim() -eq 'SYSTEM startup' -and
        $lines[2].Trim() -eq 'task-acl-v1' -and
        [bool]$lines[3].Trim())
}

function Remove-RecognizedRemovalDirectory($path) {
    $item = Get-Item -LiteralPath $path -Force -ErrorAction Stop
    if (-not $item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw "broker removal transaction is not a normal directory: $path"
    }
    $children = @(Get-ChildItem -LiteralPath $path -Force -ErrorAction Stop)
    if ($children.Count -eq 0) {
        Remove-Item -LiteralPath $path -Force -ErrorAction Stop
        return
    }
    Assert-ProtectedPath $path
    foreach ($child in $children) {
        if ($child.PSIsContainer -or ($child.Attributes -band [IO.FileAttributes]::ReparsePoint) -or
            ($child.Name -notin @('neuron-chroma-broker.exe', 'broker-installed.sha256', 'broker-install.pending') -and
             $child.Name -notlike 'broker-install.pending.partial-*' -and
             $child.Name -notlike 'broker-installed.sha256.partial-*')) {
            throw "broker removal transaction contains an unexpected path: $($child.FullName)"
        }
    }
    foreach ($recordName in @('broker-installed.sha256', 'broker-install.pending')) {
        $recordPath = Join-Path $path $recordName
        if (Test-Path -LiteralPath $recordPath -PathType Leaf) {
            $lines = @(Get-Content -LiteralPath $recordPath -TotalCount 5)
            if (-not (Test-OwnershipRecord $lines) -or $lines[3].Trim() -ine $OwnerSid) {
                throw "broker removal transaction has an invalid ownership record: $recordPath"
            }
            Assert-ProtectedPath $recordPath
        }
    }
    foreach ($child in $children) {
        Remove-Item -LiteralPath $child.FullName -Force -ErrorAction Stop
    }
    Remove-Item -LiteralPath $path -Force -ErrorAction Stop
}

$ownerToken = $OwnerSid -replace '[^A-Za-z0-9-]', '_'
$removingPrefix = "NeuronChromaBroker.removing-$ownerToken-"
$installingPrefix = "NeuronChromaBroker.installing-$ownerToken-"
$transactionMutex = New-BrokerTransactionMutex
$transactionHeld = $false
try {
try {
    $transactionHeld = $transactionMutex.WaitOne([TimeSpan]::FromMinutes(2))
} catch [Threading.AbandonedMutexException] {
    $transactionHeld = $true
}
if (-not $transactionHeld) { throw 'timed out waiting for another Chroma broker transaction' }
foreach ($stale in @(Get-ChildItem -LiteralPath $programFiles64 -Directory -Force -ErrorAction Stop |
    Where-Object { $_.Name.StartsWith($removingPrefix) -or $_.Name.StartsWith($installingPrefix) })) {
    Remove-RecognizedRemovalDirectory $stale.FullName
}

$receiptPresent = Test-Path -LiteralPath $receipt -PathType Leaf
$pendingPresent = Test-Path -LiteralPath $pending -PathType Leaf
$receiptLines = if ($receiptPresent) { @(Get-Content -LiteralPath $receipt -TotalCount 5) } else { @() }
$pendingLines = if ($pendingPresent) { @(Get-Content -LiteralPath $pending -TotalCount 5) } else { @() }
$record = if ($receiptPresent -and (Test-OwnershipRecord $receiptLines)) {
    @{ Path = $receipt; Lines = $receiptLines }
} elseif ($pendingPresent -and (Test-OwnershipRecord $pendingLines)) {
    @{ Path = $pending; Lines = $pendingLines }
} else { $null }
if ($record) {
    if ($record.Lines[3].Trim() -ine $OwnerSid) {
        throw 'the machine-wide Chroma broker belongs to another Windows user'
    }
    Assert-ProtectedPath $dir
    Assert-ProtectedPath $record.Path
} elseif ($receiptPresent -or $pendingPresent) {
    throw 'the machine-wide Chroma broker ownership record is malformed'
} elseif ((Test-Path -LiteralPath $dir) -or (Get-OptionalScheduledTask $taskName)) {
    throw 'existing machine-wide Chroma broker state has no ownership receipt'
}

$task = Get-OptionalScheduledTask $taskName
if ($task) {
    Disable-ScheduledTask -TaskName $taskName -ErrorAction Stop | Out-Null
    Stop-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
    for ($attempt = 0; $attempt -lt 30; $attempt++) {
        if ((Get-ScheduledTask -TaskName $taskName -ErrorAction Stop).State -ne 'Running') { break }
        Start-Sleep -Milliseconds 200
    }
    if ((Get-ScheduledTask -TaskName $taskName -ErrorAction Stop).State -eq 'Running') {
        throw 'broker did not stop; task remains disabled'
    }
    Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction Stop
}
if (Test-Path -LiteralPath $dir) {
    $item = Get-Item -LiteralPath $dir -Force -ErrorAction Stop
    if (-not $item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw "broker destination is not a normal directory: $dir"
    }
    $children = @(Get-ChildItem -LiteralPath $dir -Force -ErrorAction Stop)
    foreach ($child in $children) {
        if ($child.PSIsContainer -or ($child.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            throw "broker path is not a regular file: $($child.FullName)"
        }
        $recognized = $child.Name -in @(
            'neuron-chroma-broker.exe', 'broker-installed.sha256', 'broker-install.pending'
        ) -or $child.Name -like 'broker-installed.sha256.partial-*' -or
            $child.Name -like 'broker-install.pending.partial-*'
        if (-not $recognized) {
            throw "broker directory contains an unexpected file: $($child.FullName)"
        }
    }
    # Remove only recognized abandoned record temporaries while the authenticated receipt still
    # occupies the final path. Unknown files above stop the transaction before ownership is lost.
    foreach ($child in $children) {
        if ($child.Name -like 'broker-installed.sha256.partial-*' -or
            $child.Name -like 'broker-install.pending.partial-*') {
            Remove-Item -LiteralPath $child.FullName -Force -ErrorAction Stop
        }
    }
    $tombstone = Join-Path $programFiles64 (
        $removingPrefix + $PID + '-' + [guid]::NewGuid().ToString('N'))
    # The same-volume rename removes the live final path atomically while its ownership record is
    # still inside it. Interruption after this point cannot strand an unowned final installation.
    [IO.Directory]::Move($dir, $tombstone)
    foreach ($name in @('neuron-chroma-broker.exe', 'broker-installed.sha256', 'broker-install.pending')) {
        $path = Join-Path $tombstone $name
        if (Test-Path -LiteralPath $path -PathType Leaf) {
            Remove-Item -LiteralPath $path -Force -ErrorAction Stop
        }
    }
    # Intentionally non-recursive: the preflight above guarantees only known regular files moved.
    Remove-Item -LiteralPath $tombstone -Force -ErrorAction Stop
}

foreach ($name in @('Neuron', 'Neuron (elevated tray)')) {
    if (Get-OptionalScheduledTask $name) {
        Unregister-ScheduledTask -TaskName $name -Confirm:$false -ErrorAction Stop
    }
}

Write-Output 'removed: Neuron Chroma broker'
} finally {
    if ($transactionHeld) { $transactionMutex.ReleaseMutex() }
    $transactionMutex.Dispose()
}
