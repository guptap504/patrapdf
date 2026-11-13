; Inno Setup script for the Windows installer.
;   cargo build --release
;   iscc /DAppVersion=0.1.0 packaging\windows\installer.iss
; The installer goes to dist\.

#ifndef AppVersion
  #error Pass the version: iscc /DAppVersion=x.y.z installer.iss
#endif

[Setup]
AppId={{3B0499C5-99F9-483F-AA50-3CB6DB89FECD}
AppName=PDF Signer
AppVersion={#AppVersion}
AppVerName=PDF Signer {#AppVersion}
AppPublisher=Prashant Gupta
AppPublisherURL=https://github.com/guptap504/pdf-signer
AppSupportURL=https://github.com/guptap504/pdf-signer/issues
AppUpdatesURL=https://github.com/guptap504/pdf-signer/releases
VersionInfoVersion={#AppVersion}
DefaultDirName={autopf}\PDF Signer
DefaultGroupName=PDF Signer
DisableProgramGroupPage=yes
LicenseFile=..\..\LICENSE
OutputDir=..\..\dist
OutputBaseFilename=PDF-Signer-{#AppVersion}-windows-x64-setup
SetupIconFile=app.ico
UninstallDisplayIcon={app}\pdf-signer.exe
UninstallDisplayName=PDF Signer
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
Source: "..\..\target\release\pdf-signer.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\PDF Signer"; Filename: "{app}\pdf-signer.exe"
Name: "{autodesktop}\PDF Signer"; Filename: "{app}\pdf-signer.exe"; Tasks: desktopicon

[Registry]
; Offer PDF Signer under "Open with" for PDFs (without taking over the default viewer).
Root: HKA; Subkey: "Software\Classes\PdfSigner.pdf"; ValueType: string; ValueData: "PDF Document"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\PdfSigner.pdf\DefaultIcon"; ValueType: string; ValueData: "{app}\pdf-signer.exe,0"
Root: HKA; Subkey: "Software\Classes\PdfSigner.pdf\shell\open\command"; ValueType: string; ValueData: """{app}\pdf-signer.exe"" ""%1"""
Root: HKA; Subkey: "Software\Classes\.pdf\OpenWithProgids"; ValueType: string; ValueName: "PdfSigner.pdf"; ValueData: ""; Flags: uninsdeletevalue
Root: HKA; Subkey: "Software\Classes\Applications\pdf-signer.exe"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "PDF Signer"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Applications\pdf-signer.exe\SupportedTypes"; ValueType: string; ValueName: ".pdf"; ValueData: ""
Root: HKA; Subkey: "Software\Classes\Applications\pdf-signer.exe\shell\open\command"; ValueType: string; ValueData: """{app}\pdf-signer.exe"" ""%1"""

[Run]
Filename: "{app}\pdf-signer.exe"; Description: "{cm:LaunchProgram,PDF Signer}"; Flags: nowait postinstall skipifsilent
; The in-app updater runs this installer silently with /RELAUNCH: start the new version.
Filename: "{app}\pdf-signer.exe"; Flags: nowait runasoriginaluser; Check: ShouldRelaunch

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
