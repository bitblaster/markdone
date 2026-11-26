; Inno Setup script for the Markdone Windows installer.
;
;   ISCC.exe /DAppVersion=0.1.0 /DSourceExe=<path to markdone.exe> /O<output dir> ^
;            /F<output name> packaging\windows\markdone.iss
;
; Installs per user by default (no administrator rights), with the option to install for
; all users; adds a Start menu entry, optionally a desktop icon and the association of
; Markdown files. Relative paths are relative to this file.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceExe
  #define SourceExe "..\..\target\release\markdone.exe"
#endif

[Setup]
; Identifies the application to Windows across versions (upgrades, uninstall): never change.
AppId={{2967A7B3-51DF-4696-8644-1230F69DE0C6}
AppName=Markdone
AppVersion={#AppVersion}
AppVerName=Markdone {#AppVersion}
; The installer's own version information (code signing enforces the product name and
; version, see .signpath/).
VersionInfoVersion={#AppVersion}
VersionInfoProductName=Markdone
VersionInfoProductVersion={#AppVersion}
VersionInfoProductTextVersion={#AppVersion}
VersionInfoDescription=Markdone Setup
AppPublisher=The Markdone contributors
AppPublisherURL=https://github.com/bitblaster/markdone
AppSupportURL=https://github.com/bitblaster/markdone/issues
AppUpdatesURL=https://github.com/bitblaster/markdone/releases
DefaultDirName={autopf}\Markdone
DefaultGroupName=Markdone
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
LicenseFile=..\..\LICENSE
SetupIconFile=markdone.ico
UninstallDisplayIcon={app}\markdone.exe
UninstallDisplayName=Markdone
ChangesAssociations=yes
WizardStyle=modern
Compression=lzma2
SolidCompression=yes
OutputBaseFilename=markdone-setup

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "italian"; MessagesFile: "compiler:Languages\Italian.isl"

[CustomMessages]
english.AssocTask=Open Markdown files (.md, .markdown) with Markdone
italian.AssocTask=Apri i file Markdown (.md, .markdown) con Markdone
english.AssocGroup=File associations:
italian.AssocGroup=Associazioni dei file:
english.MarkdownDocument=Markdown document
italian.MarkdownDocument=Documento Markdown

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "associate"; Description: "{cm:AssocTask}"; GroupDescription: "{cm:AssocGroup}"

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; DestName: "markdone.exe"; Flags: ignoreversion
Source: "..\..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Markdone"; Filename: "{app}\markdone.exe"
Name: "{autodesktop}\Markdone"; Filename: "{app}\markdone.exe"; Tasks: desktopicon

[Registry]
; "Open with" offers Markdone for Markdown files in any case; with the task, Markdone is
; registered as a handler (Windows still lets the user pick the default).
Root: HKA; Subkey: "Software\Classes\Applications\markdone.exe"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "Markdone"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Applications\markdone.exe\SupportedTypes"; ValueType: string; ValueName: ".md"; ValueData: ""
Root: HKA; Subkey: "Software\Classes\Applications\markdone.exe\SupportedTypes"; ValueType: string; ValueName: ".markdown"; ValueData: ""
Root: HKA; Subkey: "Software\Classes\Applications\markdone.exe\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\markdone.exe"" ""%1"""
Root: HKA; Subkey: "Software\Classes\Markdone.Markdown"; ValueType: string; ValueName: ""; ValueData: "{cm:MarkdownDocument}"; Flags: uninsdeletekey; Tasks: associate
Root: HKA; Subkey: "Software\Classes\Markdone.Markdown\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\markdone.exe,0"; Tasks: associate
Root: HKA; Subkey: "Software\Classes\Markdone.Markdown\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\markdone.exe"" ""%1"""; Tasks: associate
Root: HKA; Subkey: "Software\Classes\.md\OpenWithProgids"; ValueType: string; ValueName: "Markdone.Markdown"; ValueData: ""; Flags: uninsdeletevalue; Tasks: associate
Root: HKA; Subkey: "Software\Classes\.markdown\OpenWithProgids"; ValueType: string; ValueName: "Markdone.Markdown"; ValueData: ""; Flags: uninsdeletevalue; Tasks: associate

[Run]
Filename: "{app}\markdone.exe"; Description: "{cm:LaunchProgram,Markdone}"; Flags: nowait postinstall skipifsilent
