; Remote Input Bridge - Windows installer (Inno Setup 6.3 or newer).
;
; Per-user by design: everything lands in %LOCALAPPDATA%, so installing, updating and uninstalling
; never ask for administrator rights. That matters most for updates - the app replaces itself by
; running this installer silently, and a UAC prompt on every update would make the feature useless.
;
; Build it with scripts/build-windows.ps1, or by hand:
;   iscc /DAppVersion=0.2.0 installer\windows\rib-setup.iss

#ifndef AppVersion
  #define AppVersion "0.0.0-dev"
#endif
#ifndef SourceExe
  #define SourceExe "..\..\windows-sender\target\release\rib-sender.exe"
#endif

#define AppName "Remote Input Bridge"
#define ExeName "rib-sender.exe"
#define Publisher "Lince Studio"
#define RepoUrl "https://github.com/maketryuk/remote-Input-bridge"

[Setup]
; Never change AppId: it is what ties an upgrade to the installation it replaces, and what the
; entry in "Apps & features" is keyed on.
AppId={{BEF6E912-7FE8-404E-99D6-5A9FFCB74772}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#Publisher}
AppPublisherURL={#RepoUrl}
AppSupportURL={#RepoUrl}/issues
AppUpdatesURL={#RepoUrl}/releases
VersionInfoVersion={#AppVersion}
DefaultDirName={localappdata}\Programs\RemoteInputBridge
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
DisableDirPage=auto
UninstallDisplayName={#AppName}
UninstallDisplayIcon={app}\{#ExeName}
OutputDir=..\..\dist
OutputBaseFilename=RemoteInputBridge-Setup-{#AppVersion}
SetupIconFile=..\..\windows-sender\resources\app.ico
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes
; No elevation, ever. The bridge needs no privileges; a user who wants input suppression to reach
; elevated windows runs the app itself as administrator, which is a separate decision.
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Restart Manager closes whatever still holds the executable, which is what makes a silent
; self-update work: the app spawns this installer and exits, and a file that has not been let go of
; yet is dealt with here instead of failing the install.
CloseApplications=yes
; It must not put the app back on its own, though - the self-update already relaunches it through
; /RIBRESTART, and both firing would start two copies at once.
RestartApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
; Checked by default: a bridge that has to be started by hand before it can hand over the keyboard
; defeats the point of it.
Name: "startup"; Description: "Start {#AppName} when I sign in to Windows"; GroupDescription: "Startup:"
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Shortcuts:"; Flags: unchecked

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; DestName: "{#ExeName}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\README.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#ExeName}"
Name: "{group}\{#AppName} (console diagnostics)"; Filename: "{app}\{#ExeName}"; Parameters: "--diagnostics --show"; Comment: "Run with a console window and the per-second diagnostics line"
Name: "{userdesktop}\{#AppName}"; Filename: "{app}\{#ExeName}"; Tasks: desktopicon

[Registry]
; The same value the app's own "Start with Windows" checkbox writes, so the two can never disagree.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "RemoteInputBridge"; ValueData: """{app}\{#ExeName}"""; Tasks: startup; Flags: uninsdeletevalue

[Run]
Filename: "{app}\{#ExeName}"; Description: "Start {#AppName} now"; Flags: nowait postinstall skipifsilent
; A silent install runs nothing by default, which for a self-update would mean the bridge stops
; when it updates. The app passes /RIBRESTART=1 to say "you interrupted me, put me back".
Filename: "{app}\{#ExeName}"; Flags: nowait runasoriginaluser; Check: WantsRestart

[Code]
function WantsRestart: Boolean;
begin
  Result := ExpandConstant('{param:RIBRESTART|0}') = '1';
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  ConfigDir: String;
begin
  if CurUninstallStep <> usUninstall then
    Exit;

  // The [Registry] entry above is only removed when the startup task was chosen at install time.
  // Enabling autostart later from inside the app writes the same value, and leaving it behind
  // would make Windows try to launch a program that is no longer there.
  RegDeleteValue(HKEY_CURRENT_USER, 'Software\Microsoft\Windows\CurrentVersion\Run',
    'RemoteInputBridge');

  // Settings and the paired-Mac keys are user data, so they survive an uninstall unless asked
  // about. Silent uninstalls keep them: nobody is there to answer.
  ConfigDir := ExpandConstant('{userappdata}\RemoteInputBridge');
  if DirExists(ConfigDir) and not UninstallSilent then
    if MsgBox('Also delete the settings and the keys of paired Macs?' + #13#10#13#10 + ConfigDir,
              mbConfirmation, MB_YESNO) = IDYES then
      DelTree(ConfigDir, True, True, True);
end;
