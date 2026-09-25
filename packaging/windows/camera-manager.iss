; Instalador do Camera Manager para Windows (Inno Setup 6).
; Chamado por tools/package-windows.sh com:
;   /DAppVersion=0.1.0 /DSourceDir=<pasta empacotada> /DOutputDir=<destino>

#define AppName "Camera Manager"
#define AppExe  "camera-manager.exe"

[Setup]
; GUID fixo: identifica o app para atualizar/desinstalar. NÃO mude entre versões.
AppId={{6E1F0C1A-4B7D-4E0B-9C47-2D3A5B8F7A10}
AppName={#AppName}
AppVersion={#AppVersion}
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
UninstallDisplayIcon={app}\{#AppExe}
OutputDir={#OutputDir}
OutputBaseFilename=camera-manager-{#AppVersion}-windows-x64-setup
Compression=lzma2
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Instala só para o usuário atual, sem exigir administrador (o usuário pode escolher
; "todos os usuários" na primeira tela).
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
WizardStyle=modern

[Languages]
Name: "brazilianportuguese"; MessagesFile: "compiler:Languages\BrazilianPortuguese.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: recursesubdirs createallsubdirs ignoreversion

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{group}\{cm:UninstallProgram,{#AppName}}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExe}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent

; Os dados do usuário (câmeras, layout) ficam em %APPDATA%\camera-manager e NÃO são
; removidos ao desinstalar, de propósito.
