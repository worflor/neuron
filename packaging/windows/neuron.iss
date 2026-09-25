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

[Setup]
AppId={{9E6F3A1F-68A7-4314-B05F-902A8EDABF14}
AppName=Neuron
AppVersion={#AppVersion}
AppPublisher=Woflo Labs
AppPublisherURL=https://github.com/worflor/neuron
DefaultDirName={localappdata}\Programs\Neuron
DefaultGroupName=Neuron
PrivilegesRequired=lowest
OutputBaseFilename=neuron-{#PackageLabel}-windows-x86_64-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\neuron-app.exe

[Files]
Source: "{#StageDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\Neuron"; Filename: "{app}\neuron-app.exe"; WorkingDir: "{app}"

[Code]
function InitializeSetup(): Boolean;
var
  ResultCode: Integer;
  Params: String;
begin
  { The previous release could leave a HighestAvailable task pointing at this user-writable app.
    Check before setup replaces any files; a limited installer cannot safely repair that task. }
  Params := '-NoProfile -NonInteractive -Command "try {$t=Get-ScheduledTask -TaskName ''Neuron (elevated tray)'' -ErrorAction Stop} catch {if ($_.CategoryInfo.Category -eq ''ObjectNotFound'') {exit 0}; exit 2}; if ($t.Principal.RunLevel -eq ''Limited'') {exit 0}; exit 3"';
  if not Exec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'),
    Params, '', SW_HIDE, ewWaitUntilTerminated, ResultCode) then begin
    MsgBox('Neuron setup could not verify the startup task. No files were changed.', mbCriticalError, MB_OK);
    Result := False;
    Exit;
  end;
  if ResultCode = 0 then begin
    Result := True;
    Exit;
  end;
  if ResultCode = 3 then
    MsgBox('Neuron setup found an old elevated startup task. Remove or replace it from an administrator PowerShell, then run setup again. The tray app must run at Limited privilege.', mbCriticalError, MB_OK)
  else
    MsgBox('Neuron setup could not verify the startup task. No files were changed.', mbCriticalError, MB_OK);
  Result := False;
end;
