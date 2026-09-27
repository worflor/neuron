; SPDX-FileCopyrightText: 2026 Woflo Labs
; SPDX-License-Identifier: GPL-3.0-or-later
; Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

#ifndef AppVersion
  #error AppVersion must be supplied with /DAppVersion=...
#endif
#ifndef PackageLabel
  #error PackageLabel must be supplied with /DPackageLabel=...
#endif
#ifndef StageDir
  #error StageDir must be supplied with /DStageDir=...
#endif

#define BrokerHash GetSHA256OfFile(StageDir + "\neuron-chroma-broker.exe")
#define BrokerInstallerHash GetSHA256OfFile(StageDir + "\install-chroma-broker.ps1")
#define BrokerUninstallerHash GetSHA256OfFile(StageDir + "\uninstall-chroma-broker.ps1")

[Setup]
AppId={{9E6F3A1F-68A7-4314-B05F-902A8EDABF14}
AppName=Neuron
AppVersion={#AppVersion}
AppPublisher=Woflo Labs
AppPublisherURL=https://github.com/worflor/neuron
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
DefaultDirName={localappdata}\Programs\Neuron
DefaultGroupName=Neuron
PrivilegesRequired=lowest
OutputBaseFilename=neuron-{#PackageLabel}-windows-x86_64-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\neuron-app.exe
CloseApplications=force
RestartApplications=no

[Files]
Source: "{#StageDir}\*"; DestDir: "{app}"; Excludes: "portable.flag"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "{#StageDir}\install-chroma-broker.ps1"; Flags: dontcopy
Source: "{#StageDir}\neuron-chroma-broker.exe"; Flags: dontcopy

[InstallDelete]
Type: files; Name: "{app}\portable.flag"

[Icons]
Name: "{autoprograms}\Neuron"; Filename: "{app}\neuron-app.exe"; WorkingDir: "{app}"

[Code]
var
  RestoreAutostart: Boolean;
  RestartTray: Boolean;
  InstallOwnerSid: String;
  NeedsTaskCleanup: Boolean;
  PreparedInstall: Boolean;
  CompletedInstall: Boolean;
  CustomExitCode: Integer;

function PowerShellLiteral(Value: String): String;
begin
  Result := Value;
  StringChangeEx(Result, '''', '''''', True);
  Result := '''' + Result + '''';
end;

function ResolveCurrentUserSid(): String;
var
  ResultCode: Integer;
  SidPath: String;
  Params: String;
  Lines: TArrayOfString;
begin
  Result := '';
  SidPath := ExpandConstant('{tmp}\neuron-owner.sid');
  DeleteFile(SidPath);
  Params := '-NoProfile -NonInteractive -Command "$sid=' +
    '[Security.Principal.WindowsIdentity]::GetCurrent().User.Value;' +
    '[IO.File]::WriteAllText(' + PowerShellLiteral(SidPath) +
    ',$sid,[Text.UTF8Encoding]::new($false))"';
  if Exec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'),
      Params, '', SW_HIDE, ewWaitUntilTerminated, ResultCode) and
     (ResultCode = 0) and LoadStringsFromFile(SidPath, Lines) and
     (GetArrayLength(Lines) >= 1) then
    Result := Trim(Lines[0]);
  DeleteFile(SidPath);
  if Pos('S-1-', Result) <> 1 then Result := '';
end;

function OwnershipRecordOwner(Path: String): String;
var
  Lines: TArrayOfString;
begin
  Result := '';
  if (not FileExists(Path)) or (not LoadStringsFromFile(Path, Lines)) then Exit;
  if GetArrayLength(Lines) <> 4 then Exit;
  if (Length(Trim(Lines[0])) = 64) and
     (Lines[1] = 'SYSTEM startup') and
     (Lines[2] = 'task-acl-v1') then
    Result := Trim(Lines[3]);
end;

function InspectChromaTaskAtSetup(): Integer;
var
  ResultCode: Integer;
begin
  if not Exec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'),
    '-NoProfile -NonInteractive -Command "try{Get-ScheduledTask -TaskName ''Neuron Chroma broker'' -ErrorAction Stop|Out-Null;exit 0}catch{if($_.CategoryInfo.Category -eq ''ObjectNotFound''){exit 1};exit 2}"',
    '', SW_HIDE, ewWaitUntilTerminated, ResultCode) then
    Result := 2
  else
    Result := ResultCode;
end;

function InspectUserAutostartTasks(): Integer;
var
  ResultCode: Integer;
  Params: String;
begin
  Params := '-NoProfile -NonInteractive -Command "$present=$false;$unsafe=$false;$enabled=$false;' +
    '$names=@(''Neuron'',''Neuron (elevated tray)'');foreach($name in $names){' +
    'try{$t=Get-ScheduledTask -TaskName $name -ErrorAction Stop}catch{' +
    'if($_.CategoryInfo.Category -eq ''ObjectNotFound''){continue};exit 8};$present=$true;' +
    'if($t.Settings.Enabled -and @($t.Triggers|Where-Object{' +
    '$_.CimClass.CimClassName -eq ''MSFT_TaskLogonTrigger'' -and $_.Enabled}).Count -gt 0){$enabled=$true};' +
    'if($t.Principal.RunLevel -ne ''Limited''){$unsafe=$true}};' +
    '$code=0;if($present){$code+=1};if($enabled){$code+=2};if($unsafe){$code+=4};exit $code"';
  if not Exec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'),
    Params, '', SW_HIDE, ewWaitUntilTerminated, ResultCode) then
    Result := 8
  else
    Result := ResultCode;
end;

function NeuronTrayRunning(): Boolean;
var
  ResultCode: Integer;
  Params: String;
begin
  Params := '-NoProfile -NonInteractive -Command "$target=' +
    PowerShellLiteral(ExpandConstant('{app}\neuron-app.exe')) + ';' +
    '$p=@(Get-Process -Name neuron-app -ErrorAction SilentlyContinue|Where-Object{' +
    'try{$_.Path -ieq $target}catch{$false}});if($p.Count -gt 0){exit 0};exit 1"';
  Result := Exec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'),
    Params,
    '', SW_HIDE, ewWaitUntilTerminated, ResultCode) and (ResultCode = 0);
end;

function StopInstalledTray(): Boolean;
var
  ResultCode: Integer;
  Params: String;
  AppExe: String;
begin
  AppExe := ExpandConstant('{app}\neuron-app.exe');
  Params := '-NoProfile -NonInteractive -Command "$ErrorActionPreference=''Stop'';' +
    '$target=' + PowerShellLiteral(AppExe) + ';' +
    'try{$e=[Threading.EventWaitHandle]::OpenExisting(''Local\WofloLabs.Neuron.Shutdown'');' +
    '$e.Set()|Out-Null;$e.Dispose()}catch{};' +
    '$deadline=[DateTime]::UtcNow.AddSeconds(5);do{' +
    '$p=@(Get-Process -Name neuron-app -ErrorAction SilentlyContinue|Where-Object{' +
    'try{$_.Path -ieq $target}catch{$false}});if($p.Count -eq 0){exit 0};' +
    'Start-Sleep -Milliseconds 100}while([DateTime]::UtcNow -lt $deadline);' +
    '$p|Stop-Process -Force -ErrorAction SilentlyContinue;' +
    '$deadline=[DateTime]::UtcNow.AddSeconds(5);do{' +
    '$left=@(Get-Process -Name neuron-app -ErrorAction SilentlyContinue|Where-Object{' +
    'try{$_.Path -ieq $target}catch{$false}});if($left.Count -eq 0){exit 0};' +
    'Start-Sleep -Milliseconds 100}while([DateTime]::UtcNow -lt $deadline);exit 1"';
  Result := Exec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'),
    Params, '', SW_HIDE, ewWaitUntilTerminated, ResultCode) and (ResultCode = 0);
end;

procedure RemoveLegacyAutostartTask();
var
  ResultCode: Integer;
begin
  if not Exec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'),
    '-NoProfile -NonInteractive -Command "$t=Get-ScheduledTask -TaskName ''Neuron (elevated tray)'' -ErrorAction SilentlyContinue;if($t){Unregister-ScheduledTask -TaskName ''Neuron (elevated tray)'' -Confirm:$false -ErrorAction SilentlyContinue}"',
    '', SW_HIDE, ewWaitUntilTerminated, ResultCode) then
    Log('Could not launch legacy autostart cleanup');
end;

function LegacyAutostartTaskPresent(): Boolean;
var
  ResultCode: Integer;
begin
  Result := Exec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'),
    '-NoProfile -NonInteractive -Command "if(Get-ScheduledTask -TaskName ''Neuron (elevated tray)'' -ErrorAction SilentlyContinue){exit 0};exit 1"',
    '', SW_HIDE, ewWaitUntilTerminated, ResultCode) and (ResultCode = 0);
end;

function RemoveAutostartTasksElevated(): Boolean;
var
  ResultCode: Integer;
  Params: String;
begin
  Params := '-NoProfile -NonInteractive -Command "$ErrorActionPreference=''Stop'';' +
    'foreach($name in @(''Neuron'',''Neuron (elevated tray)'')){' +
    '$t=Get-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue;if($t){' +
    'Stop-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue;' +
    'Unregister-ScheduledTask -TaskName $name -Confirm:$false -ErrorAction Stop}}"';
  Result := ShellExec('runas', ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'),
    Params, '', SW_HIDE, ewWaitUntilTerminated, ResultCode) and (ResultCode = 0);
end;

function RegisterLimitedAutostartTask(): Boolean;
var
  ResultCode: Integer;
  Params: String;
  AppExe: String;
  AppDir: String;
begin
  AppExe := ExpandConstant('{app}\neuron-app.exe');
  AppDir := ExpandConstant('{app}');
  Params := '-NoProfile -NonInteractive -Command "$ErrorActionPreference=''Stop'';' +
    '$user=[Security.Principal.WindowsIdentity]::GetCurrent().Name;' +
    '$action=New-ScheduledTaskAction -Execute ' + PowerShellLiteral(AppExe) +
    ' -Argument ''--tray'' -WorkingDirectory ' + PowerShellLiteral(AppDir) + ';' +
    '$trigger=New-ScheduledTaskTrigger -AtLogOn -User $user;$trigger.Delay=''PT15S'';' +
    '$principal=New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited;' +
    '$settings=New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) ' +
    '-MultipleInstances IgnoreNew -StartWhenAvailable -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries;' +
    'Register-ScheduledTask -TaskName ''Neuron'' -Action $action -Trigger $trigger ' +
    '-Principal $principal -Settings $settings -Force|Out-Null"';
  Result := Exec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'),
    Params, '', SW_HIDE, ewWaitUntilTerminated, ResultCode) and (ResultCode = 0);
end;

function ChromaTaskHealthy(ExpectedExe: String): Boolean;
var
  ResultCode: Integer;
  Params: String;
  ExpectedDir: String;
begin
  ExpectedDir := ExtractFileDir(ExpectedExe);
  Params := '-NoProfile -NonInteractive -Command "$ErrorActionPreference=''Stop'';' +
    'try {$t=Get-ScheduledTask -TaskName ''Neuron Chroma broker'' -ErrorAction Stop} catch {exit 1};' +
    '$svc=New-Object -ComObject ''Schedule.Service'';$svc.Connect();' +
    '$sddl=$svc.GetFolder(''\'').GetTask(''Neuron Chroma broker'').GetSecurityDescriptor(7);' +
    '$sd=[Security.AccessControl.CommonSecurityDescriptor]::new($false,$false,$sddl);' +
    '$prot=($sd.ControlFlags -band [Security.AccessControl.ControlFlags]::DiscretionaryAclProtected) -ne 0;' +
    '$rights=@{''S-1-5-18''=0x001F01FF;''S-1-5-32-544''=0x001F01FF;''S-1-5-32-545''=0x00120089};' +
    'if($sd.Owner.Value -ne ''S-1-5-32-544'' -or -not $prot -or ' +
    '-not $sd.DiscretionaryAcl.IsCanonical -or $sd.DiscretionaryAcl.Count -ne 3){exit 1};' +
    'foreach($ace in $sd.DiscretionaryAcl){$sid=$ace.SecurityIdentifier.Value;' +
    'if(-not $rights.ContainsKey($sid) -or ' +
    '$ace.AceQualifier -ne [Security.AccessControl.AceQualifier]::AccessAllowed -or ' +
    '$ace.AccessMask -ne $rights[$sid]){exit 1}};' +
    '$p=[string]$t.Principal.UserId;' +
    'if (($p -notin @(''SYSTEM'',''S-1-5-18'')) -or $t.Principal.LogonType -ne ''ServiceAccount'' -or ' +
    '$t.Principal.RunLevel -ne ''Highest'' -or -not $t.Settings.Enabled -or $t.Actions.Count -ne 1 -or ' +
    '$t.Actions[0].Execute -ine ' + PowerShellLiteral(ExpectedExe) + ' -or $t.Actions[0].Arguments -or ' +
    '$t.Actions[0].WorkingDirectory -ine ' + PowerShellLiteral(ExpectedDir) + ' -or ' +
    '$t.Triggers.Count -ne 1 -or $t.Triggers[0].CimClass.CimClassName -ne ''MSFT_TaskBootTrigger'') {exit 1};exit 0"';
  Result := Exec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'),
    Params, '', SW_HIDE, ewWaitUntilTerminated, ResultCode) and (ResultCode = 0);
end;

function ChromaTaskPresent(): Boolean;
var
  ResultCode: Integer;
begin
  Result := Exec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'),
    '-NoProfile -NonInteractive -Command "try{Get-ScheduledTask -TaskName ''Neuron Chroma broker'' -ErrorAction Stop|Out-Null;exit 0}catch{if($_.CategoryInfo.Category -eq ''ObjectNotFound''){exit 1};exit 2}"',
    '', SW_HIDE, ewWaitUntilTerminated, ResultCode) and (ResultCode <> 1);
end;

function ChromaTransactionPresent(): Boolean;
var
  ResultCode: Integer;
  Params: String;
begin
  Params := '-NoProfile -NonInteractive -Command "$p=' +
    PowerShellLiteral(ExpandConstant('{commonpf64}')) + ';' +
    '$d=Get-ChildItem -LiteralPath $p -Directory -Force -ErrorAction SilentlyContinue|' +
    'Where-Object{$_.Name -like ''NeuronChromaBroker.installing-*'' -or ' +
    '$_.Name -like ''NeuronChromaBroker.removing-*''}|Select-Object -First 1;' +
    'if($d){exit 0};exit 1"';
  Result := Exec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'),
    Params, '', SW_HIDE, ewWaitUntilTerminated, ResultCode) and (ResultCode = 0);
end;

procedure RemoveUserAutostartTasks();
var
  ResultCode: Integer;
begin
  if not Exec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'),
    '-NoProfile -NonInteractive -Command "foreach($n in @(''Neuron'',''Neuron (elevated tray)'')){if(Get-ScheduledTask -TaskName $n -ErrorAction SilentlyContinue){Unregister-ScheduledTask -TaskName $n -Confirm:$false -ErrorAction SilentlyContinue}}"',
    '', SW_HIDE, ewWaitUntilTerminated, ResultCode) then
    Log('Could not launch user autostart cleanup');
end;

function RemoveUserAutostartTasksAndVerify(): Boolean;
var
  TaskState: Integer;
begin
  RemoveUserAutostartTasks();
  TaskState := InspectUserAutostartTasks();
  Result := TaskState = 0;
end;

function InitializeSetup(): Boolean;
var
  TaskState: Integer;
  ChromaTaskState: Integer;
  ReceiptPath: String;
  PendingPath: String;
  ReceiptOwner: String;
  PendingOwner: String;
begin
  RestoreAutostart := False;
  RestartTray := False;
  NeedsTaskCleanup := False;
  PreparedInstall := False;
  CompletedInstall := False;
  CustomExitCode := 0;
  InstallOwnerSid := ResolveCurrentUserSid();
  if InstallOwnerSid = '' then begin
    MsgBox('Neuron setup could not identify the current Windows user. No files were changed.', mbCriticalError, MB_OK);
    Result := False;
    Exit;
  end;
  ReceiptPath := ExpandConstant('{commonpf64}\NeuronChromaBroker\broker-installed.sha256');
  PendingPath := ExpandConstant('{commonpf64}\NeuronChromaBroker\broker-install.pending');
  ChromaTaskState := InspectChromaTaskAtSetup();
  if ChromaTaskState = 2 then begin
    MsgBox('Neuron setup could not verify the machine-wide Chroma broker task. No files were changed.', mbCriticalError, MB_OK);
    Result := False;
    Exit;
  end;
  if (not FileExists(ReceiptPath)) and (not FileExists(PendingPath)) and
     (DirExists(ExpandConstant('{commonpf64}\NeuronChromaBroker')) or (ChromaTaskState = 0)) then begin
    MsgBox('Existing machine-wide Neuron Chroma broker state has no ownership receipt. Repair or remove it from an administrator PowerShell before installing Neuron.', mbCriticalError, MB_OK);
    Result := False;
    Exit;
  end;
  ReceiptOwner := OwnershipRecordOwner(ReceiptPath);
  PendingOwner := OwnershipRecordOwner(PendingPath);
  if ((ReceiptOwner <> '') and (CompareText(ReceiptOwner, InstallOwnerSid) <> 0)) or
     ((ReceiptOwner = '') and (PendingOwner <> '') and
      (CompareText(PendingOwner, InstallOwnerSid) <> 0)) or
     ((ReceiptOwner = '') and (PendingOwner = '') and
      (FileExists(ReceiptPath) or FileExists(PendingPath))) then begin
    MsgBox('The machine-wide Neuron Chroma broker ownership record is invalid or belongs to another Windows user. Use the owning account to repair or remove it.', mbCriticalError, MB_OK);
    Result := False;
    Exit;
  end;
  TaskState := InspectUserAutostartTasks();
  if TaskState = 8 then begin
    MsgBox('Neuron setup could not verify the startup task. No files were changed.', mbCriticalError, MB_OK);
    Result := False;
    Exit;
  end;
  RestoreAutostart := (TaskState = 3) or (TaskState = 7);
  NeedsTaskCleanup := (TaskState = 5) or (TaskState = 7);
  Result := True;
end;

procedure RestartInstalledTray();
var
  ResultCode: Integer;
  Params: String;
begin
  if not RestartTray then Exit;
  Params := '-NoProfile -NonInteractive -Command "$t=Get-ScheduledTask -TaskName ''Neuron'' -ErrorAction SilentlyContinue;' +
    'if($t -and $t.Settings.Enabled -and $t.Principal.RunLevel -eq ''Limited''){' +
    'Start-ScheduledTask -TaskName ''Neuron'' -ErrorAction Stop}' +
    'else{Start-Process -FilePath ' + PowerShellLiteral(ExpandConstant('{app}\neuron-app.exe')) +
    ' -ArgumentList ''--tray'' -WorkingDirectory ' + PowerShellLiteral(ExpandConstant('{app}')) + '}"';
  if not Exec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'),
    Params, '', SW_HIDE, ewWaitUntilTerminated, ResultCode) or (ResultCode <> 0) then
    Log('Neuron tray relaunch failed with exit code ' + IntToStr(ResultCode));
end;

procedure RestoreFailedSetupState();
begin
  if RestoreAutostart and (not RegisterLimitedAutostartTask()) then
    Log('Could not restore the limited startup task after setup stopped');
  RestartInstalledTray();
end;

function EnsureProtectedChromaBroker(): String;
var
  ResultCode: Integer;
  ScriptPath: String;
  BrokerPath: String;
  ProtectedPath: String;
  ReceiptPath: String;
  Receipt: TArrayOfString;
  ErrorPath: String;
  BrokerErrors: TArrayOfString;
  Params: String;
  CleanupScript: String;
  I: Integer;
begin
  Result := '';
  ProtectedPath := ExpandConstant('{commonpf64}\NeuronChromaBroker\neuron-chroma-broker.exe');
  ReceiptPath := ExpandConstant('{commonpf64}\NeuronChromaBroker\broker-installed.sha256');
  if (not NeedsTaskCleanup) and FileExists(ReceiptPath) and FileExists(ProtectedPath) and
     (CompareText(GetSHA256OfFile(ProtectedPath), '{#BrokerHash}') = 0) and
     ChromaTaskHealthy(ProtectedPath) and
     (not LegacyAutostartTaskPresent()) then begin
    if LoadStringsFromFile(ReceiptPath, Receipt) then begin
       if GetArrayLength(Receipt) = 4 then begin
         if (CompareText(Receipt[0], '{#BrokerHash}') = 0) and
            (Receipt[1] = 'SYSTEM startup') and
            (Receipt[2] = 'task-acl-v1') and
            (CompareText(Trim(Receipt[3]), InstallOwnerSid) = 0) then begin
          Log('Protected Chroma broker already matches this package');
          Exit;
        end;
      end;
    end;
  end;

  Log('Preparing protected Chroma broker before app files change');
  try
    ExtractTemporaryFile('install-chroma-broker.ps1');
    ExtractTemporaryFile('neuron-chroma-broker.exe');
  except
    Result := 'Neuron setup could not extract its protected Chroma broker payload. No app files were changed.';
    Exit;
  end;
  ScriptPath := ExpandConstant('{tmp}\install-chroma-broker.ps1');
  BrokerPath := ExpandConstant('{tmp}\neuron-chroma-broker.exe');
  if (not FileExists(ScriptPath)) or
     (CompareText(GetSHA256OfFile(ScriptPath), '{#BrokerInstallerHash}') <> 0) or
     (not FileExists(BrokerPath)) or
     (CompareText(GetSHA256OfFile(BrokerPath), '{#BrokerHash}') <> 0) then begin
    Log('Chroma broker package hash verification failed');
    Result := 'The Chroma broker files did not match this installer. No app files were changed.';
    Exit;
  end;

  { Elevated PowerShell hashes and executes one in-memory read of the script. Checking here and
    then using -File would let another process replace the writable script during UAC. }
  ErrorPath := ExpandConstant('{tmp}\neuron-chroma-setup-error.txt');
  DeleteFile(ErrorPath);
  CleanupScript := '';
  if NeedsTaskCleanup then
    CleanupScript :=
      'foreach($name in @(''Neuron'',''Neuron (elevated tray)'')){' +
      '$t=Get-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue;if($t){' +
      'Stop-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue;' +
      'Unregister-ScheduledTask -TaskName $name -Confirm:$false -ErrorAction Stop}};' +
      'foreach($name in @(''Neuron'',''Neuron (elevated tray)'')){' +
      'if(Get-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue){' +
      'throw ''startup task cleanup did not complete''}};';
  Params := '-NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "' +
    '$ErrorActionPreference=''Stop'';$errorPath=' + PowerShellLiteral(ErrorPath) + ';try{' +
    '$p=' + PowerShellLiteral(ScriptPath) +
    ';$bytes=[IO.File]::ReadAllBytes($p);' +
    '$actual=[BitConverter]::ToString([Security.Cryptography.SHA256]::Create().ComputeHash($bytes)).Replace(''-'','''');' +
    'if ($actual -ine ''{#BrokerInstallerHash}'') {throw ''broker installer hash mismatch''};' +
    '$code=[Text.Encoding]::UTF8.GetString($bytes);' +
    '&([ScriptBlock]::Create($code)) -BinaryPath ' + PowerShellLiteral(BrokerPath) +
    ' -ExpectedSha256 {#BrokerHash} -OwnerSid ' + PowerShellLiteral(InstallOwnerSid) + ';' +
    CleanupScript +
    '}catch{[IO.File]::WriteAllText($errorPath,($_|Out-String),[Text.UTF8Encoding]::new($false));exit 1}"';
  if not ShellExec('runas', ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'),
    Params, '', SW_HIDE, ewWaitUntilTerminated, ResultCode) then begin
    Log('Chroma broker elevation failed: ' + SysErrorMessage(ResultCode));
    Result := 'Administrator approval is required to install Neuron. No app files were changed.';
    Exit;
  end;
  Log('Chroma broker setup exit code: ' + IntToStr(ResultCode));
  if ResultCode <> 0 then begin
    if LoadStringsFromFile(ErrorPath, BrokerErrors) then
      for I := 0 to GetArrayLength(BrokerErrors) - 1 do
        Log('Chroma broker setup: ' + BrokerErrors[I]);
    Result := 'Native Chroma setup failed. No app files were changed.';
    Exit;
  end;
  DeleteFile(ErrorPath);
  if (not FileExists(ProtectedPath)) or
     (CompareText(GetSHA256OfFile(ProtectedPath), '{#BrokerHash}') <> 0) or
     (not ChromaTaskHealthy(ProtectedPath)) or
     (NeedsTaskCleanup and (InspectUserAutostartTasks() <> 0)) then begin
    Result := 'Neuron could not verify the protected Chroma broker. No app files were changed.';
    Exit;
  end;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  ResultCode: Integer;
begin
  RestartTray := NeuronTrayRunning();
  { The broker and any unsafe legacy task are resolved before Inno mutates the app directory. }
  Result := EnsureProtectedChromaBroker();
  if Result <> '' then begin
    CustomExitCode := 1;
    RestoreFailedSetupState();
    Exit;
  end;
  { Current builds exit through their normal tray-quit path and flush pending state. Older builds
    do not own the event; Restart Manager's bounded forced-close fallback handles those. }
  Exec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'),
    '-NoProfile -NonInteractive -Command "try{$e=[Threading.EventWaitHandle]::OpenExisting(''Local\WofloLabs.Neuron.Shutdown'');$e.Set()|Out-Null;$e.Dispose()}catch{}"',
    '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  PreparedInstall := True;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep <> ssPostInstall then Exit;
  if RestoreAutostart then begin
    if not RegisterLimitedAutostartTask() then begin
      CustomExitCode := 1;
      RaiseException('Neuron setup could not restore the limited startup task.');
    end;
    RemoveLegacyAutostartTask();
  end else if not RemoveUserAutostartTasksAndVerify() then begin
    CustomExitCode := 1;
    RaiseException('Neuron setup could not remove its disabled startup task.');
  end;
  CompletedInstall := True;
  RestartInstalledTray();
end;

procedure DeinitializeSetup();
begin
  if PreparedInstall and (not CompletedInstall) then
    RestoreFailedSetupState();
end;

function GetCustomSetupExitCode(): Integer;
begin
  Result := CustomExitCode;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  ResultCode: Integer;
  ScriptPath: String;
  ProtectedDir: String;
  Params: String;
  OwnerSid: String;
begin
  if CurUninstallStep <> usUninstall then Exit;
  RestartTray := NeuronTrayRunning();
  OwnerSid := ResolveCurrentUserSid();
  if OwnerSid = '' then
    RaiseException('Neuron could not identify the current Windows user. No files were removed.');
  if not StopInstalledTray() then
    RaiseException('Neuron could not stop its running tray process. No files were removed.');
  ProtectedDir := ExpandConstant('{commonpf64}\NeuronChromaBroker');
  if (not DirExists(ProtectedDir)) and (not ChromaTaskPresent()) and
     (not ChromaTransactionPresent()) then begin
    Log('No protected Chroma broker to remove');
    if not RemoveUserAutostartTasksAndVerify() then begin
      if (not RemoveAutostartTasksElevated()) or (InspectUserAutostartTasks() <> 0) then begin
        RestartInstalledTray();
        RaiseException('Neuron could not remove its startup task. No files were removed.');
      end;
    end;
    Exit;
  end;
  ScriptPath := ExpandConstant('{app}\uninstall-chroma-broker.ps1');
  if (not FileExists(ScriptPath)) or
     (CompareText(GetSHA256OfFile(ScriptPath), '{#BrokerUninstallerHash}') <> 0) then begin
    Log('Chroma broker cleanup script hash verification failed');
    RestartInstalledTray();
    RaiseException('Neuron could not verify its Chroma broker cleanup helper. No files were removed.');
  end;

  Params := '-NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "' +
    '$ErrorActionPreference=''Stop'';$p=' + PowerShellLiteral(ScriptPath) +
    ';$bytes=[IO.File]::ReadAllBytes($p);' +
    '$actual=[BitConverter]::ToString([Security.Cryptography.SHA256]::Create().ComputeHash($bytes)).Replace(''-'','''');' +
    'if ($actual -ine ''{#BrokerUninstallerHash}'') {exit 86};' +
    '$code=[Text.Encoding]::UTF8.GetString($bytes);&([ScriptBlock]::Create($code))' +
    ' -OwnerSid ' + PowerShellLiteral(OwnerSid) + '"';
  if not ShellExec('runas', ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'),
    Params, '', SW_HIDE, ewWaitUntilTerminated, ResultCode) then begin
    Log('Chroma broker cleanup elevation failed: ' + SysErrorMessage(ResultCode));
    RestartInstalledTray();
    RaiseException('Administrator approval is required to remove Neuron. No files were removed.');
  end;
  Log('Chroma broker cleanup exit code: ' + IntToStr(ResultCode));
  if ResultCode <> 0 then begin
    RestartInstalledTray();
    RaiseException('Neuron could not remove its protected Chroma broker. The per-user files were kept.');
  end;
  if InspectUserAutostartTasks() <> 0 then begin
    RestartInstalledTray();
    RaiseException('Neuron could not remove its startup task. The per-user files were kept.');
  end;
end;
