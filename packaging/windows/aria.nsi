; Aria 的 Windows 安装包脚本(NSIS 3.x)
;
; 特点:
;   - SetCompressor /SOLID lzma:整包固实压缩,单文件安装包体积接近 7z 极限压缩;
;   - 按用户安装($LOCALAPPDATA\Programs\Aria),不需要管理员权限;
;   - 自带卸载项、开始菜单/桌面快捷方式,卸载时清掉「开机自启」的 Run 值。
;
; 不要直接调用 makensis,用 packaging\windows\build-installer.ps1(会自动带版本号)。

Unicode true
SetCompressor /SOLID lzma
SetCompressorDictSize 32
SetDatablockOptimize on

; ---- 由构建脚本传入 ---------------------------------------------------------
!ifndef APPVERSION
  !define APPVERSION "0.0.0"
!endif
!ifndef ROOT
  !define ROOT "..\.."
!endif
!ifndef OUTDIR
  !define OUTDIR "${ROOT}\dist"
!endif

!define APP_NAME "Aria"
!define APP_EXE "aria.exe"
!define APP_PUBLISHER "Ohdmire"
!define APP_URL "https://github.com/Ohdmire/Aria"
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\Aria"
!define RUN_KEY "Software\Microsoft\Windows\CurrentVersion\Run"

Name "${APP_NAME}"
BrandingText "${APP_NAME} ${APPVERSION}"
OutFile "${OUTDIR}\aria-${APPVERSION}-win64-setup.exe"
InstallDir "$LOCALAPPDATA\Programs\Aria"
InstallDirRegKey HKCU "Software\Aria" "InstallDir"
RequestExecutionLevel user
ShowInstDetails show
ShowUninstDetails show

; 安装包自己的文件属性(资源管理器「详细信息」里看得到)
VIProductVersion "${APPVERSION}.0"
VIAddVersionKey /LANG=2052 "ProductName" "${APP_NAME}"
VIAddVersionKey /LANG=2052 "FileDescription" "${APP_NAME} 安装程序"
VIAddVersionKey /LANG=2052 "FileVersion" "${APPVERSION}"
VIAddVersionKey /LANG=2052 "ProductVersion" "${APPVERSION}"
VIAddVersionKey /LANG=2052 "CompanyName" "${APP_PUBLISHER}"
VIAddVersionKey /LANG=2052 "LegalCopyright" "MIT License"
VIAddVersionKey /LANG=2052 "Comments" "${APP_URL}"

; ---- 界面 -------------------------------------------------------------------
!include "MUI2.nsh"
!include "FileFunc.nsh"

!define MUI_ICON "${ROOT}\icons\icon.ico"
!define MUI_UNICON "${ROOT}\icons\icon.ico"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$INSTDIR\${APP_EXE}"
!define MUI_FINISHPAGE_RUN_TEXT "立即启动 ${APP_NAME}"
!define MUI_FINISHPAGE_LINK "${APP_URL}"
!define MUI_FINISHPAGE_LINK_LOCATION "${APP_URL}"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "SimpChinese"

; ---- 安装 -------------------------------------------------------------------
Section "Aria(必需)" SEC_MAIN
  SectionIn RO

  ; 覆盖安装前先结束正在运行的实例,否则 exe 被占用写不进去。
  ; 用 nsExec 而不是 ExecWait:后者会给控制台程序弹出黑窗。
  nsExec::Exec 'taskkill /F /IM ${APP_EXE}'
  Pop $0
  Sleep 300

  SetOutPath "$INSTDIR"
  File "${ROOT}\target\release\aria.exe"
  File "${ROOT}\README.md"

  WriteUninstaller "$INSTDIR\uninstall.exe"

  CreateDirectory "$SMPROGRAMS\${APP_NAME}"
  CreateShortCut "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}"
  CreateShortCut "$SMPROGRAMS\${APP_NAME}\卸载 ${APP_NAME}.lnk" "$INSTDIR\uninstall.exe"

  ; 卸载信息(控制面板 / 设置-应用 里能看到)
  WriteRegStr HKCU "Software\Aria" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayName" "${APP_NAME}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayVersion" "${APPVERSION}"
  WriteRegStr HKCU "${UNINST_KEY}" "Publisher" "${APP_PUBLISHER}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\${APP_EXE}"
  WriteRegStr HKCU "${UNINST_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "${UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegStr HKCU "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINST_KEY}" "URLInfoAbout" "${APP_URL}"
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoRepair" 1

  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKCU "${UNINST_KEY}" "EstimatedSize" "$0"
SectionEnd

Section "桌面快捷方式" SEC_DESKTOP
  CreateShortCut "$DESKTOP\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}"
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SEC_MAIN} "Aria 主程序(约 34 MB)和说明文档。"
  !insertmacro MUI_DESCRIPTION_TEXT ${SEC_DESKTOP} "在桌面创建一个快捷方式。"
!insertmacro MUI_FUNCTION_DESCRIPTION_END

; ---- 卸载 -------------------------------------------------------------------
Section "Uninstall"
  ; 先结束进程再删文件(nsExec 不弹黑窗)
  nsExec::Exec 'taskkill /F /IM ${APP_EXE}'
  Pop $0
  Sleep 300

  Delete "$INSTDIR\${APP_EXE}"
  Delete "$INSTDIR\README.md"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  Delete "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk"
  Delete "$SMPROGRAMS\${APP_NAME}\卸载 ${APP_NAME}.lnk"
  RMDir "$SMPROGRAMS\${APP_NAME}"
  Delete "$DESKTOP\${APP_NAME}.lnk"

  ; 开机自启(程序自己写的 HKCU\...\Run 值)和安装记录
  DeleteRegValue HKCU "${RUN_KEY}" "${APP_NAME}"
  DeleteRegKey HKCU "${UNINST_KEY}"
  DeleteRegKey HKCU "Software\Aria"

  ; 用户数据(曲库缓存/设置)在 %APPDATA%\com.ohdmire.aria,卸载不删
SectionEnd

Function .onInit
  ; 64 位系统照常安装到 32 位进程可见的 LOCALAPPDATA,无需重定向
  SetShellVarContext current
FunctionEnd

Function un.onInit
  SetShellVarContext current
FunctionEnd
