# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
# Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

# Run once from an administrator PowerShell after building neuron-chroma-broker.exe. The tray
# remains Limited; only this fixed-purpose executable is installed in a protected directory.

[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$BinaryPath,
    [Parameter(Mandatory)][ValidatePattern('^[0-9a-fA-F]{64}$')][string]$ExpectedSha256,
    [Parameter(Mandatory)][string]$UserSid
)

$ErrorActionPreference = 'Stop'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'install-chroma-broker.ps1 requires an administrator PowerShell'
}
if ($identity.User.Value -ne $UserSid) {
    throw 'the administrator token must belong to the same user as the Neuron tray'
}

$source = [IO.Path]::GetFullPath($BinaryPath)
if (-not (Test-Path -LiteralPath $source -PathType Leaf)) { throw "broker binary not found: $source" }
if ((Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash -ine $ExpectedSha256) {
    throw 'broker binary hash changed before installation'
}

$taskName = 'Neuron Chroma broker'
$dir = Join-Path $env:ProgramFiles 'NeuronChromaBroker'
$exe = Join-Path $dir 'neuron-chroma-broker.exe'
$admins = New-Object Security.Principal.SecurityIdentifier('S-1-5-32-544')
$system = New-Object Security.Principal.SecurityIdentifier('S-1-5-18')
$users = New-Object Security.Principal.SecurityIdentifier('S-1-5-32-545')

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

if (Test-Path -LiteralPath $dir) {
    $item = Get-Item -LiteralPath $dir -Force
    if (-not $item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw "broker destination is not a normal directory: $dir"
    }
} else {
    New-Item -ItemType Directory -Path $dir -ErrorAction Stop | Out-Null
}
Set-ProtectedFileAcl $dir $true

$oldTask = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
if ($oldTask) {
    Disable-ScheduledTask -TaskName $taskName -ErrorAction Stop | Out-Null
    Stop-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
    for ($attempt = 0; $attempt -lt 30; $attempt++) {
        $oldState = (Get-ScheduledTask -TaskName $taskName).State
        if ($oldState -ne 'Running') { break }
        Start-Sleep -Milliseconds 200
    }
    if ((Get-ScheduledTask -TaskName $taskName).State -eq 'Running') {
        throw 'previous broker did not stop; task remains disabled'
    }
}
Copy-Item -LiteralPath $source -Destination $exe -Force -ErrorAction Stop
Set-ProtectedFileAcl $exe $false
if ((Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash -ine $ExpectedSha256) {
    throw 'broker binary hash changed during installation; task remains disabled'
}

$user = (New-Object Security.Principal.SecurityIdentifier($UserSid)).Translate(
    [Security.Principal.NTAccount]
).Value
$action = New-ScheduledTaskAction -Execute $exe -WorkingDirectory $dir
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $user
$brokerPrincipal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Highest
$settings = New-ScheduledTaskSettingsSet -Disable -ExecutionTimeLimit (New-TimeSpan -Seconds 0) -MultipleInstances IgnoreNew -StartWhenAvailable -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)
Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger -Principal $brokerPrincipal -Settings $settings -Force -ErrorAction Stop | Out-Null

# Task Scheduler otherwise adds a user ACE that could let the limited user edit the elevated
# action. SYSTEM and Administrators alone may change or run it; the logon trigger still fires.
$service = New-Object -ComObject 'Schedule.Service'
$service.Connect()
$registered = $service.GetFolder('\').GetTask($taskName)
$registered.SetSecurityDescriptor('O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)', 0x10)
$sddl = $registered.GetSecurityDescriptor(0xF)
$taskDescriptor = [Security.AccessControl.CommonSecurityDescriptor]::new($false, $false, $sddl)
if (-not $taskDescriptor.DiscretionaryAcl.IsCanonical) {
    throw 'broker task ACL is not canonical'
}
foreach ($ace in $taskDescriptor.DiscretionaryAcl) {
    if ($ace.SecurityIdentifier.Value -notin @($system.Value, $admins.Value)) {
        throw 'broker task ACL grants an unexpected identity access'
    }
}
$task = Get-ScheduledTask -TaskName $taskName -ErrorAction Stop
$principalId = [string]$task.Principal.UserId
$principalSid = if ($principalId -match '^S-1-') { $principalId } else {
    ([Security.Principal.NTAccount]$principalId).Translate([Security.Principal.SecurityIdentifier]).Value
}
if ($task.Principal.RunLevel -ne 'Highest' -or $task.Actions.Count -ne 1 -or
    $task.Principal.LogonType -ne 'Interactive' -or
    $principalSid -ne $UserSid -or
    $task.Actions[0].Execute -ine $exe -or
    $task.Actions[0].Arguments -or
    $task.Actions[0].WorkingDirectory -ine $dir) {
    throw 'broker task verification failed'
}
Enable-ScheduledTask -TaskName $taskName -ErrorAction Stop | Out-Null
Start-ScheduledTask -TaskName $taskName -ErrorAction Stop
Write-Output "installed: $exe"
Write-Output "sha256: $ExpectedSha256"
Write-Output "task: $taskName"
