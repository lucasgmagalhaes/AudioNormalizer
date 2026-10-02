#ifndef MyAppVersion
  #define MyAppVersion "0.1.0"
#endif

[Setup]
AppId={{A47551B7-97A4-4203-A26A-6F56A56B0D34}
AppName=Audio Normalizer
AppVersion={#MyAppVersion}
AppPublisher=Lucas Gomes
DefaultDirName={autopf}\Audio Normalizer
DefaultGroupName=Audio Normalizer
DisableProgramGroupPage=yes
OutputDir=..\backend\target\release\bundle\inno
OutputBaseFilename=AudioNormalizer-{#MyAppVersion}-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ArchitecturesInstallIn64BitMode=x64compatible

[Files]
Source: "..\backend\target\release\audio-normalizer.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\backend\runtime\*.dll"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Audio Normalizer"; Filename: "{app}\audio-normalizer.exe"
Name: "{autodesktop}\Audio Normalizer"; Filename: "{app}\audio-normalizer.exe"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Additional shortcuts:"

[Run]
Filename: "{app}\audio-normalizer.exe"; Description: "Launch Audio Normalizer"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
Type: filesandordirs; Name: "{app}"
