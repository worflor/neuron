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

function Get-BrokerTaskSnapshot($taskName) {
    $task = Get-OptionalScheduledTask $taskName
    if (-not $task) { return @{ Present = $false } }
    $xml = Export-ScheduledTask -TaskName $taskName -ErrorAction Stop | Out-String
    $service = New-Object -ComObject 'Schedule.Service'
    $service.Connect()
    $registered = $service.GetFolder('\').GetTask($taskName)
    return @{
        Present = $true
        Xml = $xml
        Sddl = $registered.GetSecurityDescriptor(0x7)
        Enabled = [bool]$registered.Enabled
        Running = ($task.State -eq 'Running')
    }
}

function Get-NormalizedBrokerTaskXml($xml) {
    $document = [xml]$xml
    $settings = $document.SelectSingleNode("//*[local-name()='Settings']")
    if (-not $settings) { throw 'broker task XML has no settings element' }
    $enabledNode = $settings.SelectSingleNode("*[local-name()='Enabled']")
    if ($enabledNode) { [void]$settings.RemoveChild($enabledNode) }
    return $document.OuterXml
}

function Restore-BrokerTaskSnapshot($taskName, $snapshot) {
    $current = Get-OptionalScheduledTask $taskName
    if ($current) {
        Disable-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue | Out-Null
        Stop-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
        Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction Stop
    }
    if (-not $snapshot.Present) { return }
    $document = [xml]$snapshot.Xml
    $settings = $document.SelectSingleNode("//*[local-name()='Settings']")
    if (-not $settings) { throw 'previous broker task XML has no settings element' }
    $enabledNode = $settings.SelectSingleNode("*[local-name()='Enabled']")
    if (-not $enabledNode) {
        $enabledNode = $document.CreateElement('Enabled', $settings.NamespaceURI)
        [void]$settings.AppendChild($enabledNode)
    }
    $enabledNode.InnerText = 'false'
    Register-ScheduledTask -TaskName $taskName -Xml $document.OuterXml -Force -ErrorAction Stop | Out-Null
    Disable-ScheduledTask -TaskName $taskName -ErrorAction Stop | Out-Null
    $service = New-Object -ComObject 'Schedule.Service'
    $service.Connect()
    $registered = $service.GetFolder('\').GetTask($taskName)
    $registered.SetSecurityDescriptor($snapshot.Sddl, 0x10)
    if ($registered.GetSecurityDescriptor(0x7) -cne $snapshot.Sddl) {
        throw 'previous broker task ACL did not restore exactly'
    }
    if ($snapshot.Running) {
        Enable-ScheduledTask -TaskName $taskName -ErrorAction Stop | Out-Null
        Start-ScheduledTask -TaskName $taskName -ErrorAction Stop
        for ($attempt = 0; $attempt -lt 25; $attempt++) {
            if ((Get-ScheduledTask -TaskName $taskName -ErrorAction Stop).State -eq 'Running') { break }
            Start-Sleep -Milliseconds 200
        }
        if ((Get-ScheduledTask -TaskName $taskName -ErrorAction Stop).State -ne 'Running') {
            throw 'previous broker task did not resume during rollback'
        }
        if (-not $snapshot.Enabled) {
            Disable-ScheduledTask -TaskName $taskName -ErrorAction Stop | Out-Null
        }
    } elseif ($snapshot.Enabled) {
        Enable-ScheduledTask -TaskName $taskName -ErrorAction Stop | Out-Null
    } else {
        Disable-ScheduledTask -TaskName $taskName -ErrorAction Stop | Out-Null
    }
    $restored = Get-BrokerTaskSnapshot $taskName
    if (-not $restored.Present -or $restored.Enabled -ne $snapshot.Enabled -or
        $restored.Running -ne $snapshot.Running -or $restored.Sddl -cne $snapshot.Sddl -or
        (Get-NormalizedBrokerTaskXml $restored.Xml) -cne (Get-NormalizedBrokerTaskXml $snapshot.Xml)) {
        throw 'previous broker task state did not restore'
    }
}

function Prepare-BrokerCandidate($context) {
    $ownerToken = $context.OwnerSid -replace '[^A-Za-z0-9-]', '_'
    $context.RecoveryPrefix = "NeuronChromaBroker.recovery-$ownerToken-"
    $recoveryDirectory = Join-Path $context.ProgramFiles (
        $context.RecoveryPrefix + $PID + '-' + [guid]::NewGuid().ToString('N'))
    if (Test-Path -LiteralPath $recoveryDirectory) { throw 'broker recovery directory already exists' }
    New-Item -ItemType Directory -Path $recoveryDirectory -ErrorAction Stop | Out-Null
    $context.RecoveryDirectory = $recoveryDirectory
    $item = Get-Item -LiteralPath $context.RecoveryDirectory -Force -ErrorAction Stop
    if (-not $item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw 'broker recovery path is not a normal directory'
    }
    Set-ProtectedFileAcl $context.RecoveryDirectory $true
    Write-ProtectedRecord (Join-Path $context.RecoveryDirectory 'broker-install.pending') (
        "$($context.ExpectedSha256)`nSYSTEM startup`ntask-acl-v1`n$($context.OwnerSid)`n")
    $context.Candidate = Join-Path $context.RecoveryDirectory 'candidate.exe'
    Copy-Item -LiteralPath $context.Source -Destination $context.Candidate -ErrorAction Stop | Out-Null
    Assert-RegularFileIfPresent $context.Candidate
    Set-ProtectedFileAcl $context.Candidate $false
    if ((Get-FileHash -LiteralPath $context.Candidate -Algorithm SHA256).Hash -ine $context.ExpectedSha256) {
        throw 'staged broker binary hash changed before replacement'
    }
}

function Copy-ProtectedBrokerFile($source, $destination) {
    $sourceHash = (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash
    Copy-Item -LiteralPath $source -Destination $destination -ErrorAction Stop | Out-Null
    Assert-RegularFileIfPresent $destination
    Set-ProtectedFileAcl $destination $false
    if ((Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash -ine $sourceHash) {
        throw "broker recovery copy failed verification: $destination"
    }
    return $sourceHash
}

function Capture-BrokerPriorState($context) {
    Assert-RegularFileIfPresent $context.Executable
    Assert-ProtectedFileAcl $context.Executable
    Assert-RegularFileIfPresent $context.Receipt
    Assert-RegularFileIfPresent $context.Pending
    if (-not (Test-Path -LiteralPath $context.Executable -PathType Leaf)) {
        throw 'existing broker executable is missing; replacement stopped before task changes'
    }
    $record = if (Test-Path -LiteralPath $context.Receipt -PathType Leaf) {
        @(Get-Content -LiteralPath $context.Receipt -TotalCount 5)
    } else {
        @(Get-Content -LiteralPath $context.Pending -TotalCount 5)
    }
    if (-not (Test-OwnershipRecord $record) -or $record[3].Trim() -ine $context.OwnerSid) {
        throw 'existing broker ownership record cannot authenticate its executable'
    }
    $oldHash = (Get-FileHash -LiteralPath $context.Executable -Algorithm SHA256).Hash
    if ($oldHash -ine $record[0].Trim()) {
        throw 'existing broker executable does not match its ownership receipt'
    }
    $previousExe = Join-Path $context.RecoveryDirectory 'previous.exe'
    $null = Copy-ProtectedBrokerFile $context.Executable $previousExe
    if ((Get-FileHash -LiteralPath $previousExe -Algorithm SHA256).Hash -ine $oldHash) {
        throw 'existing broker executable changed while its recovery copy was made'
    }
    $receiptPresent = Test-Path -LiteralPath $context.Receipt -PathType Leaf
    $pendingPresent = Test-Path -LiteralPath $context.Pending -PathType Leaf
    $previousReceipt = Join-Path $context.RecoveryDirectory 'previous.receipt'
    $previousPending = Join-Path $context.RecoveryDirectory 'previous.pending'
    $receiptHash = if ($receiptPresent) { Copy-ProtectedBrokerFile $context.Receipt $previousReceipt } else { $null }
    $pendingHash = if ($pendingPresent) { Copy-ProtectedBrokerFile $context.Pending $previousPending } else { $null }
    $task = Get-BrokerTaskSnapshot $context.TaskName
    if ($task.Present) {
        $taskXml = Join-Path $context.RecoveryDirectory 'previous-task.xml'
        $taskSddl = Join-Path $context.RecoveryDirectory 'previous-task.sddl'
        [IO.File]::WriteAllText($taskXml, $task.Xml, [Text.Encoding]::Unicode)
        [IO.File]::WriteAllText($taskSddl, $task.Sddl, [Text.UTF8Encoding]::new($false))
        Set-ProtectedFileAcl $taskXml $false
        Set-ProtectedFileAcl $taskSddl $false
    }
    $state = @{
        ExecutableHash = $oldHash
        ReceiptHash = $receiptHash
        PendingHash = $pendingHash
        ReceiptPresent = $receiptPresent
        PendingPresent = $pendingPresent
        Task = $task
    }
    $statePath = Join-Path $context.RecoveryDirectory 'previous-state.json'
    [IO.File]::WriteAllText($statePath, ($state | ConvertTo-Json -Compress -Depth 4), [Text.UTF8Encoding]::new($false))
    Set-ProtectedFileAcl $statePath $false
    $context.PriorState = @{
        ExecutableHash = $oldHash
        ReceiptHash = $receiptHash
        PendingHash = $pendingHash
        Executable = $previousExe
        ReceiptPresent = $receiptPresent
        Receipt = $previousReceipt
        PendingPresent = $pendingPresent
        Pending = $previousPending
        Task = $task
    }
}

function Stop-BrokerTask($context, [bool]$verifyPrior = $true) {
    $task = Get-BrokerTaskSnapshot $context.TaskName
    if ($verifyPrior) {
        $previous = $context.PriorState.Task
        if ($task.Present -ne $previous.Present) {
            throw 'broker task changed while preparing replacement'
        }
        if (-not $previous.Present) { return }
        if ($task.Enabled -ne $previous.Enabled -or $task.Running -ne $previous.Running -or
            $task.Sddl -cne $previous.Sddl -or
            (Get-NormalizedBrokerTaskXml $task.Xml) -cne (Get-NormalizedBrokerTaskXml $previous.Xml)) {
            throw 'broker task changed while preparing replacement'
        }
    } elseif (-not $task.Present) {
        return
    }
    if ($verifyPrior) { $context.TaskStopAttempted = $true }
    Disable-ScheduledTask -TaskName $context.TaskName -ErrorAction Stop | Out-Null
    if ($task.Running) {
        Stop-ScheduledTask -TaskName $context.TaskName -ErrorAction SilentlyContinue
    }
    for ($attempt = 0; $attempt -lt 30; $attempt++) {
        if ((Get-ScheduledTask -TaskName $context.TaskName -ErrorAction Stop).State -ne 'Running') { return }
        Start-Sleep -Milliseconds 200
    }
    throw 'previous broker did not stop; task remains disabled'
}

function Restore-BrokerTaskRuntimeState($context) {
    $snapshot = $context.PriorState.Task
    $task = Get-OptionalScheduledTask $context.TaskName
    if (-not $snapshot.Present) {
        if ($task) { throw 'broker task appeared before file publication; preserving it' }
        return
    }
    if (-not $task) { throw 'previous broker task disappeared before file publication' }
    if ($snapshot.Running -and $task.State -ne 'Running') {
        Enable-ScheduledTask -TaskName $context.TaskName -ErrorAction Stop | Out-Null
        Start-ScheduledTask -TaskName $context.TaskName -ErrorAction Stop
        for ($attempt = 0; $attempt -lt 25; $attempt++) {
            if ((Get-ScheduledTask -TaskName $context.TaskName -ErrorAction Stop).State -eq 'Running') { break }
            Start-Sleep -Milliseconds 200
        }
        if ((Get-ScheduledTask -TaskName $context.TaskName -ErrorAction Stop).State -ne 'Running') {
            throw 'previous broker task did not resume during rollback'
        }
    } elseif (-not $snapshot.Running -and $task.State -eq 'Running') {
        Stop-ScheduledTask -TaskName $context.TaskName -ErrorAction Stop
        for ($attempt = 0; $attempt -lt 30; $attempt++) {
            if ((Get-ScheduledTask -TaskName $context.TaskName -ErrorAction Stop).State -ne 'Running') { break }
            Start-Sleep -Milliseconds 200
        }
        if ((Get-ScheduledTask -TaskName $context.TaskName -ErrorAction Stop).State -eq 'Running') {
            throw 'previous broker task did not stop during rollback'
        }
    }
    if ($snapshot.Enabled) {
        Enable-ScheduledTask -TaskName $context.TaskName -ErrorAction Stop | Out-Null
    } else {
        Disable-ScheduledTask -TaskName $context.TaskName -ErrorAction Stop | Out-Null
    }
    $restored = Get-BrokerTaskSnapshot $context.TaskName
    if ($restored.Enabled -ne $snapshot.Enabled -or $restored.Running -ne $snapshot.Running -or
        $restored.Sddl -cne $snapshot.Sddl -or
        (Get-NormalizedBrokerTaskXml $restored.Xml) -cne (Get-NormalizedBrokerTaskXml $snapshot.Xml)) {
        throw 'previous broker task runtime state did not restore'
    }
}

function Replace-BrokerFile($source, $destination) {
    if (Test-Path -LiteralPath $destination -PathType Leaf) {
        # PowerShell 5.1 binds $null as an empty backup path unless given NullString.
        [IO.File]::Replace($source, $destination, [NullString]::Value)
    } else {
        [IO.File]::Move($source, $destination)
    }
}

function Publish-BrokerCandidate($context) {
    Assert-RegularFileIfPresent $context.Candidate
    Replace-BrokerFile $context.Candidate $context.Executable
    Assert-RegularFileIfPresent $context.Executable
    Set-ProtectedFileAcl $context.Executable $false
    if ((Get-FileHash -LiteralPath $context.Executable -Algorithm SHA256).Hash -ine $context.ExpectedSha256) {
        throw 'broker binary hash changed during replacement'
    }
}

function Install-BrokerTask($context) {
    $action = New-ScheduledTaskAction -Execute $context.Executable -WorkingDirectory $context.Directory
    $trigger = New-ScheduledTaskTrigger -AtStartup
    $brokerPrincipal = New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest
    $settings = New-ScheduledTaskSettingsSet -Disable -ExecutionTimeLimit (New-TimeSpan -Seconds 0) -MultipleInstances IgnoreNew -StartWhenAvailable -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)
    Register-ScheduledTask -TaskName $context.TaskName -Action $action -Trigger $trigger -Principal $brokerPrincipal -Settings $settings -Force -ErrorAction Stop | Out-Null

    # Task Scheduler otherwise adds ACEs that could let a limited user edit the elevated action.
    $service = New-Object -ComObject 'Schedule.Service'
    $service.Connect()
    $registered = $service.GetFolder('\').GetTask($context.TaskName)
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
    $task = Get-ScheduledTask -TaskName $context.TaskName -ErrorAction Stop
    $principalId = [string]$task.Principal.UserId
    $principalSid = if ($principalId -match '^S-1-') { $principalId } else {
        ([Security.Principal.NTAccount]$principalId).Translate([Security.Principal.SecurityIdentifier]).Value
    }
    if ($task.Principal.RunLevel -ne 'Highest' -or $task.Actions.Count -ne 1 -or
        $task.Principal.LogonType -ne 'ServiceAccount' -or
        $principalSid -ne $system.Value -or
        $task.Triggers.Count -ne 1 -or $task.Triggers[0].CimClass.CimClassName -ne 'MSFT_TaskBootTrigger' -or
        $task.Actions[0].Execute -ine $context.Executable -or
        $task.Actions[0].Arguments -or
        $task.Actions[0].WorkingDirectory -ine $context.Directory) {
        throw 'broker task verification failed'
    }
    Enable-ScheduledTask -TaskName $context.TaskName -ErrorAction Stop | Out-Null
    Start-ScheduledTask -TaskName $context.TaskName -ErrorAction Stop
    for ($attempt = 0; $attempt -lt 25; $attempt++) {
        if ((Get-ScheduledTask -TaskName $context.TaskName -ErrorAction Stop).State -eq 'Running') { break }
        Start-Sleep -Milliseconds 200
    }
    if ((Get-ScheduledTask -TaskName $context.TaskName -ErrorAction Stop).State -ne 'Running') {
        Disable-ScheduledTask -TaskName $context.TaskName -ErrorAction SilentlyContinue | Out-Null
        throw 'broker task did not remain running'
    }
}

function Commit-BrokerReceipt($context) {
    Write-ProtectedRecord $context.Receipt (
        "$($context.ExpectedSha256)`nSYSTEM startup`ntask-acl-v1`n$($context.OwnerSid)`n")
    if (Test-Path -LiteralPath $context.Pending) {
        Remove-Item -LiteralPath $context.Pending -Force -ErrorAction Stop
    }
}

function Verify-BrokerReplacement($context, [bool]$receiptCommitted) {
    Assert-RegularFileIfPresent $context.Executable
    Assert-ProtectedFileAcl $context.Executable
    if ((Get-FileHash -LiteralPath $context.Executable -Algorithm SHA256).Hash -ine $context.ExpectedSha256) {
        throw 'installed broker binary changed before replacement commit'
    }
    if ($receiptCommitted) {
        Assert-ProtectedFileAcl $context.Receipt
        $record = @(Get-Content -LiteralPath $context.Receipt -TotalCount 5)
        if (-not (Test-OwnershipRecord $record) -or
            $record[0].Trim() -ine $context.ExpectedSha256 -or
            $record[3].Trim() -ine $context.OwnerSid -or
            (Test-Path -LiteralPath $context.Pending)) {
            throw 'broker receipt did not commit the verified executable'
        }
    }
    if ((Get-ScheduledTask -TaskName $context.TaskName -ErrorAction Stop).State -ne 'Running') {
        throw 'broker task stopped before replacement commit'
    }
}

function Restore-BrokerFile($backup, $destination, [bool]$present, $expectedHash) {
    if (-not $present) {
        if (Test-Path -LiteralPath $destination) {
            Remove-Item -LiteralPath $destination -Force -ErrorAction Stop
        }
        if (Test-Path -LiteralPath $destination) { throw "broker state was not removed: $destination" }
        return
    }
    $temp = "$destination.restore-$PID-$([guid]::NewGuid().ToString('N'))"
    try {
        if ((Get-FileHash -LiteralPath $backup -Algorithm SHA256).Hash -ine $expectedHash) {
            throw "broker recovery copy changed: $backup"
        }
        Copy-Item -LiteralPath $backup -Destination $temp -ErrorAction Stop | Out-Null
        Set-ProtectedFileAcl $temp $false
        if ((Get-FileHash -LiteralPath $temp -Algorithm SHA256).Hash -ine $expectedHash) {
            throw "broker recovery copy failed verification: $destination"
        }
        Replace-BrokerFile $temp $destination
        Set-ProtectedFileAcl $destination $false
        Assert-ProtectedFileAcl $destination
        if ((Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash -ine $expectedHash) {
            throw "restored broker file failed verification: $destination"
        }
    } finally {
        Remove-Item -LiteralPath $temp -Force -ErrorAction SilentlyContinue
    }
}

function Restore-BrokerState($context) {
    Stop-BrokerTask $context $false
    Restore-BrokerFile $context.PriorState.Executable $context.Executable $true $context.PriorState.ExecutableHash
    if ((Get-FileHash -LiteralPath $context.Executable -Algorithm SHA256).Hash -ine $context.PriorState.ExecutableHash) {
        throw 'restored broker executable does not match its recovery copy'
    }
    Restore-BrokerFile $context.PriorState.Receipt $context.Receipt $context.PriorState.ReceiptPresent $context.PriorState.ReceiptHash
    Restore-BrokerFile $context.PriorState.Pending $context.Pending $context.PriorState.PendingPresent $context.PriorState.PendingHash
    Restore-BrokerTaskSnapshot $context.TaskName $context.PriorState.Task
}

function Cleanup-BrokerTransaction($context) {
    if (-not $context.RecoveryDirectory) { return }
    $item = Get-Item -LiteralPath $context.RecoveryDirectory -Force -ErrorAction SilentlyContinue
    if (-not $item) { return }
    if (-not $item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -or
        -not $item.Name.StartsWith($context.RecoveryPrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'broker recovery directory identity changed; preserving it'
    }
    $marker = Join-Path $context.RecoveryDirectory 'broker-install.pending'
    if (-not (Test-Path -LiteralPath $marker -PathType Leaf)) { throw 'broker recovery ownership marker is missing; preserving it' }
    $markerLines = @(Get-Content -LiteralPath $marker -TotalCount 5)
    if (-not (Test-OwnershipRecord $markerLines) -or $markerLines[3].Trim() -ine $context.OwnerSid -or
        $markerLines[0].Trim() -ine $context.ExpectedSha256) {
        throw 'broker recovery ownership marker changed; preserving it'
    }
    Assert-ProtectedFileAcl $context.RecoveryDirectory
    Assert-ProtectedFileAcl $marker
    $allowed = @(
        'broker-install.pending', 'candidate.exe', 'previous.exe', 'previous.receipt',
        'previous.pending', 'previous-task.xml', 'previous-task.sddl', 'previous-state.json'
    )
    $children = @(Get-ChildItem -LiteralPath $context.RecoveryDirectory -Force -ErrorAction Stop)
    foreach ($child in $children) {
        if ($child.PSIsContainer -or ($child.Attributes -band [IO.FileAttributes]::ReparsePoint) -or
            $child.Name -notin $allowed) {
            throw 'broker recovery directory contains unknown state; preserving it'
        }
        Assert-ProtectedFileAcl $child.FullName
    }
    foreach ($child in $children) {
        Remove-Item -LiteralPath $child.FullName -Force -ErrorAction Stop
    }
    Remove-Item -LiteralPath $context.RecoveryDirectory -Force -ErrorAction Stop
}

function Install-ProtectedBrokerReplacement($Source, $ExpectedSha256, $OwnerSid, $ProgramFiles,
    $Directory, $Executable, $Receipt, $Pending, $TaskName) {
    $context = @{
        Source = $Source
        ExpectedSha256 = $ExpectedSha256
        OwnerSid = $OwnerSid
        ProgramFiles = $ProgramFiles
        Directory = $Directory
        Executable = $Executable
        Receipt = $Receipt
        Pending = $Pending
        TaskName = $TaskName
        TaskStopAttempted = $false
    }
    $priorCaptured = $false
    $publicationAttempted = $false
    try {
        Prepare-BrokerCandidate $context
        Capture-BrokerPriorState $context
        $priorCaptured = $true
        Stop-BrokerTask $context
        $publicationAttempted = $true
        Publish-BrokerCandidate $context
        Install-BrokerTask $context
        Verify-BrokerReplacement $context $false
        Commit-BrokerReceipt $context
        Verify-BrokerReplacement $context $true
    } catch {
        $failure = $_
        if ($priorCaptured) {
            try {
                if ($publicationAttempted) {
                    Restore-BrokerState $context
                } elseif ($context.TaskStopAttempted) {
                    Restore-BrokerTaskRuntimeState $context
                }
            } catch {
                throw "broker replacement failed: $($failure.Exception.Message); rollback failed: $($_.Exception.Message); recovery retained at $($context.RecoveryDirectory)"
            }
        }
        try {
            Cleanup-BrokerTransaction $context
        } catch {
            throw "broker replacement failed: $($failure.Exception.Message); recovery state retained at $($context.RecoveryDirectory): $($_.Exception.Message)"
        }
        throw $failure
    }
    try {
        Cleanup-BrokerTransaction $context
    } catch {
        Write-Warning "broker replacement is verified, but recovery cleanup failed; preserved at $($context.RecoveryDirectory): $($_.Exception.Message)"
    }
}

$ownerToken = $OwnerSid -replace '[^A-Za-z0-9-]', '_'
$stagingPrefix = "NeuronChromaBroker.installing-$ownerToken-"
$removingPrefix = "NeuronChromaBroker.removing-$ownerToken-"
$recoveryPrefix = 'NeuronChromaBroker.recovery-'
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
$recoveryState = @(Get-ChildItem -LiteralPath $programFiles64 -Force -ErrorAction Stop |
    Where-Object { $_.Name.StartsWith($recoveryPrefix, [StringComparison]::OrdinalIgnoreCase) })
if ($recoveryState.Count -gt 0) {
    throw "an interrupted broker replacement needs inspection; preserving $($recoveryState[0].FullName)"
}

$dirPresent = Test-Path -LiteralPath $dir
$existingInstall = $dirPresent
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

if ($existingInstall) {
    Install-ProtectedBrokerReplacement $source $ExpectedSha256 $OwnerSid $programFiles64 $dir $exe $receipt $pending $taskName
} else {
    Assert-RegularFileIfPresent $exe
    Set-ProtectedFileAcl $exe $false
    if ((Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash -ine $ExpectedSha256) {
        throw 'staged broker binary hash changed before task installation'
    }
    $context = @{
        ExpectedSha256 = $ExpectedSha256
        OwnerSid = $OwnerSid
        Directory = $dir
        Executable = $exe
        TaskName = $taskName
        Receipt = $receipt
        Pending = $pending
    }
    Install-BrokerTask $context
    Verify-BrokerReplacement $context $false
    Commit-BrokerReceipt $context
    Verify-BrokerReplacement $context $true
}
Write-Output "installed: $exe"
Write-Output "sha256: $ExpectedSha256"
Write-Output "task: $taskName"
} finally {
    if ($transactionHeld) { $transactionMutex.ReleaseMutex() }
    $transactionMutex.Dispose()
}
