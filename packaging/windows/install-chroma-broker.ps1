# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
# Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

# The installer and updater elevate this fixed-purpose broker setup. The tray stays Limited.

[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$BinaryPath,
    [Parameter(Mandatory)][ValidatePattern('^[0-9a-fA-F]{64}$')][string]$ExpectedSha256,
    [Parameter(Mandatory)][string]$OwnerSid
)

$ErrorActionPreference = 'Stop'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'install-chroma-broker.ps1 requires an administrator PowerShell'
}

$source = [IO.Path]::GetFullPath($BinaryPath)
if (-not (Test-Path -LiteralPath $source -PathType Leaf)) { throw "broker binary not found: $source" }
try { $owner = [Security.Principal.SecurityIdentifier]::new($OwnerSid) } catch {
    throw 'broker owner is not a valid Windows SID'
}
$OwnerSid = $owner.Value
function Assert-RegularFileIfPresent($path) {
    $item = Get-Item -LiteralPath $path -Force -ErrorAction SilentlyContinue
    if ($item -and ($item.PSIsContainer -or
        ($item.Attributes -band [IO.FileAttributes]::ReparsePoint))) {
        throw "broker path is not a regular file: $path"
    }
}
Assert-RegularFileIfPresent $source
if ((Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash -ine $ExpectedSha256) {
    throw 'broker binary hash changed before installation'
}

$taskName = 'Neuron Chroma broker'
$programFiles64 = if ($env:ProgramW6432) { $env:ProgramW6432 } else {
    [Environment]::GetFolderPath([Environment+SpecialFolder]::ProgramFiles)
}
$dir = Join-Path $programFiles64 'NeuronChromaBroker'
$exe = Join-Path $dir 'neuron-chroma-broker.exe'
$receipt = Join-Path $dir 'broker-installed.sha256'
$pending = Join-Path $dir 'broker-install.pending'
$admins = New-Object Security.Principal.SecurityIdentifier('S-1-5-32-544')
$system = New-Object Security.Principal.SecurityIdentifier('S-1-5-18')
$users = New-Object Security.Principal.SecurityIdentifier('S-1-5-32-545')

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
        # PowerShell 7 uses the System.Threading.AccessControl factory.
        $args = @($false, $name, $false, $security)
        $mutex = $aclType.GetMethod('Create').Invoke($null, $args)
    } else {
        # Windows PowerShell 5.1 exposes the same ACL-bearing API as a constructor.
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

function Set-ProtectedFileAcl($path, [bool]$directory) {
    $acl = if ($directory) {
        New-Object Security.AccessControl.DirectorySecurity
    } else {
        New-Object Security.AccessControl.FileSecurity
    }
    $acl.SetOwner($admins)
    $acl.SetAccessRuleProtection($true, $false)
    $inherit = if ($directory) {
        [Security.AccessControl.InheritanceFlags]'ContainerInherit, ObjectInherit'
    } else {
        [Security.AccessControl.InheritanceFlags]::None
    }
    $propagate = [Security.AccessControl.PropagationFlags]::None
    $allow = [Security.AccessControl.AccessControlType]::Allow
    foreach ($entry in @(
        @($system, [Security.AccessControl.FileSystemRights]::FullControl),
        @($admins, [Security.AccessControl.FileSystemRights]::FullControl),
        @($users, [Security.AccessControl.FileSystemRights]::ReadAndExecute)
    )) {
        $rule = [Security.AccessControl.FileSystemAccessRule]::new(
            $entry[0], $entry[1], $inherit, $propagate, $allow)
        $acl.AddAccessRule($rule)
    }
    Set-Acl -LiteralPath $path -AclObject $acl -ErrorAction Stop
    Assert-ProtectedFileAcl $path
}

function Assert-ProtectedFileAcl($path) {
    $installed = Get-Acl -LiteralPath $path
    $ownerSid = ([Security.Principal.NTAccount]$installed.Owner).Translate(
        [Security.Principal.SecurityIdentifier]
    ).Value
    if ($ownerSid -ne $admins.Value -or -not $installed.AreAccessRulesProtected) {
        throw "protected ACL verification failed: $path"
    }
    $allowed = @($system.Value, $admins.Value, $users.Value)
    foreach ($rule in $installed.GetAccessRules($true, $false, [Security.Principal.SecurityIdentifier])) {
        if ($rule.IdentityReference.Value -notin $allowed -or
            ($rule.IdentityReference.Value -eq $users.Value -and
             ($rule.FileSystemRights -band [Security.AccessControl.FileSystemRights]::Write) -ne 0)) {
            throw "unexpected writable ACL rule: $path"
        }
    }
}

function Write-ProtectedRecord($path, $content) {
    $temp = "$path.partial-$PID-$([guid]::NewGuid().ToString('N'))"
    try {
        [IO.File]::WriteAllText($temp, $content, [Text.UTF8Encoding]::new($false))
        Set-ProtectedFileAcl $temp $false
        Move-Item -LiteralPath $temp -Destination $path -Force -ErrorAction Stop
        Assert-ProtectedFileAcl $path
    } finally {
        Remove-Item -LiteralPath $temp -Force -ErrorAction SilentlyContinue
    }
}

function Test-OwnershipRecord($lines) {
    return ($lines.Count -eq 4 -and
        $lines[0].Trim() -match '^[0-9a-fA-F]{64}$' -and
        $lines[1].Trim() -eq 'SYSTEM startup' -and
        $lines[2].Trim() -eq 'task-acl-v1' -and
        [bool]$lines[3].Trim())
}

function Remove-RecognizedTransactionDirectory($path) {
    $item = Get-Item -LiteralPath $path -Force -ErrorAction SilentlyContinue
    if (-not $item) { return }
    if (-not $item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        return
    }
    $children = @(Get-ChildItem -LiteralPath $path -Force -ErrorAction Stop)
    foreach ($child in $children) {
        if ($child.PSIsContainer -or ($child.Attributes -band [IO.FileAttributes]::ReparsePoint) -or
            ($child.Name -notin @('neuron-chroma-broker.exe', 'broker-install.pending', 'broker-installed.sha256') -and
             $child.Name -notlike 'broker-install.pending.partial-*' -and
             $child.Name -notlike 'broker-installed.sha256.partial-*')) {
            return
        }
    }
    $recordPath = @(
        (Join-Path $path 'broker-installed.sha256'),
        (Join-Path $path 'broker-install.pending')
    ) | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } | Select-Object -First 1
    if ($recordPath) {
        $lines = @(Get-Content -LiteralPath $recordPath -TotalCount 5)
        if (-not (Test-OwnershipRecord $lines) -or $lines[3].Trim() -ine $OwnerSid) { return }
        Assert-ProtectedFileAcl $path
        Assert-ProtectedFileAcl $recordPath
    } elseif ($children.Count -ne 0) {
        return
    }
    foreach ($child in $children) {
        Remove-Item -LiteralPath $child.FullName -Force -ErrorAction Stop
    }
    Remove-Item -LiteralPath $path -Force -ErrorAction Stop
}

$ownerToken = $OwnerSid -replace '[^A-Za-z0-9-]', '_'
$stagingPrefix = "NeuronChromaBroker.installing-$ownerToken-"
$removingPrefix = "NeuronChromaBroker.removing-$ownerToken-"
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
    Where-Object { $_.Name.StartsWith($stagingPrefix) -or $_.Name.StartsWith($removingPrefix) })) {
    Remove-RecognizedTransactionDirectory $stale.FullName
}

$dirPresent = Test-Path -LiteralPath $dir
if ($dirPresent) {
    $item = Get-Item -LiteralPath $dir -Force
    if (-not $item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw "broker destination is not a normal directory: $dir"
    }
}
Assert-RegularFileIfPresent $exe
Assert-RegularFileIfPresent $receipt
Assert-RegularFileIfPresent $pending
$receiptPresent = Test-Path -LiteralPath $receipt -PathType Leaf
$pendingPresent = Test-Path -LiteralPath $pending -PathType Leaf
$previousReceipt = if ($receiptPresent) {
    @(Get-Content -LiteralPath $receipt -TotalCount 5)
} else { @() }
$pendingReceipt = if ($pendingPresent) {
    @(Get-Content -LiteralPath $pending -TotalCount 5)
} else { @() }
$oldTask = Get-OptionalScheduledTask $taskName
$record = if ($receiptPresent -and (Test-OwnershipRecord $previousReceipt)) {
    @{ Path = $receipt; Lines = $previousReceipt }
} elseif ($pendingPresent -and (Test-OwnershipRecord $pendingReceipt)) {
    @{ Path = $pending; Lines = $pendingReceipt }
} else { $null }
if ($record) {
    if ($record.Lines[3].Trim() -ine $OwnerSid) {
        throw 'the machine-wide Chroma broker belongs to another Windows user'
    }
    Assert-ProtectedFileAcl $dir
    Assert-ProtectedFileAcl $record.Path
} elseif ($receiptPresent -or $pendingPresent) {
    throw 'the machine-wide Chroma broker ownership record is malformed'
} elseif ($dirPresent -or $oldTask -or (Test-Path -LiteralPath $exe -PathType Leaf)) {
    throw 'existing machine-wide Chroma broker state has no ownership receipt'
}

if (-not $dirPresent) {
    $stage = Join-Path $programFiles64 ($stagingPrefix + $PID + '-' + [guid]::NewGuid().ToString('N'))
    $stageExe = Join-Path $stage 'neuron-chroma-broker.exe'
    $stagePending = Join-Path $stage 'broker-install.pending'
    try {
        New-Item -ItemType Directory -Path $stage -ErrorAction Stop | Out-Null
        Set-ProtectedFileAcl $stage $true
        Write-ProtectedRecord $stagePending "$ExpectedSha256`nSYSTEM startup`ntask-acl-v1`n$OwnerSid`n"
        Copy-Item -LiteralPath $source -Destination $stageExe -Force -ErrorAction Stop
        Set-ProtectedFileAcl $stageExe $false
        if ((Get-FileHash -LiteralPath $stageExe -Algorithm SHA256).Hash -ine $ExpectedSha256) {
            throw 'staged broker binary hash changed before publication'
        }
        # Same-volume directory rename publishes a complete, protected pending transaction. A kill
        # before this point leaves only a recognized staging directory; after it, repair can trust
        # the pending owner record at the final path.
        [IO.Directory]::Move($stage, $dir) # no-replace: a concurrent/fabricated destination fails
        $dirPresent = $true
        $pendingPresent = $true
    } catch {
        Remove-RecognizedTransactionDirectory $stage
        throw
    }
}
Set-ProtectedFileAcl $dir $true

if ($oldTask) {
    Disable-ScheduledTask -TaskName $taskName -ErrorAction Stop | Out-Null
    Stop-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
    for ($attempt = 0; $attempt -lt 30; $attempt++) {
        $oldState = (Get-ScheduledTask -TaskName $taskName -ErrorAction Stop).State
        if ($oldState -ne 'Running') { break }
        Start-Sleep -Milliseconds 200
    }
    if ((Get-ScheduledTask -TaskName $taskName -ErrorAction Stop).State -eq 'Running') {
        throw 'previous broker did not stop; task remains disabled'
    }
}
Copy-Item -LiteralPath $source -Destination $exe -Force -ErrorAction Stop
Assert-RegularFileIfPresent $exe
Set-ProtectedFileAcl $exe $false
if ((Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash -ine $ExpectedSha256) {
    throw 'broker binary hash changed during installation; task remains disabled'
}

$action = New-ScheduledTaskAction -Execute $exe -WorkingDirectory $dir
$trigger = New-ScheduledTaskTrigger -AtStartup
$brokerPrincipal = New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest
$settings = New-ScheduledTaskSettingsSet -Disable -ExecutionTimeLimit (New-TimeSpan -Seconds 0) -MultipleInstances IgnoreNew -StartWhenAvailable -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)
Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger -Principal $brokerPrincipal -Settings $settings -Force -ErrorAction Stop | Out-Null

# Task Scheduler otherwise adds ACEs that could let a limited user edit the elevated action.
$service = New-Object -ComObject 'Schedule.Service'
$service.Connect()
$registered = $service.GetFolder('\').GetTask($taskName)
$registered.SetSecurityDescriptor('O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FR;;;BU)', 0x10)
$sddl = $registered.GetSecurityDescriptor(0x7)
$taskDescriptor = [Security.AccessControl.CommonSecurityDescriptor]::new($false, $false, $sddl)
if ($taskDescriptor.Owner.Value -ne $admins.Value -or
    -not $taskDescriptor.DiscretionaryAcl.IsCanonical -or
    $taskDescriptor.DiscretionaryAcl.Count -ne 3) {
    throw 'broker task ACL is not canonical'
}
$expectedTaskRights = @{}
$expectedTaskRights[$system.Value] = 0x001F01FF
$expectedTaskRights[$admins.Value] = 0x001F01FF
$expectedTaskRights[$users.Value] = 0x00120089
foreach ($ace in $taskDescriptor.DiscretionaryAcl) {
    $sid = $ace.SecurityIdentifier.Value
    if (-not $expectedTaskRights.ContainsKey($sid) -or
        $ace.AceQualifier -ne [Security.AccessControl.AceQualifier]::AccessAllowed -or
        $ace.AccessMask -ne $expectedTaskRights[$sid]) {
        throw 'broker task ACL differs from the protected definition'
    }
}
$task = Get-ScheduledTask -TaskName $taskName -ErrorAction Stop
$principalId = [string]$task.Principal.UserId
$principalSid = if ($principalId -match '^S-1-') { $principalId } else {
    ([Security.Principal.NTAccount]$principalId).Translate([Security.Principal.SecurityIdentifier]).Value
}
if ($task.Principal.RunLevel -ne 'Highest' -or $task.Actions.Count -ne 1 -or
    $task.Principal.LogonType -ne 'ServiceAccount' -or
    $principalSid -ne $system.Value -or
    $task.Triggers.Count -ne 1 -or $task.Triggers[0].CimClass.CimClassName -ne 'MSFT_TaskBootTrigger' -or
    $task.Actions[0].Execute -ine $exe -or
    $task.Actions[0].Arguments -or
    $task.Actions[0].WorkingDirectory -ine $dir) {
    throw 'broker task verification failed'
}
Enable-ScheduledTask -TaskName $taskName -ErrorAction Stop | Out-Null
Start-ScheduledTask -TaskName $taskName -ErrorAction Stop
for ($attempt = 0; $attempt -lt 25; $attempt++) {
    if ((Get-ScheduledTask -TaskName $taskName -ErrorAction Stop).State -eq 'Running') { break }
    Start-Sleep -Milliseconds 200
}
if ((Get-ScheduledTask -TaskName $taskName -ErrorAction Stop).State -ne 'Running') {
    Disable-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue | Out-Null
    throw 'broker task did not remain running'
}

Write-ProtectedRecord $receipt "$ExpectedSha256`nSYSTEM startup`ntask-acl-v1`n$OwnerSid`n"
Remove-Item -LiteralPath $pending -Force -ErrorAction SilentlyContinue
Write-Output "installed: $exe"
Write-Output "sha256: $ExpectedSha256"
Write-Output "task: $taskName"
} finally {
    if ($transactionHeld) { $transactionMutex.ReleaseMutex() }
    $transactionMutex.Dispose()
}
