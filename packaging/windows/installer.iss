; Inno Setup script for the Windows installer.
;   cargo build --release
;   iscc /DAppVersion=0.1.0 packaging\windows\installer.iss
; The installer goes to dist\.

#ifndef AppVersion
  #error Pass the version: iscc /DAppVersion=x.y.z installer.iss
#endif

[Setup]
AppId={{3B0499C5-99F9-483F-AA50-3CB6DB89FECD}
AppName=PatraPDF
AppVersion={#AppVersion}
AppVerName=PatraPDF {#AppVersion}
AppPublisher=Prashant Gupta
AppPublisherURL=https://github.com/guptap504/patrapdf
AppSupportURL=https://github.com/guptap504/patrapdf/issues
AppUpdatesURL=https://github.com/guptap504/patrapdf/releases
VersionInfoVersion={#AppVersion}
DefaultDirName={autopf}\PatraPDF
DefaultGroupName=PatraPDF
DisableProgramGroupPage=yes
LicenseFile=..\..\LICENSE
OutputDir=..\..\dist
OutputBaseFilename=PatraPDF-{#AppVersion}-windows-x64-setup
SetupIconFile=app.ico
UninstallDisplayIcon={app}\patrapdf.exe
UninstallDisplayName=PatraPDF
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Install for the current user without admin rights, or for everyone.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ChangesAssociations=yes

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "..\..\target\release\patrapdf.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\PatraPDF"; Filename: "{app}\patrapdf.exe"
Name: "{autodesktop}\PatraPDF"; Filename: "{app}\patrapdf.exe"; Tasks: desktopicon

[Registry]
; Offer PatraPDF under "Open with" for PDFs (without taking over the default viewer).
Root: HKA; Subkey: "Software\Classes\PatraPDF.pdf"; ValueType: string; ValueData: "PDF Document"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\PatraPDF.pdf\DefaultIcon"; ValueType: string; ValueData: "{app}\patrapdf.exe,0"
Root: HKA; Subkey: "Software\Classes\PatraPDF.pdf\shell\open\command"; ValueType: string; ValueData: """{app}\patrapdf.exe"" ""%1"""
Root: HKA; Subkey: "Software\Classes\.pdf\OpenWithProgids"; ValueType: string; ValueName: "PatraPDF.pdf"; ValueData: ""; Flags: uninsdeletevalue
Root: HKA; Subkey: "Software\Classes\Applications\patrapdf.exe"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "PatraPDF"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Applications\patrapdf.exe\SupportedTypes"; ValueType: string; ValueName: ".pdf"; ValueData: ""
Root: HKA; Subkey: "Software\Classes\Applications\patrapdf.exe\shell\open\command"; ValueType: string; ValueData: """{app}\patrapdf.exe"" ""%1"""

[Run]
Filename: "{app}\patrapdf.exe"; Description: "{cm:LaunchProgram,PatraPDF}"; Flags: nowait postinstall skipifsilent
; The in-app updater runs this installer silently with /RELAUNCH: start the new version.
Filename: "{app}\patrapdf.exe"; Flags: nowait runasoriginaluser; Check: ShouldRelaunch

[Code]
function ShouldRelaunch: Boolean;
var
  I: Integer;
begin
  Result := False;
  for I := 1 to ParamCount do
    if CompareText(ParamStr(I), '/RELAUNCH') = 0 then
      Result := True;
end;
