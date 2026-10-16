# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
# Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

# Windows PowerShell 5.1 fixtures. Never run an installer or touch live tasks, processes or HID.
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
$updater = Join-Path $repoRoot 'skills/neuron-lazy-update/scripts/neuron-update.ps1'
$release = Join-Path $repoRoot 'release.ps1'
$setup = [IO.File]::ReadAllText((Join-Path $PSScriptRoot 'neuron.iss'))
$fixtureRoot = Join-Path ([IO.Path]::GetTempPath()) ('neuron-lifecycle-test-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $fixtureRoot | Out-Null
$utf8 = New-Object Text.UTF8Encoding($false)
$script:Assertions = 0

function Assert($condition, $message) {
    if (-not $condition) { throw $message }
    $script:Assertions++
}

function Import-Functions($path, $names) {
    $errors = $null
    $ast = [Management.Automation.Language.Parser]::ParseFile($path, [ref]$null, [ref]$errors)
    if ($errors) { throw ($errors.Message -join '; ') }
    foreach ($name in $names) {
        $definition = $ast.Find({ param($node)
            $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name
        }, $false)
        if (-not $definition) { throw "missing function $name in $path" }
        $definitionText = $definition.Extent.Text -replace '^function\s+', 'function script:'
        . ([scriptblock]::Create($definitionText))
    }
}

function New-Copy($relative, $version, $kind) {
    $dir = [IO.Path]::GetFullPath((Join-Path $fixtureRoot $relative)).Replace('/', '\')
    New-Item -ItemType Directory -Path $dir -Force | Out-Null
    [IO.File]::WriteAllText((Join-Path $dir 'neuron-app.exe'), '', $utf8)
    [IO.File]::WriteAllText((Join-Path $dir 'SOURCE.txt'), "Neuron $version - fixture", $utf8)
    if ($kind -eq 'installer') { [IO.File]::WriteAllText((Join-Path $dir 'unins000.exe'), '', $utf8) }
    if ($kind -eq 'portable') { [IO.File]::WriteAllText((Join-Path $dir 'portable.flag'), '', $utf8) }
    return $dir
}

function Get-InnoCommand($name) {
    $start = $setup.IndexOf("function $name(")
    if ($start -lt 0) { throw "missing Inno function $name" }
    $body = $setup.Substring($start)
    $body = $body.Substring(0, $body.IndexOf("`nend;") + 5)
    $paramsStart = $body.IndexOf('Params := ') + 'Params := '.Length
    $paramsEnd = $body.IndexOf(";`n", $paramsStart)
    if ($paramsEnd -lt 0) { $paramsEnd = $body.IndexOf(";`r`n", $paramsStart) }
    $expression = $body.Substring($paramsStart, $paramsEnd - $paramsStart)
    $targetLiteral = "'" + (Join-Path $fixtureRoot 'installed/neuron-app.exe').Replace("'", "''") + "'"
    $expression = $expression.Replace("PowerShellLiteral(ExpandConstant('{app}\neuron-app.exe'))", "'" + $targetLiteral.Replace("'", "''") + "'")
    $transactionLiteral = "'" + (Join-Path $fixtureRoot 'programfiles').Replace("'", "''") + "'"
    $expression = $expression.Replace("PowerShellLiteral(ExpandConstant('{commonpf64}'))", "'" + $transactionLiteral.Replace("'", "''") + "'")
    $parts = [regex]::Matches($expression, "'(?:[^']|'')*'")
    $command = ($parts | ForEach-Object { $_.Value.Substring(1, $_.Value.Length - 2).Replace("''", "'") }) -join ''
    $prefix = '-NoProfile -NonInteractive -Command "'
    if (-not $command.StartsWith($prefix) -or -not $command.EndsWith('"')) { throw "could not decode command $name" }
    $command = $command.Substring($prefix.Length, $command.Length - $prefix.Length - 1)
    $errors = $null
    $ast = [Management.Automation.Language.Parser]::ParseInput($command, [ref]$null, [ref]$errors)
    if ($errors) { throw ($errors.Message -join '; ') }
    # Only these commands have fixture implementations; reject new OS entry points before execution.
    foreach ($node in $ast.FindAll({ param($n)
        $n -is [Management.Automation.Language.CommandAst] -or
        $n -is [Management.Automation.Language.TypeExpressionAst] -or
        $n -is [Management.Automation.Language.InvokeMemberExpressionAst]
    }, $true)) {
        if ($node -isnot [Management.Automation.Language.CommandAst] -or
            $node.GetCommandName() -notin @('Get-Process', 'Get-ScheduledTask', 'Unregister-ScheduledTask', 'Where-Object', 'Out-Null', 'Get-ChildItem', 'Select-Object')) {
            throw 'fixture runner refuses unmocked commands, dynamic calls and .NET invocation'
        }
    }
    return $command
}

function Invoke-InnoFixture($command, $processes, $tasks, $failQuery = $false, $failProcess = $false, $transactions = @(), $failTransactions = $false) {
    $fixture = @{ Command=$command; Processes=@($processes); Tasks=@($tasks); FailQuery=$failQuery; FailProcess=$failProcess; Transactions=@($transactions); FailTransactions=$failTransactions; TransactionRoot=(Join-Path $fixtureRoot 'programfiles') }
    $inputPath = Join-Path $fixtureRoot 'case.json'
    $outputPath = Join-Path $fixtureRoot 'case-result.json'
    [IO.File]::WriteAllText($inputPath, (ConvertTo-Json -InputObject $fixture -Depth 8), $utf8)
    $runnerPath = Join-Path $fixtureRoot 'case.ps1'
    $runner = @'
param($InputPath, $OutputPath)
$ErrorActionPreference = 'Stop'
$fixture = Get-Content -LiteralPath $InputPath -Raw | ConvertFrom-Json
$script:tasks = @($fixture.Tasks)
$script:removed = @()
function Get-Process { param($Name, $ErrorAction) if ($fixture.FailProcess) { throw 'fixture process query failure' }; return $fixture.Processes }
function Get-ChildItem {
    [CmdletBinding()]param($LiteralPath, [switch]$Force)
    if ($LiteralPath -ne $fixture.TransactionRoot) { throw 'fixture refused an unexpected transaction root' }
    if ($fixture.FailTransactions) { throw 'fixture transaction query failure' }
    return $fixture.Transactions
}
function Get-ScheduledTask {
    [CmdletBinding()]param($TaskName)
    if ($fixture.FailQuery) { throw 'fixture query failure' }
    $task = $script:tasks | Where-Object { $_.TaskName -eq $TaskName }
    if (-not $task) { Write-Error 'fixture task absent' -Category ObjectNotFound -ErrorAction Stop }
    return $task
}


function Unregister-ScheduledTask {
    param($TaskName, $Confirm, $ErrorAction)
    $script:removed += $TaskName
    $script:tasks = @($script:tasks | Where-Object { $_.TaskName -ne $TaskName })
}
try { & ([scriptblock]::Create($fixture.Command)) } finally {
    $result = @{Tasks=@($script:tasks); Removed=@($script:removed)}
    [IO.File]::WriteAllText($OutputPath, (ConvertTo-Json -InputObject $result -Depth 8))
}
'@
    [IO.File]::WriteAllText($runnerPath, $runner, $utf8)
    & powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File $runnerPath $inputPath $outputPath
    $code = $LASTEXITCODE
    $result = Get-Content -LiteralPath $outputPath -Raw | ConvertFrom-Json
    return [pscustomobject]@{Code=$code; Tasks=@($result.Tasks); Removed=@($result.Removed)}
}

function Invoke-UpdaterFixture($target, $action, $extraArgs = @(), $startupTaskDirs = @()) {
    $runnerPath = Join-Path $fixtureRoot 'updater-case.ps1'
    $scratch = Join-Path $fixtureRoot 'updater-scratch'
    $taskDirsPath = Join-Path $fixtureRoot 'updater-task-dirs.json'
    $targetArgument = if ($target) { $target } else { '__NO_INSTALL_DIR__' }
    New-Item -ItemType Directory -Path $scratch -Force | Out-Null
    [IO.File]::WriteAllText($taskDirsPath, (ConvertTo-Json -InputObject @{Dirs=@($startupTaskDirs)}), $utf8)
    $runner = @'
param($Updater, $FixtureRoot, $Target, $Action, $TaskDirsPath)
$ErrorActionPreference = 'Stop'
$env:LOCALAPPDATA = $FixtureRoot
$env:ProgramW6432 = Join-Path $FixtureRoot 'machine'
$env:TEMP = Join-Path $FixtureRoot 'updater-scratch'
$taskData = Get-Content -LiteralPath $TaskDirsPath -Raw | ConvertFrom-Json
$global:NEURON_LIFECYCLE_TASK_DIRS = @($taskData.Dirs)
function Get-Process { param($Name, $ErrorAction) }
function schtasks {
    $taskName = [string]$args[2]
    $index = if ($taskName -eq 'Neuron') { 0 } else { 1 }
    $dir = if ($index -lt $global:NEURON_LIFECYCLE_TASK_DIRS.Count) { $global:NEURON_LIFECYCLE_TASK_DIRS[$index] } else { $null }
    if (-not $dir) { $global:LASTEXITCODE = 1; return }
    $global:LASTEXITCODE = 0
    '<Task xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task"><Triggers><LogonTrigger><Enabled>true</Enabled></LogonTrigger></Triggers><Settings><Enabled>true</Enabled></Settings><Actions><Exec><Command>' + $dir + '\neuron-app.exe</Command></Exec></Actions></Task>'
}
function Get-ScheduledTask {
    [CmdletBinding()]param($TaskName)
    Write-Error 'fixture task absent' -Category ObjectNotFound -ErrorAction Stop
}
function Start-Process { throw 'fixture forbids process launches' }
function Stop-Process { throw 'fixture forbids process termination' }
function gh {
    if ($args[0] -eq 'api') {
        $global:LASTEXITCODE = 0
        '{"tag_name":"v0.1.5","assets":[{"name":"neuron-v0.1.5-windows-x86_64.zip"},{"name":"SHA256SUMS.txt"}]}'
    } else {
        $global:LASTEXITCODE = 1
        'fixture download unavailable'
    }
}
$text = [IO.File]::ReadAllText($Updater)
$ast = [Management.Automation.Language.Parser]::ParseInput($text, [ref]$null, [ref]$null)
foreach ($definition in $ast.FindAll({param($node)
    $node -is [Management.Automation.Language.FunctionDefinitionAst] -and
    $node.Name -in @('Stop-Neuron', 'Start-Neuron', 'Ensure-ChromaBroker')
}, $false)) {
    $replacement = if ($definition.Name -eq 'Stop-Neuron') {
        'function Stop-Neuron { return ''not-running'' }'
    } else { 'function ' + $definition.Name + ' { throw ''fixture forbids live lifecycle actions'' }' }
    $text = $text.Replace($definition.Extent.Text, $replacement)
}
$testScript = Join-Path $FixtureRoot 'updater-under-test.ps1'
[IO.File]::WriteAllText($testScript, $text)
$invokeArgs = @{ Action=$Action }
if ($Target -ne '__NO_INSTALL_DIR__') { $invokeArgs.InstallDir = $Target }
for ($i = 0; $i -lt $args.Count; $i++) {
    if ([string]$args[$i] -notmatch '^-([A-Za-z]+)$') { throw "invalid updater fixture argument $($args[$i])" }
    $name = $Matches[1]
    if ($name -in @('Json', 'NoRelaunch')) {
        $invokeArgs[$name] = $true
        continue
    }
    if ($name -eq 'ZipPath' -and ($i + 1) -lt $args.Count) {
        $i++
        $invokeArgs[$name] = $args[$i]
        continue
    }
    throw "unsupported updater fixture argument $name"
}
& $testScript @invokeArgs
exit $LASTEXITCODE
'@
    [IO.File]::WriteAllText($runnerPath, $runner, $utf8)
    $output = & powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File $runnerPath $updater $fixtureRoot $targetArgument $action $taskDirsPath @extraArgs
    $code = $LASTEXITCODE
    return [pscustomobject]@{Code=$code; Output=($output -join "`n"); Scratch=@(Get-ChildItem -LiteralPath $scratch -Force)}
}

try {
    Import-Functions $updater @('Parse-Version', 'Same-Dir', 'Test-SourceBuild', 'Get-InstalledVersion', 'Get-InstallKind', 'Get-InstallInventory', 'Compare-ReleaseVersion', 'Get-TaskExeDirs', 'Get-NeuronProcesses', 'Get-TargetNeuronAppProcesses', 'Test-TargetNeuronProcessPath', 'Request-NeuronShutdown', 'Request-TargetNeuronShutdown', 'Get-OptionalScheduledTask', 'Get-TargetNeuronTaskNames')
    Import-Functions $release @('Test-EnabledLogonTask', 'Test-LimitedStartupTask', 'Get-ReleaseLaunchMode', 'Get-ReleaseProcesses', 'Get-ReleaseTrayProcesses', 'Signal-ReleaseTrayProcesses')
    $DefaultDir = New-Copy 'installed' 'v0.1.4' 'installer'
    $sourceDir = New-Copy 'checkout/target/release' 'v0.1.5' 'source-build'
    [IO.File]::WriteAllText((Join-Path $fixtureRoot 'checkout/Cargo.toml'), '', $utf8)
    $script:processFixtures = @([pscustomobject]@{Name='neuron-app'; Path=(Join-Path $sourceDir 'neuron-app.exe'); Id=123})
    function Get-NeuronProcesses { param($dir) $script:processFixtures }
    $copies = @(Get-InstallInventory $sourceDir @($sourceDir))
    Assert ($copies.Count -eq 2) 'inventory must distinguish the installed copy and active source build'
    $active = $copies | Where-Object { $_.Running }
    Assert ($active.Kind -eq 'source-build' -and $active.Version -eq 'v0.1.5' -and $active.Startup) ("source build must report its version and startup role: " + ($copies | ConvertTo-Json -Compress))
    Assert (($copies | Where-Object { $_.Kind -eq 'installer' }).Version -eq 'v0.1.4') 'installed version must remain separate'
    Assert (($copies | Where-Object { $_.Kind -eq 'installer' }).RelativeToRunning -eq 'older-version') 'an older installed copy must be visibly older than the running build'
    Assert ($active.DefaultConfigDirectory -eq (Join-Path $env:LOCALAPPDATA 'neuron')) 'source builds must report the shared user config home'

    $passive = New-Copy 'passive-version' 'v0.1.5' 'portable'
    [IO.File]::WriteAllText((Join-Path $passive 'neuron.exe'), 'sentinel: do not execute', $utf8)
    $versionAst = [Management.Automation.Language.Parser]::ParseFile($updater, [ref]$null, [ref]$null)
    $versionFunction = $versionAst.Find({ param($node)
        $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Get-InstalledVersion'
    }, $false)
    Assert (-not ($versionFunction.Extent.Text -match '&\s*\$')) 'version inventory must not execute a discovered CLI binary'
    Assert ((Get-InstalledVersion $passive) -eq 'v0.1.5') 'version inventory must read SOURCE.txt without invoking neuron.exe'
    $unknownVersion = Join-Path $fixtureRoot 'missing-version-metadata'
    New-Item -ItemType Directory -Path $unknownVersion | Out-Null
    [IO.File]::WriteAllText((Join-Path $unknownVersion 'neuron-app.exe'), '', $utf8)
    [IO.File]::WriteAllText((Join-Path $unknownVersion 'SOURCE.txt'), 'Neuron source fixture without version metadata', $utf8)
    Assert ($null -eq (Get-InstalledVersion $unknownVersion)) 'missing passive version metadata must stay unknown'

    $shutdownForeign = New-Copy 'foreign-running' 'v0.1.4' 'portable'
    $script:processFixtures = @(
        [pscustomobject]@{Name='neuron-app'; Path=(Join-Path $sourceDir 'neuron-app.exe'); Id=123; InDir=$true},
        [pscustomobject]@{Name='neuron-app'; Path=(Join-Path $shutdownForeign 'neuron-app.exe'); Id=456; InDir=$false},
        [pscustomobject]@{Name='neuron-app'; Path=(Join-Path $sourceDir 'neuron-app.exe'); CurrentPath=(Join-Path $shutdownForeign 'neuron-app.exe'); Id=321; InDir=$true},
        [pscustomobject]@{Name='neuron'; Path=(Join-Path $sourceDir 'neuron.exe'); Id=789; InDir=$true},
        [pscustomobject]@{Name='neuron-app'; Path=$null; Id=987; InDir=$false}
    )
    function Get-Process {
        param($Id, $Name, $ErrorAction)
        if ($PSBoundParameters.ContainsKey('Id')) {
            $fixtureProcess = $script:processFixtures | Where-Object { $_.Id -eq $Id } | Select-Object -First 1
            if ($fixtureProcess) {
                $path = if ($fixtureProcess.CurrentPath) { $fixtureProcess.CurrentPath } else { $fixtureProcess.Path }
                return [pscustomobject]@{Id=$Id; Path=$path}
            }
        }
        return @()
    }
    $script:signaledShutdowns = @()
    function Request-NeuronShutdown($processId) { $script:signaledShutdowns += [int]$processId; return $true }
    Assert ((Request-TargetNeuronShutdown $sourceDir) -eq 1) 'only path-owned tray processes receive a shutdown request'
    Assert ($script:signaledShutdowns.Count -eq 1 -and $script:signaledShutdowns[0] -eq 123) 'targeted shutdown must exclude foreign and unknown PIDs'
    $stopAst = $versionAst.Find({ param($node)
        $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Stop-Neuron'
    }, $false)
    Assert ($stopAst.Extent.Text -notmatch [regex]::Escape("Local\WofloLabs.Neuron.Shutdown'")) 'selected-copy stop must not signal the legacy session-wide event'
    Assert ($stopAst.Extent.Text -match 'Get-TargetNeuronTaskNames\s+\$dir') 'legacy stop fallback must use path-owned tasks'
    $requestAst = $versionAst.Find({ param($node)
        $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Request-NeuronShutdown'
    }, $false)
    Assert ($requestAst.Extent.Text -match 'Shutdown\.\$processId') 'graceful shutdown must address a specific process id'

    $Tasks = @('Neuron', 'Neuron (elevated tray)')
    $script:taskFixtures = @{
        'Neuron' = [pscustomobject]@{Actions=@([pscustomobject]@{Execute=((Join-Path $sourceDir 'NEURON-APP.EXE').Replace('\', '/'))})}
        'Neuron (elevated tray)' = [pscustomobject]@{Actions=@([pscustomobject]@{Execute=(Join-Path $shutdownForeign 'neuron-app.exe')})}
    }
    function Get-OptionalScheduledTask($name) { return $script:taskFixtures[$name] }
    $ownedTaskNames = @(Get-TargetNeuronTaskNames $sourceDir)
    Assert ($ownedTaskNames.Count -eq 1 -and $ownedTaskNames[0] -eq 'Neuron') 'legacy task fallback must select only task actions owned by the target directory'
    $script:taskFixtures['Neuron'] = [pscustomobject]@{Actions=@(
        [pscustomobject]@{Execute=(Join-Path $sourceDir 'neuron-app.exe')},
        [pscustomobject]@{Execute=(Join-Path $shutdownForeign 'neuron-app.exe')}
    )}
    Assert (@(Get-TargetNeuronTaskNames $sourceDir).Count -eq 0) 'a mixed-path task must not be ended on behalf of either copy'
    $script:taskFixtures['Neuron'] = [pscustomobject]@{Actions=@(
        [pscustomobject]@{Execute=(Join-Path $sourceDir 'neuron-app.exe')},
        [pscustomobject]@{Execute=(Join-Path $sourceDir 'neuron-helper.exe')}
    )}
    Assert (@(Get-TargetNeuronTaskNames $sourceDir).Count -eq 0) 'same-directory helper actions must not be ended as an app task'
    $script:taskFixtures['Neuron'] = [pscustomobject]@{Actions=@([pscustomobject]@{Execute=(Join-Path $sourceDir 'neuron.exe')})}
    Assert (@(Get-TargetNeuronTaskNames $sourceDir).Count -eq 0) 'a task for the CLI must not be ended as an app task'

    $script:processFixtures = @([pscustomobject]@{Name='neuron-app'; Path=(Join-Path $sourceDir 'neuron-app.exe'); Id=123})
    $portable = New-Copy 'portable' 'v0.1.5' 'portable'
    $portableCopy = Get-InstallInventory $portable @() | Where-Object { $_.Selected }
    Assert ($portableCopy.DefaultConfigDirectory -eq $portable) 'portable config must stay with the portable copy'
    $custom = New-Copy 'custom-lane/release' 'v0.1.5' 'source-build'
    New-Item -ItemType Directory -Path (Join-Path $custom '.fingerprint') | Out-Null
    Assert (Test-SourceBuild $custom) 'a custom Cargo target lane must never be overwritten as a release install'
    Assert ((Compare-ReleaseVersion 'v0.1.5' 'v0.1.5') -eq 'same-version') 'equal source versions must be described as same-version'
    Assert ((Compare-ReleaseVersion 'v0.1.4' 'v0.1.5') -eq 'older-version') 'older versions must request an update'
    Assert ((Compare-ReleaseVersion $null 'v0.1.5') -eq 'unknown') 'unknown versions must stay unknown'

    $current = New-Copy 'current' 'v0.1.5' 'portable'
    $result = Invoke-UpdaterFixture $current 'apply'
    Assert ($result.Code -eq 0 -and $result.Output -match 'RESULT: up-to-date') ('same-version apply must avoid reinstall: ' + $result.Output)
    Assert ($result.Scratch.Count -eq 0 -and -not (Test-Path -LiteralPath (Join-Path $current '.neuron-update-backup'))) 'no-op updates must create neither scratch directories nor backups'
    $result = Invoke-UpdaterFixture $DefaultDir 'apply'
    Assert ($result.Code -ne 0 -and $result.Output -match 'DOWNLOAD_FAILED' -and $result.Output -match 'RESULT: error') 'failed downloads must fail before installation'
    Assert ($result.Scratch.Count -eq 0) 'failed downloads must clean their updater scratch directory'
    $result = Invoke-UpdaterFixture $sourceDir 'apply'
    Assert ($result.Code -eq 0 -and $result.Output -match 'RESULT: source-build' -and $result.Scratch.Count -eq 0) 'release apply must never overwrite source-build output'
    $badZip = Join-Path $fixtureRoot 'invalid.zip'
    [IO.File]::WriteAllText($badZip, 'invalid fixture archive', $utf8)
    [IO.File]::WriteAllText([IO.Path]::ChangeExtension($badZip, 'SHA256SUMS.txt'), (('0' * 64) + '  invalid.zip'), $utf8)
    $result = Invoke-UpdaterFixture $current 'apply' @('-ZipPath', $badZip)
    Assert ($result.Code -ne 0 -and $result.Output -match 'HASH_MISMATCH' -and $result.Output -match 'RESULT: blocked') 'invalid local checksums must block before extraction or mutation'
    Assert ($result.Scratch.Count -eq 0) 'blocked verification must clean only updater scratch'
    $rollbackCopy = New-Copy 'rollback' 'v0.1.4' 'portable'
    $oldPayload = Join-Path $rollbackCopy '.neuron-update-backup/v0.1.3-20261001-010203'
    New-Item -ItemType Directory -Path $oldPayload -Force | Out-Null
    [IO.File]::WriteAllText((Join-Path $oldPayload 'SOURCE.txt'), 'Neuron v0.1.3 - fixture', $utf8)
    $profileDir = Join-Path $rollbackCopy 'profiles'
    New-Item -ItemType Directory -Path $profileDir | Out-Null
    [IO.File]::WriteAllText((Join-Path $profileDir 'keep.toml'), 'keep my profile', $utf8)
    $result = Invoke-UpdaterFixture $rollbackCopy 'rollback' @('-NoRelaunch')
    Assert ($result.Code -eq 0 -and $result.Output -match 'installed_version: v0.1.3' -and $result.Output -match 'RESULT: rolled-back') ('rollback must restore the saved payload: ' + $result.Output)
    Assert ([IO.File]::ReadAllText((Join-Path $profileDir 'keep.toml')) -eq 'keep my profile') 'rollback must retain current user configuration'

    $startupA = New-Copy 'startup-a' 'v0.1.4' 'portable'
    $startupB = New-Copy 'startup-b' 'v0.1.5' 'installer'
    $status = Invoke-UpdaterFixture $null 'status' @('-Json') @($startupA, $startupB)
    Assert ($status.Code -eq 0) ('status must return inventory despite multiple startup targets: ' + $status.Output)
    $statusJson = $status.Output | ConvertFrom-Json
    $startupCopies = @($statusJson.Copies | Where-Object { $_.Startup })
    Assert ($statusJson.StartupTargetsAmbiguous -and -not $statusJson.SelectedDirectory) ("ambiguous status must report the ambiguity without selecting an update target: $($status.Output)")
    Assert ($startupCopies.Count -eq 2 -and @($statusJson.StartupTargetDirectories).Count -eq 2) 'ambiguous status must include both startup copies'
    foreach ($action in @('check', 'apply')) {
        $ambiguous = Invoke-UpdaterFixture $null $action @() @($startupA, $startupB)
        Assert ($ambiguous.Code -ne 0 -and $ambiguous.Output -match 'TASK_AMBIGUOUS' -and $ambiguous.Output -match 'RESULT: blocked') "$action must remain blocked when no target resolves multiple startup copies"
    }

    $Tasks = @('Neuron')
    $script:taskEnabled = 'false'
    function schtasks {
        $global:LASTEXITCODE = 0
        '<Task xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task"><Triggers><LogonTrigger><Enabled>true</Enabled></LogonTrigger></Triggers><Settings><Enabled>' + $script:taskEnabled + '</Enabled></Settings><Actions><Exec><Command>C:\A&amp;B\neuron-app.exe</Command></Exec></Actions></Task>'
    }
    Assert (@(Get-TaskExeDirs).Count -eq 0) 'a disabled task must not select the update target'
    $script:taskEnabled = 'true'
    Assert ((Get-TaskExeDirs) -eq 'C:\A&B') 'startup paths must decode XML entities'

    $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    $ownTask = [pscustomobject]@{
        TaskName='Neuron'
        Settings=[pscustomobject]@{Enabled=$true;ExecutionTimeLimit='PT0S'}
        Triggers=@([pscustomobject]@{CimClass=[pscustomobject]@{CimClassName='MSFT_TaskLogonTrigger'};Enabled=$true})
        Actions=@([pscustomobject]@{Execute=(Join-Path $sourceDir 'neuron-app.exe');Arguments='--tray';WorkingDirectory=$sourceDir})
        Principal=[pscustomobject]@{UserId=$sid;RunLevel='Limited';LogonType='Interactive'}
    }
    Assert ((Get-ReleaseLaunchMode $ownTask (Join-Path $sourceDir 'neuron-app.exe') $false) -eq 'task') 'a healthy task for this build may be reused'
    Assert ((Get-ReleaseLaunchMode $ownTask (Join-Path $DefaultDir 'neuron-app.exe') $false) -eq 'direct') 'a developer launch must preserve another copy startup'
    Assert ((Get-ReleaseLaunchMode $null (Join-Path $sourceDir 'neuron-app.exe') $false) -eq 'direct') 'a developer launch must preserve startup-off'
    Assert ((Get-ReleaseLaunchMode $null (Join-Path $sourceDir 'neuron-app.exe') $true) -eq 'register') 'only an explicit request may claim startup'
    $NoRelaunch = $true; $ReleaseDir = $sourceDir
    function Get-Process { param($Name, $ErrorAction) @(
        [pscustomobject]@{ProcessName='neuron-app'; Path=(Join-Path $DefaultDir 'neuron-app.exe'); Id=456},
        [pscustomobject]@{ProcessName='neuron-app'; Path=(Join-Path $sourceDir 'neuron-app.exe'); Id=123}
    ) }
    Assert (@(Get-ReleaseProcesses).Count -eq 1) 'build-only must leave unrelated installed processes running'
    $releaseTrays = @(Get-ReleaseTrayProcesses)
    Assert ($releaseTrays.Count -eq 1 -and $releaseTrays[0].Id -eq 123) 'shutdown selection must include only the selected tray PID'
    $releaseAst = [Management.Automation.Language.Parser]::ParseFile($release, [ref]$null, [ref]$null)
    $releaseSignal = $releaseAst.Find({ param($node)
        $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Signal-ReleaseTrayProcesses'
    }, $false)
    Assert ($releaseSignal.Extent.Text -match 'Shutdown\.\x27\s*\+\s*\$process\.Id') 'release shutdown events must be PID-scoped'
    Assert ($releaseSignal.Extent.Text -match 'Get-Process\s+-Id\s+\$process\.Id') 'release must recheck the selected PID before signaling'

    $probe = Get-InnoCommand 'OtherNeuronCopyInUse'
    $target = Join-Path $DefaultDir 'neuron-app.exe'
    $foreign = Join-Path $sourceDir 'neuron-app.exe'
    $foreignTask = @{TaskName='Neuron';Actions=@(@{Execute=$foreign})}
    $installedTask = @{TaskName='Neuron (elevated tray)';Actions=@(@{Execute=$target})}
    Assert ((Invoke-InnoFixture $probe @() @()).Code -eq 0) 'clean install must have no foreign owner'
    Assert ((Invoke-InnoFixture $probe @(@{Path=$target}) @()).Code -eq 0) 'installed-only update must retain its own lifecycle'
    Assert ((Invoke-InnoFixture $probe @(@{Path=$foreign}) @()).Code -eq 1) 'checkpoint install must recognize a running source copy'
    Assert ((Invoke-InnoFixture $probe @() @($foreignTask)).Code -eq 1) 'checkpoint install must preserve a source startup task'
    Assert ((Invoke-InnoFixture $probe @(@{Path=$null}) @()).Code -eq 2) 'an unreadable running path must block lifecycle changes'
    Assert ((Invoke-InnoFixture $probe @() @() $true).Code -eq 2) 'a failed startup query must block lifecycle changes'
    Assert ((Invoke-InnoFixture $probe @() @() $false $true).Code -eq 2) 'a failed process query must block rather than look like a foreign owner'
    $remove = Get-InnoCommand 'RemoveInstalledStartupTasks'
    $result = Invoke-InnoFixture $remove @() @($foreignTask, $installedTask)
    Assert ($result.Code -eq 0 -and $result.Removed.Count -eq 1 -and $result.Removed[0] -eq 'Neuron (elevated tray)') 'checkpoint uninstall may remove only its own startup action'
    Assert ($result.Tasks.Count -eq 1 -and $result.Tasks[0].Actions[0].Execute -eq $foreign) 'checkpoint uninstall must preserve the other startup target'
    $mixed = @{TaskName='Neuron';Actions=@(@{Execute=$target}, @{Execute=$foreign})}
    $result = Invoke-InnoFixture $remove @() @($mixed)
    Assert ($result.Code -ne 0 -and $result.Removed.Count -eq 0) 'mixed startup actions must never be deleted by guesswork'
    Assert ((Invoke-InnoFixture $remove @() @() $true).Code -ne 0) 'uninstall must fail closed when task inspection fails'
    $stop = $setup.Substring($setup.IndexOf('function StopInstalledTray()'))
    $stop = $stop.Substring(0, $stop.IndexOf("`nend;"))
    Assert ($stop.IndexOf('if($own.Count -eq 0){exit 0}') -lt $stop.IndexOf('EventWaitHandle')) 'uninstall must check its own process before signaling a shutdown event'
    Assert ($stop.Contains("Shutdown.''+`$p.Id")) 'uninstall shutdown events must include the selected process id'
    Assert (-not $stop.Contains("OpenExisting(''Local\WofloLabs.Neuron.Shutdown'')")) 'uninstall must not signal the global legacy event'
    $prepare = $setup.Substring($setup.IndexOf('function PrepareToInstall('))
    $prepare = $prepare.Substring(0, $prepare.IndexOf("`nend;"))
    Assert ($prepare -match 'if RestartTray then SignalInstalledTray\(\)') 'setup must use the PID-scoped installed-tray signal helper'
    $signalInstalled = $setup.Substring($setup.IndexOf('function SignalInstalledTray()'))
    $signalInstalled = $signalInstalled.Substring(0, $signalInstalled.IndexOf("`nend;"))
    Assert ($signalInstalled.Contains("Shutdown.''+`$p.Id")) 'setup signal helper must address each owned process by pid'

    $transactionQuery = Get-InnoCommand 'ChromaTransactionPresent'
    Assert ((Invoke-InnoFixture $transactionQuery @() @()).Code -eq 0) 'empty transaction inventory must report none'
    foreach ($entryName in @('NeuronChromaBroker.installing-owner-test', 'NeuronChromaBroker.removing-owner-test', 'NeuronChromaBroker.recovery-foreign-test')) {
        $result = Invoke-InnoFixture $transactionQuery @() @() $false $false @(@{Name=$entryName})
        Assert ($result.Code -eq 1 -and $result.Removed.Count -eq 0) 'all transaction prefixes must block the no-broker fast path without task mutation'
    }
    Assert ((Invoke-InnoFixture $transactionQuery @() @() $false $false @() $true).Code -eq 2) 'transaction query failures must stay distinct from no transaction'

    $uninstallerSource = [IO.File]::ReadAllText((Join-Path $PSScriptRoot 'uninstall-chroma-broker.ps1'))
    $uninstallerAst = [Management.Automation.Language.Parser]::ParseInput($uninstallerSource, [ref]$null, [ref]$null)
    $recoveryAssignment = @($uninstallerAst.FindAll({ param($node)
        $node -is [Management.Automation.Language.AssignmentStatementAst] -and $node.Left.Extent.Text -eq '$recoveryState'
    }, $true))
    $recoveryGuard = @($uninstallerAst.FindAll({ param($node)
        $node -is [Management.Automation.Language.IfStatementAst] -and $node.Clauses[0].Item1.Extent.Text -eq '$recoveryState.Count -gt 0'
    }, $true))
    Assert ($recoveryAssignment.Count -eq 1 -and $recoveryGuard.Count -eq 1) 'uninstall must have one recovery preflight guard'
    Assert ($recoveryGuard[0].Extent.EndOffset -lt $uninstallerSource.IndexOf('foreach ($stale')) 'recovery inspection must precede stale-file cleanup'
    Assert ($recoveryGuard[0].Extent.EndOffset -lt $uninstallerSource.IndexOf('Disable-ScheduledTask')) 'recovery inspection must precede task mutation'
    $guardText = $recoveryAssignment[0].Extent.Text + "`n" + $recoveryGuard[0].Extent.Text
    $guardAst = [Management.Automation.Language.Parser]::ParseInput($guardText, [ref]$null, [ref]$null)
    foreach ($command in $guardAst.FindAll({param($node) $node -is [Management.Automation.Language.CommandAst]}, $true)) {
        if ($command.GetCommandName() -notin @('Get-ChildItem','Where-Object')) { throw 'uninstall preflight fixture refuses an unmocked command' }
    }
    $recoveryPrefix = 'NeuronChromaBroker.recovery-'
    foreach ($shape in @('none','directory','file','unrelated','query-failure')) {
        $programFiles64 = Join-Path $fixtureRoot ('recovery-preflight-' + $shape)
        if ($shape -ne 'query-failure') { [IO.Directory]::CreateDirectory($programFiles64) | Out-Null }
        $entryPath = Join-Path $programFiles64 'NeuronChromaBroker.recovery-foreign-test'
        if ($shape -eq 'directory') { [IO.Directory]::CreateDirectory($entryPath) | Out-Null }
        if ($shape -eq 'file') { [IO.File]::WriteAllText($entryPath, 'retain this recovery record', $utf8) }
        if ($shape -eq 'unrelated') { [IO.File]::WriteAllText((Join-Path $programFiles64 'unrelated'), 'keep', $utf8) }
        $guardFailed = $false
        try { & ([scriptblock]::Create($guardText)) } catch { $guardFailed = $true }
        Assert ($guardFailed -eq ($shape -in @('directory','file','query-failure'))) "uninstall recovery preflight returned the wrong result for $shape"
        if ($shape -eq 'directory') { Assert ([IO.Directory]::Exists($entryPath)) 'uninstall must preserve a recovery directory' }
        if ($shape -eq 'file') { Assert ([IO.File]::ReadAllText($entryPath) -eq 'retain this recovery record') 'uninstall must preserve a recovery file' }
    }
    Write-Output "lifecycle fixtures: $script:Assertions assertions passed"
} finally {
    # Only the fresh, resolved fixture directory belongs to this test.
    $resolved = [IO.Path]::GetFullPath($fixtureRoot)
    if ((Split-Path -Parent $resolved) -ne [IO.Path]::GetTempPath().TrimEnd('\') -or
        (Split-Path -Leaf $resolved) -notmatch '^neuron-lifecycle-test-[0-9a-f]{32}$') { throw 'fixture cleanup path escaped temp' }
    Remove-Item -LiteralPath $resolved -Recurse -Force
}
