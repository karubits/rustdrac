; Inno Setup script for Rust Drac. Built by scripts/build-windows.sh, which passes:
;   AppVersion      e.g. 0.4.0 (or 0.4.0-rc1)
;   NumericVersion  e.g. 0.4.0.0 (for the version resource)
;   AppDir          the staged bundle (rustdrac.exe, DLLs, etc/, lib/, share/)
;
; Per-user install (no administrator rights), silent install for winget
; (InstallerType "inno" uses /VERYSILENT). Keep AppId, AppName and AppPublisher in step
; with packaging/winget/*.installer.yaml: winget matches the Apps & Features entry on them.

#ifndef AppVersion
  #error AppVersion is required
#endif
#ifndef NumericVersion
  #define NumericVersion "0.0.0.0"
#endif
#ifndef AppDir
  #error AppDir is required
#endif

#define AppId "{B7C0E0C2-2F5E-4D3B-9F6A-1E2D7C9A4F10}"
#define WebView2Key "SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}"

[Setup]
AppId={{#AppId}
AppName=Rust Drac
AppVersion={#AppVersion}
AppVerName=Rust Drac {#AppVersion}
AppPublisher=karubits
AppPublisherURL=https://github.com/karubits/rustdrac
AppSupportURL=https://github.com/karubits/rustdrac/issues
AppUpdatesURL=https://github.com/karubits/rustdrac/releases
VersionInfoVersion={#NumericVersion}
VersionInfoDescription=Rust Drac installer
DefaultDirName={localappdata}\Programs\Rust Drac
DisableProgramGroupPage=yes
DisableDirPage=auto
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.17763
LicenseFile=..\..\LICENSE
SetupIconFile=rustdrac.ico
UninstallDisplayIcon={app}\rustdrac.exe
UninstallDisplayName=Rust Drac
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#AppDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
; The AppUserModelID groups the taskbar button with this shortcut and lets the app send
; toast notifications (src/win.rs uses the same id).
Name: "{autoprograms}\Rust Drac"; Filename: "{app}\rustdrac.exe"; AppUserModelID: "Karubits.RustDrac"
Name: "{autodesktop}\Rust Drac"; Filename: "{app}\rustdrac.exe"; AppUserModelID: "Karubits.RustDrac"; Tasks: desktopicon

[Run]
Filename: "{tmp}\MicrosoftEdgeWebView2Setup.exe"; Parameters: "/silent /install"; StatusMsg: "Installing the Microsoft Edge WebView2 Runtime..."; Check: NeedWebView2; Flags: waituntilterminated
Filename: "{app}\rustdrac.exe"; Description: "{cm:LaunchProgram,Rust Drac}"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; The browser profile WebView2 keeps for us (cache, nothing of value).
Type: filesandordirs; Name: "{localappdata}\rustdrac\WebView2"

[Code]
// The console needs the Evergreen WebView2 Runtime. Windows 11 and up-to-date Windows 10
// have it; otherwise the bootstrapper is downloaded at install time (only then).
function WebView2Installed: Boolean;
var
  Version: String;
begin
  Result := (RegQueryStringValue(HKLM, 'SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}', 'pv', Version)
             or RegQueryStringValue(HKCU, '{#WebView2Key}', 'pv', Version))
            and (Version <> '') and (Version <> '0.0.0.0');
end;

var
  WebView2Downloaded: Boolean;

function NeedWebView2: Boolean;
begin
  Result := WebView2Downloaded;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  WebView2Downloaded := False;
  if WebView2Installed then
    exit;
  try
    DownloadTemporaryFile('https://go.microsoft.com/fwlink/p/?LinkId=2124703', 'MicrosoftEdgeWebView2Setup.exe', '', nil);
    WebView2Downloaded := True;
  except
    Log('WebView2 bootstrapper download failed: ' + GetExceptionMessage);
    // Not fatal: the app runs without it (the console tab explains what to install).
  end;
end;
