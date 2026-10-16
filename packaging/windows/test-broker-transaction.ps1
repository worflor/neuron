# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
# Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

# Offline fixtures for broker replacement and first-install repair. Only bounded temporary files,
# fake Task Scheduler/ACL commands, and a controlled process-image query are used; the installation
# script's top level is never run.
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$Script:FixtureScriptPath = [IO.Path]::GetFullPath($PSCommandPath)
$ProductionPath = Join-Path $PSScriptRoot 'install-chroma-broker.ps1'
$utilityModule = [IO.Path]::Combine($PSHOME, 'Modules', 'Microsoft.PowerShell.Utility', 'Microsoft.PowerShell.Utility.psd1')
if (-not [IO.File]::Exists($utilityModule)) { throw "PowerShell Utility module is missing: $utilityModule" }
Import-Module $utilityModule -ErrorAction Stop
$Script:Assertions = 0
$Script:TaskName = 'Neuron Chroma broker'
$Script:OwnerSid = 'S-1-5-21-111111111-222222222-333333333-1001'
$Script:admins = [Security.Principal.SecurityIdentifier]::new('S-1-5-32-544')
$Script:system = [Security.Principal.SecurityIdentifier]::new('S-1-5-18')
$Script:users = [Security.Principal.SecurityIdentifier]::new('S-1-5-32-545')
$Script:FixtureParent = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd([char[]]@('\','/'))
$Script:FixtureRoot = Join-Path $Script:FixtureParent ('neuron-broker-transaction-' + [guid]::NewGuid().ToString('N'))

function Assert($Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
    $Script:Assertions++
}

function Assert-MockBindings {
    $mockCommands = @(
        'Test-Path','Get-Item','Get-ChildItem','New-Item','Copy-Item','Get-FileHash',
        'Remove-Item','Get-Content','Set-ProtectedFileAcl','Assert-ProtectedFileAcl','Get-ScheduledTask',
        'Export-ScheduledTask','Disable-ScheduledTask','Enable-ScheduledTask','Stop-ScheduledTask',
        'Start-ScheduledTask','Unregister-ScheduledTask','Register-ScheduledTask',
        'New-ScheduledTaskAction','New-ScheduledTaskTrigger','New-ScheduledTaskPrincipal',
        'New-ScheduledTaskSettingsSet','Start-Sleep','New-Object','Replace-BrokerFile',
        'Get-BrokerProcessImagePaths'
    )
    foreach ($name in $mockCommands) {
        $command = Get-Command -Name $name -ErrorAction Stop
        if ($command.CommandType -ne 'Function' -or
            [IO.Path]::GetFullPath($command.ScriptBlock.File) -ine $Script:FixtureScriptPath) {
            throw "Fixture command $name did not resolve to this script's mock function."
        }
    }
}

function Add-FixtureEvent([string]$Name) { [void]$Script:Events.Add($Name) }
function Test-FixtureFault([string]$Name) { return ($Script:Fault -eq $Name -or $Script:SecondaryFaults -contains $Name) }

function Assert-PrivatePath([string]$Path) {
    if (-not $Path) { throw 'Fixture operation received an empty path.' }
    $full = [IO.Path]::GetFullPath($Path)
    if (-not ($full.Equals($Script:FixtureRoot, [StringComparison]::OrdinalIgnoreCase) -or
        $full.StartsWith($Script:FixtureRoot + '\', [StringComparison]::OrdinalIgnoreCase))) {
        throw "Fixture refused an out-of-root path: $full"
    }
    $cursor = $full
    while ($cursor.StartsWith($Script:FixtureRoot, [StringComparison]::OrdinalIgnoreCase)) {
        if ([IO.File]::Exists($cursor) -or [IO.Directory]::Exists($cursor)) {
            if (([IO.File]::GetAttributes($cursor) -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "Fixture refused a reparse point: $cursor"
            }
        }
        if ($cursor.Equals($Script:FixtureRoot, [StringComparison]::OrdinalIgnoreCase)) { break }
        $cursor = [IO.Path]::GetDirectoryName($cursor)
    }
    return $full
}

function Get-TestHash([string]$Path) {
    $full = Assert-PrivatePath $Path
    $sha = [Security.Cryptography.SHA256]::Create()
    try {
        $bytes = [IO.File]::ReadAllBytes($full)
        return [BitConverter]::ToString($sha.ComputeHash($bytes)).Replace('-', '').ToLowerInvariant()
    } finally { $sha.Dispose() }
}

function Get-FakeTaskObject {
    if (-not $Script:BrokerTask.Present) { return $null }
    $state = if ($Script:BrokerTask.Running) { 'Running' } elseif ($Script:BrokerTask.Enabled) { 'Ready' } else { 'Disabled' }
    return [pscustomobject]@{
        TaskName=$Script:TaskName
        TaskPath='\'
        State=$state
        Principal=$Script:BrokerTask.Principal
        Actions=@($Script:BrokerTask.Actions)
        Triggers=@($Script:BrokerTask.Triggers)
    }
}

function New-FakeRegisteredTask {
    $registered = [pscustomobject]@{}
    $registered | Add-Member -MemberType ScriptProperty -Name Enabled -Value {
        [bool]$Script:BrokerTask.Enabled
    }
    $registered | Add-Member -MemberType ScriptMethod -Name GetSecurityDescriptor -Value {
        param($Flags)
        Add-FixtureEvent "GetSD:$Flags"
        return [string]$Script:BrokerTask.Sddl
    }
    $registered | Add-Member -MemberType ScriptMethod -Name SetSecurityDescriptor -Value {
        param($Sddl, $Flags)
        Add-FixtureEvent "SetSD:$Flags"
        $Script:BrokerTask.Sddl = [string]$Sddl
    }
    return $registered
}

function New-FakeTaskService {
    $folder = [pscustomobject]@{}
    $folder | Add-Member -MemberType ScriptMethod -Name GetTask -Value {
        param($Name)
        if ($Name -ne $Script:TaskName -or -not $Script:BrokerTask.Present) { throw 'fixture task missing' }
        return New-FakeRegisteredTask
    }
    $service = [pscustomobject]@{}
    $service | Add-Member -MemberType ScriptMethod -Name Connect -Value { Add-FixtureEvent 'ComConnect' }
    $service | Add-Member -MemberType ScriptMethod -Name GetFolder -Value {
        param($Path)
        if ($Path -ne '\') { throw "unexpected Task Scheduler folder: $Path" }
        return $folder
    }.GetNewClosure()
    return $service
}

function Import-ProductionFunction([string]$Name, [string]$Alias) {
    $errors = $null
    $tokens = $null
    $ast = [Management.Automation.Language.Parser]::ParseFile($ProductionPath, [ref]$tokens, [ref]$errors)
    if ($errors.Count -gt 0) { throw ($errors | ForEach-Object { $_.Message } | Out-String) }
    $definition = $ast.Find({ param($node)
        $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $Name
    }, $false)
    if (-not $definition) { throw "Missing production function $Name." }
    $definitionText = $definition.Extent.Text -replace ('^function\s+' + [regex]::Escape($Name)), ('function script:' + $Alias)
    . ([scriptblock]::Create($definitionText))
}

function Assert-ByteFile([string]$Path, [byte[]]$Expected, [string]$Label) {
    Assert-PrivatePath $Path | Out-Null
    if (-not [IO.File]::Exists($Path)) { throw "$Label is missing: $Path" }
    $actual = [IO.File]::ReadAllBytes($Path)
    if ([Convert]::ToBase64String($actual) -cne [Convert]::ToBase64String($Expected)) { throw "$Label bytes changed: $Path" }
    $Script:Assertions++
}

function Assert-NoFixtureReparseRecursively([string]$Path) {
    $root = Assert-PrivatePath $Path
    $pending = [Collections.Generic.Stack[string]]::new()
    $pending.Push($root)
    while ($pending.Count -gt 0) {
        $current = $pending.Pop()
        foreach ($entry in [IO.Directory]::GetFileSystemEntries($current)) {
            $full = Assert-PrivatePath $entry
            $attributes = [IO.File]::GetAttributes($full)
            if (($attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "Fixture cleanup found a reparse point: $full"
            }
            if (($attributes -band [IO.FileAttributes]::Directory) -ne 0) { $pending.Push($full) }
        }
    }
}

function Get-RecoveryDirectories {
    if (-not [IO.Directory]::Exists($Script:ProgramFiles)) { return @() }
    return @([IO.Directory]::GetDirectories($Script:ProgramFiles) | Where-Object {
        [IO.Path]::GetFileName($_).StartsWith('NeuronChromaBroker.recovery-', [StringComparison]::OrdinalIgnoreCase)
    })
}

function Assert-OriginalReplacementState([string]$Description) {
    Assert-ByteFile $Script:Executable $Script:OldBinary "$Description executable"
    if ($Script:OldReceiptPresent) {
        Assert-ByteFile $Script:Receipt $Script:OldReceipt "$Description receipt"
    } else {
        Assert (-not [IO.File]::Exists($Script:Receipt)) "$Description unexpectedly created a receipt"
    }
    if ($Script:OldPendingPresent) {
        Assert-ByteFile $Script:Pending $Script:OldPending "$Description pending record"
    } else {
        Assert (-not [IO.File]::Exists($Script:Pending)) "$Description did not remove the transaction pending record"
    }
    Assert ([bool]$Script:BrokerTask.Present) "$Description broker task disappeared"
    Assert ((Get-NormalizedBrokerTaskXml $Script:BrokerTask.Xml) -ceq (Get-NormalizedBrokerTaskXml $Script:OldTask.Xml)) "$Description task XML changed"
    Assert ($Script:BrokerTask.Sddl -ceq $Script:OldTask.Sddl) "$Description task security descriptor changed"
    Assert ([bool]$Script:BrokerTask.Enabled -eq [bool]$Script:OldTask.Enabled) "$Description task enabled state changed: actual=$($Script:BrokerTask.Enabled) expected=$($Script:OldTask.Enabled); error=$Script:LastReplacementMessage; events=$($Script:Events -join ',')"
    Assert ([bool]$Script:BrokerTask.Running -eq [bool]$Script:OldTask.Running) "$Description task running state changed: actual=$($Script:BrokerTask.Running) expected=$($Script:OldTask.Running); events=$($Script:Events -join ',')"
    Assert ($Script:AclState[$Script:Executable] -eq 'file-protected') "$Description executable ACL marker changed"
    if ($Script:OldReceiptPresent) {
        Assert ($Script:AclState[$Script:Receipt] -eq 'file-protected') "$Description receipt ACL marker changed"
    } else {
        Assert (-not $Script:AclState.ContainsKey($Script:Receipt)) "$Description unexpectedly retained a receipt ACL marker"
    }
    if ($Script:OldPendingPresent) {
        Assert ($Script:AclState[$Script:Pending] -eq 'file-protected') "$Description pending ACL marker changed"
    } else {
        Assert (-not $Script:AclState.ContainsKey($Script:Pending)) "$Description unexpectedly retained a pending ACL marker"
    }
    Assert ((ConvertTo-Json -InputObject $Script:ForeignTask -Compress -Depth 6) -ceq $Script:ForeignTaskJson) "$Description touched the unrelated task"
}

function Invoke-Replacement([string]$Failure) {
    $Script:Fault = $Failure
    $Script:FaultTriggered = $false
    try {
        Install-ProtectedBrokerReplacement $Script:Source $Script:ExpectedHash $Script:OwnerSid `
            $Script:ProgramFiles $Script:Directory $Script:Executable $Script:Receipt $Script:Pending $Script:TaskName
        return [pscustomobject]@{ Failed=$false; Message='' }
    } catch {
        $Script:LastReplacementMessage = $_.Exception.Message
        return [pscustomobject]@{ Failed=$true; Message=$_.Exception.Message }
    }
}

function Initialize-Fixture([string]$Name, [bool]$OldState = $true, [bool]$Enabled = $true, [bool]$Running = $true) {
    $Script:Root = Join-Path $Script:FixtureRoot $Name
    [IO.Directory]::CreateDirectory($Script:Root) | Out-Null
    $Script:ProgramFiles = Join-Path $Script:Root 'ProgramFiles'
    [IO.Directory]::CreateDirectory($Script:ProgramFiles) | Out-Null
    $Script:Directory = Join-Path $Script:ProgramFiles 'NeuronChromaBroker'
    $Script:Executable = Join-Path $Script:Directory 'neuron-chroma-broker.exe'
    $Script:Receipt = Join-Path $Script:Directory 'broker-installed.sha256'
    $Script:Pending = Join-Path $Script:Directory 'broker-install.pending'
    $Script:Source = Join-Path $Script:Root 'candidate-source.exe'
    $Script:OldBinary = [Text.Encoding]::UTF8.GetBytes("trusted-old-broker-$Name")
    $Script:NewBinary = [Text.Encoding]::UTF8.GetBytes("candidate-broker-$Name")
    $Script:ForeignTask = [ordered]@{ TaskName='Unrelated task'; Xml='<Task>foreign</Task>'; Sddl='FOREIGN-SDDL'; Enabled=$true; Running=$false }
    $Script:ForeignTaskJson = ConvertTo-Json -InputObject $Script:ForeignTask -Compress -Depth 6
    $Script:Events = [Collections.ArrayList]::new()
    $Script:AclState = @{}
    $Script:Fault = ''
    $Script:SecondaryFaults = @()
    $Script:FaultTriggered = $false
    $Script:ProcessImagePaths = @()
    $Script:ProcessQueryFailure = $false
    $Script:PendingRepairBytes = $null
    $Script:PendingRepairReceiptBytes = $null
    $Script:PendingRepairTaskJson = $null
    $Script:PendingRefreshTempPath = $null
    Assert-MockBindings
    [IO.File]::WriteAllBytes($Script:Source, $Script:NewBinary)
    $Script:ExpectedHash = Get-TestHash $Script:Source
    if ($OldState) {
        [IO.Directory]::CreateDirectory($Script:Directory) | Out-Null
        [IO.File]::WriteAllBytes($Script:Executable, $Script:OldBinary)
        $receiptText = "$((Get-TestHash $Script:Executable))`nSYSTEM startup`ntask-acl-v1`n$Script:OwnerSid`n"
        $Script:OldReceipt = [Text.Encoding]::UTF8.GetBytes($receiptText)
        $Script:OldReceiptPresent = $true
        $Script:OldPending = [byte[]]@()
        $Script:OldPendingPresent = $false
        [IO.File]::WriteAllBytes($Script:Receipt, $Script:OldReceipt)
        $Script:AclState[$Script:Directory] = 'directory-protected'
        $Script:AclState[$Script:Executable] = 'file-protected'
        $Script:AclState[$Script:Receipt] = 'file-protected'
        $Script:BrokerTask = [ordered]@{
            Present=$true
            Xml='<?xml version="1.0" encoding="UTF-16"?><Task version="1.3"><Settings></Settings><Triggers><BootTrigger /></Triggers><Actions><Exec><Command>C:\Program Files\NeuronChromaBroker\neuron-chroma-broker.exe</Command></Exec></Actions></Task>'
            Sddl='O:BA:D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FR;;;BU)'
            Enabled=$Enabled
            Running=$Running
            Principal=[pscustomobject]@{ UserId='SYSTEM'; LogonType='ServiceAccount'; RunLevel='Highest' }
            Actions=@([pscustomobject]@{ Execute=$Script:Executable; WorkingDirectory=$Script:Directory; Arguments='' })
            Triggers=@([pscustomobject]@{ CimClass=[pscustomobject]@{ CimClassName='MSFT_TaskBootTrigger' } })
        }
        $Script:OldTask = [pscustomobject]@{
            Present=$true; Xml=$Script:BrokerTask.Xml; Sddl=$Script:BrokerTask.Sddl; Enabled=$Enabled; Running=$Running
        }
    } else {
        [IO.Directory]::CreateDirectory($Script:Directory) | Out-Null
        $Script:OldBinary = [byte[]]@()
        $Script:OldReceipt = [byte[]]@()
        $Script:OldReceiptPresent = $false
        $Script:OldPending = [byte[]]@()
        $Script:OldPendingPresent = $false
        $Script:BrokerTask = [ordered]@{ Present=$false; Xml=''; Sddl=''; Enabled=$false; Running=$false; Principal=$null; Actions=@(); Triggers=@() }
        $Script:OldTask = [pscustomobject]@{ Present=$false; Xml=$null; Sddl=$null; Enabled=$false; Running=$false }
        $Script:AclState[$Script:Directory] = 'directory-protected'
    }
}

function Set-PriorPendingOnly {
    if ([IO.File]::Exists($Script:Receipt)) { [IO.File]::Delete($Script:Receipt) }
    $Script:AclState.Remove($Script:Receipt)
    $pendingText = "$((Get-TestHash $Script:Executable))`nSYSTEM startup`ntask-acl-v1`n$Script:OwnerSid`n"
    $Script:OldPending = [Text.Encoding]::UTF8.GetBytes($pendingText)
    $Script:OldPendingPresent = $true
    $Script:OldReceipt = [byte[]]@()
    $Script:OldReceiptPresent = $false
    [IO.File]::WriteAllBytes($Script:Pending, $Script:OldPending)
    $Script:AclState[$Script:Pending] = 'file-protected'
}

function Set-PendingRepairRecord([string]$Kind = 'valid') {
    $hash = $Script:ExpectedHash
    $owner = $Script:OwnerSid
    if ($Kind -eq 'malformed') { $text = "not-a-hash`nwrong kind`nwrong ACL`n$owner`n" }
    elseif ($Kind -eq 'foreign') { $text = "$hash`nSYSTEM startup`ntask-acl-v1`nS-1-5-21-999999999-999999999-999999999-1001`n" }
    elseif ($Kind -eq 'old-hash') {
        $previousCandidate = Join-Path $Script:Root 'previous-candidate.exe'
        [IO.File]::WriteAllBytes($previousCandidate, [Text.Encoding]::UTF8.GetBytes('previous trusted candidate'))
        $hash = Get-TestHash $previousCandidate
        [IO.File]::Delete($previousCandidate)
        $text = "$hash`nSYSTEM startup`ntask-acl-v1`n$owner`n"
    }
    else { $text = "$hash`nSYSTEM startup`ntask-acl-v1`n$owner`n" }
    $Script:PendingRepairBytes = [Text.Encoding]::UTF8.GetBytes($text)
    [IO.File]::WriteAllBytes($Script:Pending, $Script:PendingRepairBytes)
    $Script:AclState[$Script:Pending] = 'file-protected'
}

function Set-PendingRepairReceipt {
    $text = "$Script:ExpectedHash`nSYSTEM startup`ntask-acl-v1`n$Script:OwnerSid`n"
    $Script:PendingRepairReceiptBytes = [Text.Encoding]::UTF8.GetBytes($text)
    [IO.File]::WriteAllBytes($Script:Receipt, $Script:PendingRepairReceiptBytes)
    $Script:AclState[$Script:Receipt] = 'file-protected'
}

function Set-PendingRepairTask {
    $Script:BrokerTask = [ordered]@{
        Present=$true
        Xml='<?xml version="1.0" encoding="UTF-16"?><Task version="1.3"><Settings></Settings><Triggers><BootTrigger /></Triggers><Actions><Exec><Command>C:\Program Files\NeuronChromaBroker\neuron-chroma-broker.exe</Command></Exec></Actions></Task>'
        Sddl='O:BA:D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FR;;;BU)'
        Enabled=$true
        Running=$false
        Principal=[pscustomobject]@{ UserId='SYSTEM'; LogonType='ServiceAccount'; RunLevel='Highest' }
        Actions=@([pscustomobject]@{ Execute=$Script:Executable; WorkingDirectory=$Script:Directory; Arguments='' })
        Triggers=@([pscustomobject]@{ CimClass=[pscustomobject]@{ CimClassName='MSFT_TaskBootTrigger' } })
    }
    $Script:PendingRepairTaskJson = ConvertTo-Json -InputObject $Script:BrokerTask -Compress -Depth 8
}

function Get-StagingDirectories {
    if (-not [IO.Directory]::Exists($Script:ProgramFiles)) { return @() }
    return @([IO.Directory]::GetDirectories($Script:ProgramFiles) | Where-Object {
        Assert-PrivatePath $_ | Out-Null
        [IO.Path]::GetFileName($_).StartsWith('NeuronChromaBroker.installing-', [StringComparison]::OrdinalIgnoreCase)
    })
}

function Invoke-PendingRepair {
    try {
        Install-PendingBrokerRepair $Script:Source $Script:ExpectedHash $Script:OwnerSid `
            $Script:ProgramFiles $Script:Directory $Script:Executable $Script:Receipt $Script:Pending $Script:TaskName
        return [pscustomobject]@{ Failed=$false; Message='' }
    } catch {
        return [pscustomobject]@{ Failed=$true; Message=$_.Exception.Message }
    }
}

function Assert-PendingRepairUnchanged([string]$Description, [string]$RecordKind = 'pending') {
    Assert (-not [IO.File]::Exists($Script:Executable)) "$Description created an executable"
    if ($RecordKind -eq 'receipt') {
        Assert-ByteFile $Script:Receipt $Script:PendingRepairReceiptBytes "$Description receipt"
        Assert (-not [IO.File]::Exists($Script:Pending)) "$Description created a pending record"
    } else {
        Assert-ByteFile $Script:Pending $Script:PendingRepairBytes "$Description pending record"
        Assert (-not [IO.File]::Exists($Script:Receipt)) "$Description created a receipt"
    }
    $expectedName = if ($RecordKind -eq 'receipt') { 'broker-installed.sha256' } else { 'broker-install.pending' }
    $children = @(Get-ChildItem -LiteralPath $Script:Directory -Force)
    Assert ($children.Count -eq 1 -and $children[0].Name -ceq $expectedName) "$Description left an unexpected partial or auxiliary file"
    Assert ((Get-StagingDirectories).Count -eq 0) "$Description left a staging directory"
    Assert ((Get-RecoveryDirectories).Count -eq 0) "$Description left recovery state"
    Assert ((ConvertTo-Json -InputObject $Script:ForeignTask -Compress -Depth 6) -ceq $Script:ForeignTaskJson) "$Description touched the unrelated task"
    if ($Script:PendingRepairTaskJson) {
        Assert ((ConvertTo-Json -InputObject $Script:BrokerTask -Compress -Depth 8) -ceq $Script:PendingRepairTaskJson) "$Description changed the existing broker task"
    } else {
        Assert (-not $Script:BrokerTask.Present) "$Description registered or changed a broker task"
    }
}

function Assert-PendingRefreshTempStayedInStage([string]$Description) {
    $temp = Assert-PrivatePath $Script:PendingRefreshTempPath
    $stage = [IO.Path]::GetDirectoryName($temp)
    Assert ([IO.Path]::GetDirectoryName($stage) -ieq $Script:ProgramFiles -and
        [IO.Path]::GetFileName($stage).StartsWith('NeuronChromaBroker.installing-', [StringComparison]::OrdinalIgnoreCase)) "$Description used a pending-record temp outside its stage directory"
    Assert ($stage -ine $Script:Directory) "$Description placed a partial file in the final broker directory"
    Assert (-not [IO.File]::Exists($temp)) "$Description left its staged pending-record temp behind"
}

function Assert-PendingRepairSucceeded([string]$Description) {
    Assert-ByteFile $Script:Executable $Script:NewBinary "$Description executable"
    Assert-ProtectedFileAcl $Script:Executable
    Assert ([IO.File]::Exists($Script:Receipt)) "$Description receipt is missing"
    Assert (-not [IO.File]::Exists($Script:Pending)) "$Description left its pending record"
    Assert-ProtectedFileAcl $Script:Receipt
    $lines = [IO.File]::ReadAllLines($Script:Receipt)
    Assert ($lines.Count -eq 4 -and $lines[0] -ieq $Script:ExpectedHash -and
        $lines[1] -eq 'SYSTEM startup' -and $lines[2] -eq 'task-acl-v1' -and
        $lines[3] -eq $Script:OwnerSid) "$Description receipt does not authenticate the installed executable"
    Assert ($Script:BrokerTask.Present -and $Script:BrokerTask.Enabled -and $Script:BrokerTask.Running) "$Description task is not installed, enabled, and running"
    Assert ($Script:BrokerTask.Actions[0].Execute -ieq $Script:Executable -and
        $Script:BrokerTask.Actions[0].WorkingDirectory -ieq $Script:Directory) "$Description task points outside the protected install path"
    Assert ((Get-StagingDirectories).Count -eq 0) "$Description left a staging directory"
    Assert ((Get-RecoveryDirectories).Count -eq 0) "$Description left recovery state"
    Assert (($Script:Events | Where-Object { $_ -eq 'ProcessQuery' }).Count -eq 2) "$Description did not check process ownership before and after staging"
    Assert ((ConvertTo-Json -InputObject $Script:ForeignTask -Compress -Depth 6) -ceq $Script:ForeignTaskJson) "$Description touched the unrelated task"
}

function Test-PendingRepairRejected([string]$Name, [string]$RecordKind, [string]$Blocker = 'none', [bool]$ReceiptPresent = $false) {
    Initialize-Fixture $Name $false
    if ($ReceiptPresent) { Set-PendingRepairReceipt }
    else { Set-PendingRepairRecord $RecordKind }
    if ($Blocker -eq 'task') { Set-PendingRepairTask }
    elseif ($Blocker -eq 'process') { $Script:ProcessImagePaths = @($Script:Executable) }
    elseif ($Blocker -eq 'query-fails') { $Script:ProcessQueryFailure = $true }
    $result = Invoke-PendingRepair
    Assert $result.Failed "$Name was not rejected"
    $recordKind = if ($ReceiptPresent) { 'receipt' } else { 'pending' }
    Assert-PendingRepairUnchanged $Name $recordKind
    Assert (@($Script:Events | Where-Object { $_ -like 'Copy:*' }).Count -eq 0) "$Name copied a candidate before rejection"
    Assert (-not ($Script:Events -contains 'RegisterTask')) "$Name changed Task Scheduler before rejection"
    Assert (-not ($Script:Events -contains 'StartTask')) "$Name started a broker before rejection"
    return $result
}

function script:Test-Path {
    [CmdletBinding()]
    param([string]$LiteralPath, [ValidateSet('Any','Leaf','Container')][string]$PathType)
    $full = Assert-PrivatePath $LiteralPath
    $isFile = [IO.File]::Exists($full)
    $isDirectory = [IO.Directory]::Exists($full)
    if ($PathType -eq 'Leaf') { return $isFile }
    if ($PathType -eq 'Container') { return $isDirectory }
    return $isFile -or $isDirectory
}

function script:Get-Item {
    [CmdletBinding()]
    param([string]$LiteralPath, [switch]$Force)
    $full = Assert-PrivatePath $LiteralPath
    if (-not ([IO.File]::Exists($full) -or [IO.Directory]::Exists($full))) {
        if ($ErrorActionPreference -eq 'SilentlyContinue') { return }
        throw "Fixture item missing: $full"
    }
    $attributes = [IO.File]::GetAttributes($full)
    return [pscustomobject]@{
        FullName=$full
        Name=[IO.Path]::GetFileName($full)
        Attributes=$attributes
        PSIsContainer=(($attributes -band [IO.FileAttributes]::Directory) -ne 0)
        Length=if (($attributes -band [IO.FileAttributes]::Directory) -eq 0) { ([IO.FileInfo]::new($full)).Length } else { 0 }
    }
}

function script:Get-ChildItem {
    [CmdletBinding()]
    param([string]$LiteralPath, [switch]$Force)
    $directory = Assert-PrivatePath $LiteralPath
    if (-not [IO.Directory]::Exists($directory)) { throw "Fixture directory missing: $directory" }
    foreach ($full in [IO.Directory]::GetFileSystemEntries($directory)) {
        Assert-PrivatePath $full | Out-Null
        $attributes = [IO.File]::GetAttributes($full)
        if (($attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw "Fixture found reparse entry: $full" }
        [pscustomobject]@{
            FullName=$full
            Name=[IO.Path]::GetFileName($full)
            Attributes=$attributes
            PSIsContainer=(($attributes -band [IO.FileAttributes]::Directory) -ne 0)
        }
    }
}

function script:New-Item {
    [CmdletBinding()]
    param([string]$ItemType, [string]$Path)
    if ($ItemType -ne 'Directory') { throw "Fixture refuses New-Item type $ItemType" }
    $full = Assert-PrivatePath $Path
    [IO.Directory]::CreateDirectory($full) | Out-Null
    Add-FixtureEvent 'NewDirectory'
    Get-Item -LiteralPath $full
}

function script:Copy-Item {
    [CmdletBinding()]
    param([string]$LiteralPath, [string]$Destination, [switch]$Force)
    $source = Assert-PrivatePath $LiteralPath
    $destination = Assert-PrivatePath $Destination
    Add-FixtureEvent ("Copy:" + [IO.Path]::GetFileName($source) + '>' + [IO.Path]::GetFileName($destination))
    if ((Test-FixtureFault 'CandidateRead') -and $source -eq $Script:Source) { throw 'injected candidate source read failure' }
    if ((Test-FixtureFault 'CandidateCopy') -and [IO.Path]::GetFileName($destination) -eq 'candidate.exe') { throw 'injected candidate copy failure' }
    if ([IO.File]::Exists($destination) -and -not $Force) { throw "Fixture refused overwrite: $destination" }
    $bytes = [IO.File]::ReadAllBytes($source)
    if ((Test-FixtureFault 'CandidateCorrupt') -and [IO.Path]::GetFileName($destination) -eq 'candidate.exe') { $bytes = [Text.Encoding]::UTF8.GetBytes('corrupt-candidate') }
    if ((Test-FixtureFault 'BackupReceiptCorrupt') -and [IO.Path]::GetFileName($source) -eq 'broker-installed.sha256' -and [IO.Path]::GetFileName($destination) -eq 'previous.receipt') {
        $bytes = [Text.Encoding]::UTF8.GetBytes('corrupt-receipt-backup')
    }
    [IO.File]::WriteAllBytes($destination, $bytes)
    if ((Test-FixtureFault 'RestoreReceiptCorrupt') -and [IO.Path]::GetFileName($source) -eq 'previous.receipt' -and [IO.Path]::GetFileName($destination) -like '*.restore-*') {
        [IO.File]::WriteAllBytes($destination, [Text.Encoding]::UTF8.GetBytes('corrupt-restore-receipt'))
    }
}

function script:Get-FileHash {
    [CmdletBinding()]
    param([string]$LiteralPath, [string]$Algorithm)
    $full = Assert-PrivatePath $LiteralPath
    Add-FixtureEvent ("Hash:" + [IO.Path]::GetFileName($full))
    if (-not [IO.File]::Exists($full)) { throw "Fixture hash target missing: $full" }
    if ((Test-FixtureFault 'CandidateHashMismatch') -and [IO.Path]::GetFileName($full) -eq 'candidate.exe') {
        return [pscustomobject]@{ Algorithm='SHA256'; Hash=('0' * 64); Path=$full }
    }
    $hash = Get-TestHash $full
    return [pscustomobject]@{ Algorithm='SHA256'; Hash=$hash; Path=$full }
}

function script:Remove-Item {
    [CmdletBinding()]
    param([string]$LiteralPath, [switch]$Force)
    $full = Assert-PrivatePath $LiteralPath
    Add-FixtureEvent ("Remove:" + [IO.Path]::GetFileName($full))
    if ([IO.File]::Exists($full)) { [IO.File]::Delete($full) }
    elseif ([IO.Directory]::Exists($full)) { [IO.Directory]::Delete($full, $false) }
    $Script:AclState.Remove($full)
}

function script:Get-Content {
    [CmdletBinding()]
    param([string]$LiteralPath, [int]$TotalCount)
    $full = Assert-PrivatePath $LiteralPath
    if (-not [IO.File]::Exists($full)) { throw "Fixture content file missing: $full" }
    $lines = [IO.File]::ReadAllLines($full)
    if ($TotalCount -gt 0) { $lines = @($lines | Select-Object -First $TotalCount) }
    return $lines
}

function script:Set-ProtectedFileAcl($Path, [bool]$Directory) {
    $full = Assert-PrivatePath $Path
    $expected = if ($Directory) { 'directory-protected' } else { 'file-protected' }
    if ($Directory -and -not [IO.Directory]::Exists($full)) { throw "Fixture ACL directory missing: $full" }
    if (-not $Directory -and -not [IO.File]::Exists($full)) { throw "Fixture ACL file missing: $full" }
    $Script:AclState[$full] = $expected
    Add-FixtureEvent ("ACL:$expected")
}

function script:Assert-ProtectedFileAcl([string]$Path) {
    $full = Assert-PrivatePath $Path
    if (-not $Script:AclState.ContainsKey($full)) { throw "Fixture ACL marker missing: $full" }
    $attributes = [IO.File]::GetAttributes($full)
    $expected = if (($attributes -band [IO.FileAttributes]::Directory) -ne 0) { 'directory-protected' } else { 'file-protected' }
    if ($Script:AclState[$full] -ne $expected) { throw "Fixture ACL marker mismatch: $full" }
    Add-FixtureEvent 'AssertAcl'
}

function script:Get-ScheduledTask {
    [CmdletBinding()]
    param([string]$TaskName, [string]$TaskPath)
    if ((Test-FixtureFault 'TaskDisappearsAfterReceiptCommit') -and
        [IO.File]::Exists($Script:Receipt) -and -not [IO.File]::Exists($Script:Pending)) {
        $Script:BrokerTask.Present = $false
    }
    if ($TaskName -ne $Script:TaskName -or -not $Script:BrokerTask.Present) {
        Write-Error 'fixture task absent' -Category ObjectNotFound -ErrorAction Stop
    }
    Add-FixtureEvent 'GetTask'
    return Get-FakeTaskObject
}

function script:Export-ScheduledTask {
    [CmdletBinding()]
    param([string]$TaskName)
    if (-not $Script:BrokerTask.Present) { throw 'fixture task absent' }
    Add-FixtureEvent 'ExportTask'
    return [string]$Script:BrokerTask.Xml
}

function script:Disable-ScheduledTask {
    [CmdletBinding()]
    param([string]$TaskName)
    if ($TaskName -ne $Script:TaskName -or -not $Script:BrokerTask.Present) { throw 'fixture disable target mismatch' }
    $Script:BrokerTask.Enabled = $false
    Add-FixtureEvent 'DisableTask'
}

function script:Enable-ScheduledTask {
    [CmdletBinding()]
    param([string]$TaskName)
    if ($TaskName -ne $Script:TaskName -or -not $Script:BrokerTask.Present) { throw 'fixture enable target mismatch' }
    $Script:BrokerTask.Enabled = $true
    Add-FixtureEvent 'EnableTask'
}

function script:Stop-ScheduledTask {
    [CmdletBinding()]
    param([string]$TaskName)
    if ($TaskName -ne $Script:TaskName -or -not $Script:BrokerTask.Present) { throw 'fixture stop target mismatch' }
    Add-FixtureEvent 'StopTask'
    if ((Test-FixtureFault 'StopTimeout') -and -not $Script:FaultTriggered) { $Script:FaultTriggered = $true; return }
    $Script:BrokerTask.Running = $false
}

function script:Start-ScheduledTask {
    [CmdletBinding()]
    param([string]$TaskName)
    if ($TaskName -ne $Script:TaskName -or -not $Script:BrokerTask.Present) { throw 'fixture start target mismatch' }
    if (-not $Script:BrokerTask.Enabled) { throw 'fixture refuses to start a disabled task' }
    Add-FixtureEvent 'StartTask'
    if ((Test-FixtureFault 'TaskStart') -and -not $Script:FaultTriggered) { $Script:FaultTriggered = $true; throw 'injected task start failure' }
    $Script:BrokerTask.Running = $true
    if ((Test-FixtureFault 'ExeRemovedAfterStart') -and -not $Script:FaultTriggered) {
        [IO.File]::Delete($Script:Executable)
        $Script:AclState.Remove($Script:Executable)
        $Script:FaultTriggered = $true
    }
}

function script:Unregister-ScheduledTask {
    [CmdletBinding()]
    param([string]$TaskName, [switch]$Confirm)
    if ($TaskName -ne $Script:TaskName) { throw 'fixture unregister target mismatch' }
    Add-FixtureEvent 'UnregisterTask'
    $Script:BrokerTask.Present = $false
}

function script:Register-ScheduledTask {
    [CmdletBinding()]
    param([string]$TaskName, [string]$Xml, [object]$Action, [object]$Trigger,
        [object]$Principal, [object]$Settings, [switch]$Force)
    if ($TaskName -ne $Script:TaskName) { throw 'fixture register target mismatch' }
    Add-FixtureEvent 'RegisterTask'
    if ($PSBoundParameters.ContainsKey('Xml')) {
        $document = [xml]$Xml
        $enabledNode = $document.SelectSingleNode("//*[local-name()='Settings']/*[local-name()='Enabled']")
        $enabled = if ($enabledNode) { [bool]::Parse($enabledNode.InnerText) } else { $true }
        $Script:BrokerTask.Present = $true
        $Script:BrokerTask.Xml = $Xml
        $Script:BrokerTask.Enabled = $enabled
        $Script:BrokerTask.Running = $false
        $Script:BrokerTask.Principal = [pscustomobject]@{ UserId='SYSTEM'; LogonType='ServiceAccount'; RunLevel='Highest' }
        $Script:BrokerTask.Actions = @([pscustomobject]@{ Execute=$Script:Executable; WorkingDirectory=$Script:Directory; Arguments='' })
        $Script:BrokerTask.Triggers = @([pscustomobject]@{ CimClass=[pscustomobject]@{ CimClassName='MSFT_TaskBootTrigger' } })
        if (Test-FixtureFault 'RollbackTaskFailure') { throw 'injected task rollback registration failure' }
        return
    }
    $Script:BrokerTask.Present = $true
    $Script:BrokerTask.Xml = '<Task><Settings></Settings></Task>'
    $Script:BrokerTask.Enabled = [bool]$Settings.Enabled
    if ($Settings.Disabled) { $Script:BrokerTask.Enabled = $false }
    $Script:BrokerTask.Running = $false
    $Script:BrokerTask.Principal = $Principal
    $Script:BrokerTask.Actions = @($Action)
    $Script:BrokerTask.Triggers = @($Trigger)
    $Script:BrokerTask.Sddl = 'O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FR;;;BU)'
    if ((Test-FixtureFault 'TaskRegistration') -and -not $Script:FaultTriggered) {
        $Script:FaultTriggered = $true
        throw 'injected task registration failure after partial registration'
    }
}

function script:New-ScheduledTaskAction {
    [CmdletBinding()]
    param([string]$Execute, [string]$WorkingDirectory)
    return [pscustomobject]@{ Execute=$Execute; WorkingDirectory=$WorkingDirectory; Arguments='' }
}

function script:New-ScheduledTaskTrigger {
    [CmdletBinding()]
    param([switch]$AtStartup)
    return [pscustomobject]@{ CimClass=[pscustomobject]@{ CimClassName='MSFT_TaskBootTrigger' } }
}

function script:New-ScheduledTaskPrincipal {
    [CmdletBinding()]
    param([string]$UserId, [string]$LogonType, [string]$RunLevel)
    return [pscustomobject]@{ UserId=$UserId; LogonType=$LogonType; RunLevel=$RunLevel }
}

function script:New-ScheduledTaskSettingsSet {
    [CmdletBinding(PositionalBinding=$false)]
    param([switch]$Disable, [object]$ExecutionTimeLimit, [string]$MultipleInstances,
        [switch]$StartWhenAvailable, [switch]$AllowStartIfOnBatteries,
        [switch]$DontStopIfGoingOnBatteries, [int]$RestartCount, [object]$RestartInterval)
    return [pscustomobject]@{ Enabled=(-not $Disable); Disabled=[bool]$Disable }
}

function script:Start-Sleep {
    [CmdletBinding()]
    param([int]$Milliseconds)
    Add-FixtureEvent 'Sleep'
}

function script:New-Object {
    [CmdletBinding(DefaultParameterSetName='TypeName')]
    param([string]$TypeName, [string]$ComObject)
    if ($ComObject -eq 'Schedule.Service') { Add-FixtureEvent 'NewTaskService'; return New-FakeTaskService }
    throw "Fixture denied New-Object: $TypeName $ComObject"
}

function script:Replace-BrokerFile($SourcePath, $DestinationPath) {
    $source = Assert-PrivatePath $SourcePath
    $destination = Assert-PrivatePath $DestinationPath
    Add-FixtureEvent ("Replace:" + [IO.Path]::GetFileName($source) + '>' + [IO.Path]::GetFileName($destination))
    if ($destination -eq $Script:Pending) { $Script:PendingRefreshTempPath = $source }
    if ($Script:Fault -eq 'PublishFailure' -and -not $Script:FaultTriggered -and
        [IO.Path]::GetFileName($source) -eq 'candidate.exe' -and $destination -eq $Script:Executable) {
        $Script:FaultTriggered = $true
        throw 'injected broker publication failure'
    }
    if ((Test-FixtureFault 'PendingRefreshBeforeReplace') -and -not $Script:FaultTriggered -and $destination -eq $Script:Pending) {
        $Script:FaultTriggered = $true
        throw 'injected pending-record failure before atomic replacement'
    }
    Invoke-ProductionReplaceBrokerFile $source $destination
    if ($Script:AclState.ContainsKey($source)) { $Script:AclState[$destination] = $Script:AclState[$source]; $Script:AclState.Remove($source) }
    if ((Test-FixtureFault 'ReceiptCommit') -and -not $Script:FaultTriggered -and $destination -eq $Script:Receipt) {
        $Script:FaultTriggered = $true
        throw 'injected receipt commit failure after atomic replacement'
    }
    if ((Test-FixtureFault 'PendingRefreshAfterReplace') -and -not $Script:FaultTriggered -and $destination -eq $Script:Pending) {
        $Script:FaultTriggered = $true
        throw 'injected pending-record failure after atomic replacement'
    }
}

function Assert-ProductionCommandAllowlist([string]$Name, [string[]]$Allowed) {
    $definition = $Script:ProductionAst.Find({ param($node)
        $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $Name
    }, $false)
    if (-not $definition) { throw "Missing production AST for $Name" }
    foreach ($command in $definition.FindAll({ param($node) $node -is [Management.Automation.Language.CommandAst] }, $true)) {
        $commandName = $command.GetCommandName()
        if (-not $commandName) { throw "Dynamic command invocation in ${Name} is not allowed by the fixture: $($command.Extent.Text)" }
        if ($commandName -notin $Allowed) { throw "Unmocked command in ${Name}: $commandName" }
    }
}

$parseTokens = $null
$parseErrors = $null
$Script:ProductionAst = [Management.Automation.Language.Parser]::ParseFile($ProductionPath, [ref]$parseTokens, [ref]$parseErrors)
if ($parseErrors.Count -gt 0) { throw ($parseErrors | ForEach-Object { $_.Message } | Out-String) }

$productionFunctions = @(
    'Test-OwnershipRecord', 'Assert-RegularFileIfPresent', 'Get-OptionalScheduledTask',
    'Remove-RecognizedTransactionDirectory', 'New-StagedBrokerPayload', 'Get-BrokerProcessImagePaths',
    'Assert-NoBrokerProcessAtPath', 'Install-PendingBrokerRepair',
    'Get-BrokerTaskSnapshot', 'Get-NormalizedBrokerTaskXml', 'Restore-BrokerTaskSnapshot', 'Prepare-BrokerCandidate',
    'Copy-ProtectedBrokerFile', 'Capture-BrokerPriorState', 'Stop-BrokerTask',
    'Restore-BrokerTaskRuntimeState', 'Publish-BrokerCandidate', 'Install-BrokerTask',
    'Commit-BrokerReceipt', 'Verify-BrokerReplacement', 'Restore-BrokerFile',
    'Restore-BrokerState', 'Cleanup-BrokerTransaction', 'Install-ProtectedBrokerReplacement',
    'Write-ProtectedRecord'
)
foreach ($name in $productionFunctions) { Import-ProductionFunction $name $name }
Import-ProductionFunction 'Replace-BrokerFile' 'Invoke-ProductionReplaceBrokerFile'
Import-ProductionFunction 'Get-BrokerProcessImagePaths' 'Invoke-ProductionGetBrokerProcessImagePaths'

function script:Get-BrokerProcessImagePaths {
    [CmdletBinding()]
    param()
    Add-FixtureEvent 'ProcessQuery'
    if ($Script:ProcessQueryFailure) { throw 'injected process image query failure' }
    return @($Script:ProcessImagePaths)
}

$productionCommands = @(
    'Assert-ProtectedFileAcl', 'Assert-RegularFileIfPresent', 'Capture-BrokerPriorState',
    'Cleanup-BrokerTransaction', 'Commit-BrokerReceipt', 'Copy-Item', 'Copy-ProtectedBrokerFile',
    'ConvertTo-Json', 'Disable-ScheduledTask', 'Enable-ScheduledTask', 'Export-ScheduledTask',
    'ForEach-Object', 'Get-BrokerProcessImagePaths', 'Get-BrokerTaskSnapshot', 'Get-ChildItem',
    'Get-CimInstance', 'Get-Content', 'Get-FileHash', 'Get-Item', 'Get-Process',
    'Get-NormalizedBrokerTaskXml', 'Get-OptionalScheduledTask', 'Get-ScheduledTask',
    'Install-BrokerTask', 'Install-PendingBrokerRepair', 'Install-ProtectedBrokerReplacement',
    'New-StagedBrokerPayload', 'Assert-NoBrokerProcessAtPath',
    'Join-Path',
    'New-Item', 'New-Object', 'New-ScheduledTaskAction', 'New-ScheduledTaskPrincipal',
    'New-ScheduledTaskSettingsSet', 'New-ScheduledTaskTrigger', 'New-TimeSpan', 'Out-Null',
    'Out-String', 'Prepare-BrokerCandidate', 'Publish-BrokerCandidate', 'Register-ScheduledTask',
    'Remove-Item', 'Remove-RecognizedTransactionDirectory', 'Replace-BrokerFile', 'Restore-BrokerFile', 'Restore-BrokerState',
    'Restore-BrokerTaskRuntimeState', 'Restore-BrokerTaskSnapshot', 'Select-Object', 'Set-ProtectedFileAcl',
    'Start-ScheduledTask', 'Start-Sleep', 'Stop-BrokerTask', 'Stop-ScheduledTask', 'Test-Path',
    'Test-OwnershipRecord', 'Unregister-ScheduledTask', 'Verify-BrokerReplacement', 'Where-Object',
    'Write-ProtectedRecord', 'Write-Warning'
)
foreach ($name in $productionFunctions) { Assert-ProductionCommandAllowlist $name $productionCommands }
Assert-ProductionCommandAllowlist 'Replace-BrokerFile' $productionCommands

try {
    [IO.Directory]::CreateDirectory($Script:FixtureRoot) | Out-Null
    if ([IO.Path]::GetDirectoryName($Script:FixtureRoot).TrimEnd([char[]]@('\','/')) -ine $Script:FixtureParent) { throw 'Fixture root escaped the OS temp directory.' }

    Initialize-Fixture 'candidate-read-fails'
    $result = Invoke-Replacement 'CandidateRead'
    Assert $result.Failed 'candidate source read fault did not fail replacement'
    Assert (-not ($Script:Events -contains 'StopTask')) 'candidate source read failure stopped the broker task'
    Assert-OriginalReplacementState 'candidate source read failure'
    Assert ((Get-RecoveryDirectories).Count -eq 0) ("candidate read failure leaked a recovery directory: " + $result.Message + '; events=' + ($Script:Events -join ','))

    Initialize-Fixture 'candidate-copy-fails'
    $result = Invoke-Replacement 'CandidateCopy'
    Assert $result.Failed 'candidate copy fault did not fail replacement'
    Assert (-not ($Script:Events -contains 'StopTask')) 'candidate copy failure stopped the broker task'
    Assert-OriginalReplacementState 'candidate copy failure'
    Assert ((Get-RecoveryDirectories).Count -eq 0) 'candidate copy failure leaked a recovery directory'

    Initialize-Fixture 'candidate-copy-corrupts'
    $result = Invoke-Replacement 'CandidateCorrupt'
    Assert $result.Failed 'candidate hash mismatch did not fail replacement'
    Assert (-not ($Script:Events -contains 'StopTask')) 'candidate hash mismatch stopped the broker task'
    Assert-OriginalReplacementState 'candidate hash mismatch'
    Assert ((Get-RecoveryDirectories).Count -eq 0) 'candidate hash mismatch leaked a recovery directory'

    Initialize-Fixture 'candidate-hash-verification-fails'
    $result = Invoke-Replacement 'CandidateHashMismatch'
    Assert $result.Failed ("candidate hash verification fault did not fail replacement; events=" + ($Script:Events -join ','))
    Assert (-not ($Script:Events -contains 'StopTask')) 'candidate hash verification failure stopped the broker task'
    Assert-OriginalReplacementState 'candidate hash verification failure'
    Assert ((Get-RecoveryDirectories).Count -eq 0) 'candidate hash verification failure leaked a recovery directory'

    Initialize-Fixture 'backup-receipt-corrupts'
    $result = Invoke-Replacement 'BackupReceiptCorrupt'
    Assert $result.Failed 'corrupt old receipt backup was accepted'
    Assert (-not ($Script:Events -contains 'StopTask')) 'corrupt receipt backup stopped the broker task'
    Assert-OriginalReplacementState 'corrupt receipt backup'
    Assert ((Get-RecoveryDirectories).Count -eq 0) 'corrupt receipt backup leaked a recovery directory'

    foreach ($scenario in @(
        @{ Name='publish-fails'; Fault='PublishFailure' },
        @{ Name='task-register-fails'; Fault='TaskRegistration' },
        @{ Name='task-start-fails'; Fault='TaskStart' },
        @{ Name='receipt-commit-fails'; Fault='ReceiptCommit' },
        @{ Name='exe-disappears-after-start'; Fault='ExeRemovedAfterStart' }
    )) {
        Initialize-Fixture $scenario.Name
        $result = Invoke-Replacement $scenario.Fault
        Assert $result.Failed "$($scenario.Name) did not fail replacement"
        Assert-OriginalReplacementState $scenario.Name
        Assert ((Get-RecoveryDirectories).Count -eq 0) "$($scenario.Name) leaked recovery state after successful rollback"
    }

    Initialize-Fixture 'stop-times-out'
    $result = Invoke-Replacement 'StopTimeout'
    Assert $result.Failed 'stop timeout did not fail replacement'
    Assert (@($Script:Events | Where-Object { $_ -like 'Replace:*>neuron-chroma-broker.exe' }).Count -eq 0) 'stop timeout attempted executable publication'
    Assert (($Script:Events | Where-Object { $_ -eq 'StopTask' }).Count -eq 1) 'stop timeout retried stopping the task during rollback'
    Assert (-not ($Script:Events -contains 'UnregisterTask')) 'stop timeout replaced the task definition during runtime rollback'
    Assert (($Script:Events | Where-Object { $_ -eq 'EnableTask' }).Count -ge 1) 'stop timeout did not restore the task enabled state'
    Assert-OriginalReplacementState 'stop timeout'
    Assert-ByteFile $Script:Executable $Script:OldBinary 'stop timeout executable stayed unchanged'
    Assert-ByteFile $Script:Receipt $Script:OldReceipt 'stop timeout receipt stayed unchanged'
    Assert ((Get-RecoveryDirectories).Count -eq 0) 'stop timeout leaked a recovery directory'

    Initialize-Fixture 'rollback-keeps-trusted-backup'
    $Script:SecondaryFaults = @('RestoreReceiptCorrupt')
    $Script:Fault = 'ReceiptCommit'
    $result = Invoke-Replacement 'ReceiptCommit'
    Assert $result.Failed 'rollback copy corruption did not fail the transaction'
    Assert ($result.Message -match 'rollback failed') 'rollback failure was not reported'
    $recoveryDirs = @(Get-RecoveryDirectories)
    Assert ($recoveryDirs.Count -eq 1) 'rollback failure did not retain its recovery directory'
    $backupExe = Join-Path $recoveryDirs[0] 'previous.exe'
    $backupReceipt = Join-Path $recoveryDirs[0] 'previous.receipt'
    Assert-ByteFile $backupExe $Script:OldBinary 'retained trusted executable backup'
    Assert-ByteFile $backupReceipt $Script:OldReceipt 'retained trusted receipt backup'
    Assert ((ConvertTo-Json -InputObject $Script:ForeignTask -Compress -Depth 6) -ceq $Script:ForeignTaskJson) 'rollback failure touched the unrelated task'

    Initialize-Fixture 'pending-only-restored-after-commit-fail'
    Set-PriorPendingOnly
    $result = Invoke-Replacement 'ReceiptCommit'
    Assert $result.Failed 'pending-only receipt commit fault did not fail replacement'
    Assert-OriginalReplacementState 'pending-only receipt commit rollback'
    Assert ((Get-RecoveryDirectories).Count -eq 0) 'pending-only rollback leaked recovery state'

    Initialize-Fixture 'rollback-task-registration-fails'
    $Script:SecondaryFaults = @('RollbackTaskFailure')
    $result = Invoke-Replacement 'TaskRegistration'
    Assert $result.Failed 'task re-registration rollback fault did not fail the transaction'
    Assert ($result.Message -match 'rollback failed') 'task re-registration rollback failure was not reported'
    $recoveryDirs = @(Get-RecoveryDirectories)
    Assert ($recoveryDirs.Count -eq 1) 'task re-registration rollback failure did not retain recovery state'
    Assert-ByteFile (Join-Path $recoveryDirs[0] 'previous.exe') $Script:OldBinary 'task rollback retained trusted executable backup'
    Assert-ByteFile (Join-Path $recoveryDirs[0] 'previous.receipt') $Script:OldReceipt 'task rollback retained trusted receipt backup'
    $taskXmlBackup = Join-Path $recoveryDirs[0] 'previous-task.xml'
    $xmlDocument = [System.Xml.XmlDocument]::new()
    $xmlDocument.Load($taskXmlBackup)
    Assert ($xmlDocument.DocumentElement.LocalName -eq 'Task') 'retained exported task XML is not parseable using its declared encoding'

    Initialize-Fixture 'disabled-running-old-task' $true $false $true
    $prior = Get-BrokerTaskSnapshot $Script:TaskName
    Assert (-not $prior.Enabled -and $prior.Running) 'task snapshot did not preserve disabled-but-running state from COM'
    $snapshot = @{ Present=$true; Xml=$prior.Xml; Sddl=$prior.Sddl; Enabled=$false; Running=$true }
    $Script:BrokerTask.Enabled = $true
    $Script:BrokerTask.Running = $false
    Restore-BrokerTaskSnapshot $Script:TaskName $snapshot
    Assert (-not $Script:BrokerTask.Enabled -and $Script:BrokerTask.Running) 'disabled-but-running task state was not restored'
    Assert (($Script:Events | Where-Object { $_ -eq 'StartTask' }).Count -eq 1) 'disabled task was not temporarily enabled before restart'
    Assert (($Script:Events | Where-Object { $_ -eq 'DisableTask' }).Count -ge 2) 'disabled-but-running task was not disabled after restart'
    Assert (($Script:Events | Where-Object { $_ -like 'SetSD:*' } | Select-Object -Last 1) -eq 'SetSD:16') 'task restore used the wrong SET security descriptor flags'
    Assert (($Script:Events | Where-Object { $_ -like 'GetSD:*' } | Select-Object -Last 1) -eq 'GetSD:7') 'task restore used the wrong GET security descriptor flags'

    Initialize-Fixture 'task-snapshot-no-enabled-xml'
    $snapshot = Get-BrokerTaskSnapshot $Script:TaskName
    Assert ($snapshot.Enabled -and $snapshot.Running) 'task snapshot inferred disabled from XML that omits Enabled'
    Assert ($snapshot.Sddl -eq $Script:BrokerTask.Sddl) 'task snapshot did not capture the security descriptor'
    Assert (($Script:Events | Where-Object { $_ -like 'GetSD:*' } | Select-Object -Last 1) -eq 'GetSD:7') 'task snapshot used the wrong GET security descriptor flags'

    foreach ($scenario in @(
        @{ Name='pending-only-first-install-repair'; OtherProcess=$false; RecordKind='valid' },
        @{ Name='pending-repair-ignores-other-image'; OtherProcess=$true; RecordKind='valid' },
        @{ Name='pending-repair-refreshes-older-hash'; OtherProcess=$false; RecordKind='old-hash' }
    )) {
        Initialize-Fixture $scenario.Name $false
        Set-PendingRepairRecord $scenario.RecordKind
        if ($scenario.OtherProcess) {
            $Script:ProcessImagePaths = @((Join-Path $Script:Root 'other\neuron-chroma-broker.exe'))
        }
        $result = Invoke-PendingRepair
        Assert (-not $result.Failed) ("$($scenario.Name) failed: " + $result.Message)
        Assert-PendingRepairSucceeded $scenario.Name
    }

    Initialize-Fixture 'pending-refresh-atomic-replacement-before-fault' $false
    Set-PendingRepairRecord 'old-hash'
    $Script:Fault = 'PendingRefreshBeforeReplace'
    $result = Invoke-PendingRepair
    Assert $result.Failed 'pending refresh fault before atomic replacement did not fail repair'
    Assert-PendingRepairUnchanged 'pending refresh before replacement'
    Assert-PendingRefreshTempStayedInStage 'pending refresh before replacement'
    Assert (($Script:Events | Where-Object { $_ -eq 'ProcessQuery' }).Count -eq 2) 'pending refresh before replacement skipped a process-state recheck'
    Assert (-not ($Script:Events -contains 'RegisterTask')) 'pending refresh before replacement registered a task'

    Initialize-Fixture 'pending-refresh-atomic-replacement-after-fault' $false
    Set-PendingRepairRecord 'old-hash'
    $Script:Fault = 'PendingRefreshAfterReplace'
    $result = Invoke-PendingRepair
    Assert $result.Failed 'pending refresh fault after atomic replacement did not fail repair'
    $Script:PendingRepairBytes = [Text.Encoding]::UTF8.GetBytes("$Script:ExpectedHash`nSYSTEM startup`ntask-acl-v1`n$Script:OwnerSid`n")
    Assert-PendingRepairUnchanged 'pending refresh after replacement'
    Assert-PendingRefreshTempStayedInStage 'pending refresh after replacement'
    Assert (($Script:Events | Where-Object { $_ -eq 'ProcessQuery' }).Count -eq 2) 'pending refresh after replacement skipped a process-state recheck'
    Assert (-not ($Script:Events -contains 'RegisterTask')) 'pending refresh after replacement registered a task'

    Initialize-Fixture 'pending-repair-task-disappears-after-receipt' $false
    Set-PendingRepairRecord 'valid'
    Assert (-not [IO.File]::Exists($Script:Receipt) -and [IO.File]::Exists($Script:Pending)) 'post-receipt task disappearance pre-state is not pending-only'
    $Script:Fault = 'TaskDisappearsAfterReceiptCommit'
    $result = Invoke-PendingRepair
    Assert $result.Failed 'post-receipt task disappearance did not fail final verification'
    Assert ($result.Message -match 'for administrator inspection') ("post-receipt task disappearance did not request administrator inspection: $($result.Message)")
    Assert-ByteFile $Script:Executable $Script:NewBinary 'post-receipt task disappearance executable'
    Assert-ProtectedFileAcl $Script:Executable
    Assert-ByteFile $Script:Receipt ([Text.Encoding]::UTF8.GetBytes("$Script:ExpectedHash`nSYSTEM startup`ntask-acl-v1`n$Script:OwnerSid`n")) 'post-receipt task disappearance receipt'
    Assert (-not [IO.File]::Exists($Script:Pending)) 'post-receipt task disappearance left pending state'
    Assert (-not $Script:BrokerTask.Present) 'post-receipt task disappearance fixture did not remove its task'
    Assert ((Get-StagingDirectories).Count -eq 0) 'post-receipt task disappearance leaked its stage'
    Assert ((ConvertTo-Json -InputObject $Script:ForeignTask -Compress -Depth 6) -ceq $Script:ForeignTaskJson) 'post-receipt task disappearance touched the unrelated task'

    Initialize-Fixture 'pending-repair-image-disappears-after-task-start' $false
    Set-PendingRepairRecord 'valid'
    $Script:Fault = 'ExeRemovedAfterStart'
    $result = Invoke-PendingRepair
    Assert $result.Failed 'missing image after task start did not fail closed'
    Assert ($result.Message -match 'for administrator inspection') 'post-start image disappearance did not request administrator inspection'
    Assert (-not [IO.File]::Exists($Script:Executable)) 'post-start image-removal fixture did not remove the executable'
    Assert (-not [IO.File]::Exists($Script:Receipt)) 'post-start image-removal fixture committed a receipt'
    Assert ([IO.File]::Exists($Script:Pending)) 'post-start image-removal fixture discarded the authenticated pending record'
    $pendingLines = [IO.File]::ReadAllLines($Script:Pending)
    Assert ($pendingLines.Count -eq 4 -and $pendingLines[0] -ieq $Script:ExpectedHash -and
        $pendingLines[3] -eq $Script:OwnerSid) 'post-start image-removal fixture lost the refreshed owner record'
    Assert ($Script:BrokerTask.Present -and $Script:BrokerTask.Enabled -and $Script:BrokerTask.Running) 'post-start image-removal fixture removed or stopped the possibly active task'
    Assert ((Get-StagingDirectories).Count -eq 0) 'post-start image-removal fixture leaked its stage'
    Assert ((ConvertTo-Json -InputObject $Script:ForeignTask -Compress -Depth 6) -ceq $Script:ForeignTaskJson) 'post-start image-removal fixture touched the unrelated task'

    $null = Test-PendingRepairRejected 'pending-repair-malformed-record' 'malformed'
    $null = Test-PendingRepairRejected 'pending-repair-foreign-owner' 'foreign'
    $null = Test-PendingRepairRejected 'pending-repair-receipt-backed-missing-exe' 'valid' 'none' $true
    $null = Test-PendingRepairRejected 'pending-repair-task-present' 'valid' 'task'
    $null = Test-PendingRepairRejected 'pending-repair-process-present' 'valid' 'process'
    $null = Test-PendingRepairRejected 'pending-repair-process-query-unknown' 'valid' 'query-fails'

    foreach ($scenario in @(
        @{ Name='successful-replacement-receipt-only'; HasPending=$false },
        @{ Name='successful-replacement-with-pending'; HasPending=$true }
    )) {
        Initialize-Fixture $scenario.Name
        if ($scenario.HasPending) {
            $successPending = [Text.Encoding]::UTF8.GetBytes("$Script:ExpectedHash`nSYSTEM startup`ntask-acl-v1`n$Script:OwnerSid`n")
            [IO.File]::WriteAllBytes($Script:Pending, $successPending)
            $Script:AclState[$Script:Pending] = 'file-protected'
        }
        $result = Invoke-Replacement ''
        Assert (-not $result.Failed) ("healthy replacement $($scenario.Name) failed: " + $result.Message)
        Assert-ByteFile $Script:Executable $Script:NewBinary "$($scenario.Name) executable"
        Assert (-not [IO.File]::Exists($Script:Pending)) "$($scenario.Name) left a pending record"
        $newReceipt = [IO.File]::ReadAllText($Script:Receipt)
        Assert ($newReceipt.StartsWith($Script:ExpectedHash, [StringComparison]::OrdinalIgnoreCase)) "$($scenario.Name) receipt hash is wrong"
        Assert ($Script:BrokerTask.Enabled -and $Script:BrokerTask.Running) "$($scenario.Name) task is not enabled and running"
        Assert ((Get-RecoveryDirectories).Count -eq 0) "$($scenario.Name) retained a recovery directory"
        Assert ((ConvertTo-Json -InputObject $Script:ForeignTask -Compress -Depth 6) -ceq $Script:ForeignTaskJson) "$($scenario.Name) touched the unrelated task"
    }

    Write-Output "broker transaction fixtures: $Script:Assertions assertions passed"
} finally {
    if ([IO.Directory]::Exists($Script:FixtureRoot)) {
        $fullRoot = [IO.Path]::GetFullPath($Script:FixtureRoot).TrimEnd([char[]]@('\','/'))
        if ([IO.Path]::GetDirectoryName($fullRoot).TrimEnd([char[]]@('\','/')) -ine $Script:FixtureParent -or
            [IO.Path]::GetFileName($fullRoot) -notlike 'neuron-broker-transaction-*') {
            throw "Refusing to remove fixture root outside temp: $fullRoot"
        }
        Assert-NoFixtureReparseRecursively $fullRoot
        [IO.Directory]::Delete($fullRoot, $true)
    }
}
