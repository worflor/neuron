; SPDX-FileCopyrightText: 2026 Woflo Labs
; SPDX-License-Identifier: GPL-3.0-or-later
; Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

#ifndef AppVersion
  #error AppVersion must be supplied with /DAppVersion=...
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
OutputBaseFilename=neuron-v{#AppVersion}-windows-x86_64-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\neuron-app.exe

[Files]
Source: "{#StageDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\Neuron"; Filename: "{app}\neuron-app.exe"; WorkingDir: "{app}"
