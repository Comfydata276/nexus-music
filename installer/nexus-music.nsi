; Windows installer for Nexus Music. Per-user: no administrator prompt.
; Build with scripts/package-windows, which passes -DVERSION=x.y.z.

Unicode true
!include "MUI2.nsh"

!define APP_NAME "Nexus Music"
!define APP_EXE "NexusMusic.exe"
!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\NexusMusic"

Name "${APP_NAME}"
OutFile "../dist/NexusMusic-Setup-${VERSION}.exe"
InstallDir "$LOCALAPPDATA\Programs\${APP_NAME}"
InstallDirRegKey HKCU "Software\${APP_NAME}" "InstallDir"
RequestExecutionLevel user
SetCompressor /SOLID lzma

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${APP_NAME}"
VIAddVersionKey "FileDescription" "${APP_NAME} Setup"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" ""

!define MUI_ICON "../assets/app-icon.ico"
!define MUI_UNICON "../assets/app-icon.ico"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$INSTDIR\${APP_EXE}"
!define MUI_FINISHPAGE_RUN_TEXT "Open ${APP_NAME}"

!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Section "Install"
  ; An upgrade can't replace the program while it's running.
  nsExec::Exec 'taskkill /IM "${APP_EXE}" /F'
  Sleep 400

  SetOutPath "$INSTDIR"
  File "/oname=${APP_EXE}" "../target/x86_64-pc-windows-msvc/release/nexus-music.exe"
  WriteUninstaller "$INSTDIR\Uninstall.exe"

  CreateShortcut "$SMPROGRAMS\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}"
  CreateShortcut "$DESKTOP\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}"

  WriteRegStr HKCU "Software\${APP_NAME}" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayName" "${APP_NAME}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\${APP_EXE}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoRepair" 1

  ; The app links against the Visual C++ runtime. This installer is a 32-bit program, so
  ; "Sysnative" is how it sees the real 64-bit System32.
  IfFileExists "$WINDIR\Sysnative\vcruntime140.dll" 0 runtime_missing
  IfFileExists "$WINDIR\Sysnative\msvcp140.dll" runtime_ok runtime_missing
  runtime_missing:
    MessageBox MB_YESNO|MB_ICONINFORMATION "${APP_NAME} needs the Microsoft Visual C++ runtime, which isn't on this PC yet.$\n$\nDownload it from Microsoft now? Run the file it gives you, then open ${APP_NAME}." /SD IDNO IDNO runtime_ok
    ExecShell "open" "https://aka.ms/vs/17/release/vc_redist.x64.exe"
  runtime_ok:
SectionEnd

Section "Uninstall"
  nsExec::Exec 'taskkill /IM "${APP_EXE}" /F'
  Sleep 400

  Delete "$INSTDIR\${APP_EXE}"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\${APP_NAME}.lnk"
  Delete "$DESKTOP\${APP_NAME}.lnk"
  DeleteRegKey HKCU "${UNINSTALL_KEY}"
  DeleteRegKey HKCU "Software\${APP_NAME}"
  ; Settings and the artwork cache under %APPDATA% and %LOCALAPPDATA% are left in place,
  ; and the saved password stays in Windows Credential Manager until removed there.
SectionEnd
