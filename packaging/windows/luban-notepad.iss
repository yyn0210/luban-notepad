; Inno Setup 6 script for the Luban Notepad installer.
;
; Build it through scripts/package-windows.ps1, which passes the required
; defines and stages the files first:
;   /DAppVersion=<version>
;   /DSourceDir=<staging directory with the exe and docs>
;   /DOutputDir=<dist directory>
;   /DArch=<x86_64|aarch64>
; Optional:
;   /DChineseLanguage=1 /DChineseMessagesFile=compiler:Languages\...

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceDir
  #define SourceDir "..\..\dist\.installer-stage"
#endif
#ifndef OutputDir
  #define OutputDir "..\..\dist"
#endif
#ifndef Arch
  #define Arch "x86_64"
#endif
#ifndef Arm64
  #define Arm64 0
#endif
#ifndef IconFile
  #define IconFile "luban-notepad.ico"
#endif
#ifndef ChineseLanguage
  #define ChineseLanguage 0
#endif

#define MyAppName "Luban Notepad"
#define MyAppExeName "luban-notepad.exe"
#define MyAppPublisher "Luban Notepad Contributors"
#define MyAppURL "https://github.com/luban-notepad/luban-notepad"

[Setup]
; Keep this AppId stable: it is how Windows recognizes upgrades of the same
; product. Changing it makes the new installer install side by side instead.
AppId={{8C41A5F7-8C3B-4F4F-9C62-3A1C9B6D2E71}
AppName={#MyAppName}
AppVersion={#AppVersion}
AppVerName={#MyAppName} {#AppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
AppUpdatesURL={#MyAppURL}
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
LicenseFile={#SourceDir}\LICENSE.utf8.txt
OutputDir={#OutputDir}
OutputBaseFilename=luban-notepad-{#AppVersion}-windows-{#Arch}-setup
SetupIconFile={#IconFile}
UninstallDisplayIcon={app}\{#MyAppExeName}
UninstallDisplayName={#MyAppName}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
; Default to a per-user install so no administrator rights are required, but
; let the user pick an all-users install in the privileges dialog.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
CloseApplications=yes
RestartApplications=no
MinVersion=10.0
SetupLogging=yes

#if Arm64
ArchitecturesAllowed=arm64
ArchitecturesInstallIn64BitMode=arm64
#else
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
#endif

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
#if ChineseLanguage
Name: "chinesesimplified"; MessagesFile: "{#ChineseMessagesFile}"
#endif

[CustomMessages]
english.ContextMenu=Add "Open with Luban Notepad" to the context menu
english.DeleteSettings=Delete Luban Notepad settings, session, and recent file history?
#if ChineseLanguage
chinesesimplified.ContextMenu=将“使用鲁班记事本打开”添加到右键菜单
chinesesimplified.DeleteSettings=是否删除鲁班记事本的设置、会话和最近文件记录？
#endif

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "contextmenu"; Description: "{cm:ContextMenu}"; Flags: unchecked

[Files]
Source: "{#SourceDir}\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\CHANGELOG.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\THIRD_PARTY_LICENSES.md"; DestDir: "{app}"; Flags: ignoreversion

[InstallDelete]
; Drop a stale portable marker if someone extracts the portable build over the
; install directory; installed copies must keep using %APPDATA%.
Type: files; Name: "{app}\luban-notepad.portable"

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{group}\{cm:UninstallProgram,{#MyAppName}}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Registry]
; Allow starting the app from the Run dialog and other launchers.
Root: HKA; Subkey: "Software\Microsoft\Windows\CurrentVersion\App Paths\{#MyAppExeName}"; ValueType: string; ValueName: ""; ValueData: "{app}\{#MyAppExeName}"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Microsoft\Windows\CurrentVersion\App Paths\{#MyAppExeName}"; ValueType: string; ValueName: "Path"; ValueData: "{app}"; Flags: uninsdeletekey
; Optional Explorer integration; uninsdeletekey removes the command subkey too.
Root: HKA; Subkey: "Software\Classes\*\shell\LubanNotepad"; ValueType: string; ValueName: ""; ValueData: "Open with Luban Notepad"; Flags: uninsdeletekey; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\*\shell\LubanNotepad"; ValueType: string; ValueName: "Icon"; ValueData: "{app}\{#MyAppExeName},0"; Tasks: contextmenu
Root: HKA; Subkey: "Software\Classes\*\shell\LubanNotepad\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExeName}"" ""%1"""; Tasks: contextmenu

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#StringChange(MyAppName, '&', '&&')}}"; Flags: nowait postinstall skipifsilent

[Code]
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  ConfigDir: String;
begin
  if CurUninstallStep = usPostUninstall then
  begin
    ConfigDir := ExpandConstant('{userappdata}\luban-notepad');
    if DirExists(ConfigDir) then
      if MsgBox(ExpandConstant('{cm:DeleteSettings}') + #13#10 + #13#10 + ConfigDir,
        mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES then
        DelTree(ConfigDir, True, True, True);
  end;
end;
