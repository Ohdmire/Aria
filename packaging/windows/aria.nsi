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
VIAddVersionKey /LANG=1033 "ProductName" "${APP_NAME}"
VIAddVersionKey /LANG=1033 "FileDescription" "${APP_NAME} Setup"
VIAddVersionKey /LANG=1033 "FileVersion" "${APPVERSION}"
VIAddVersionKey /LANG=1033 "ProductVersion" "${APPVERSION}"
VIAddVersionKey /LANG=1033 "CompanyName" "${APP_PUBLISHER}"
VIAddVersionKey /LANG=1033 "LegalCopyright" "MIT License"
VIAddVersionKey /LANG=1033 "Comments" "${APP_URL}"
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
!include "WinMessages.nsh"

!define MUI_ICON "${ROOT}\icons\icon.ico"
!define MUI_UNICON "${ROOT}\icons\icon.ico"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$INSTDIR\${APP_EXE}"
!define MUI_FINISHPAGE_RUN_TEXT "Launch ${APP_NAME}"
!define MUI_FINISHPAGE_LINK "${APP_URL}"
!define MUI_FINISHPAGE_LINK_LOCATION "${APP_URL}"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!define MUI_PAGE_CUSTOMFUNCTION_SHOW FinishShow
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

; 第一种是对不上系统语言时的回落。对话框自己会按系统 UI 语言预选。
!insertmacro MUI_LANGUAGE "English"
!insertmacro MUI_LANGUAGE "SimpChinese"

!insertmacro MUI_RESERVEFILE_LANGDLL

LangString SEC_MAIN ${LANG_ENGLISH} "Aria (required)"
LangString SEC_MAIN ${LANG_SIMPCHINESE} "Aria(必需)"
LangString SEC_DESKTOP ${LANG_ENGLISH} "Desktop shortcut"
LangString SEC_DESKTOP ${LANG_SIMPCHINESE} "桌面快捷方式"
LangString DESC_MAIN ${LANG_ENGLISH} "The Aria program (about 34 MB) and the readme."
LangString DESC_MAIN ${LANG_SIMPCHINESE} "Aria 主程序(约 34 MB)和说明文档。"
LangString DESC_DESKTOP ${LANG_ENGLISH} "Create a shortcut on the desktop."
LangString DESC_DESKTOP ${LANG_SIMPCHINESE} "在桌面创建一个快捷方式。"
LangString UNINST_NAME ${LANG_ENGLISH} "Uninstall ${APP_NAME}"
LangString UNINST_NAME ${LANG_SIMPCHINESE} "卸载 ${APP_NAME}"

; ---- 安装 -------------------------------------------------------------------
Section "$(SEC_MAIN)" SEC_MAIN
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
  CreateShortCut "$SMPROGRAMS\${APP_NAME}\$(UNINST_NAME).lnk" "$INSTDIR\uninstall.exe"

  Call WriteInstallLanguage

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

Section "$(SEC_DESKTOP)" SEC_DESKTOP
  CreateShortCut "$DESKTOP\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}"
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SEC_MAIN} "$(DESC_MAIN)"
  !insertmacro MUI_DESCRIPTION_TEXT ${SEC_DESKTOP} "$(DESC_DESKTOP)"
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
  Delete "$SMPROGRAMS\${APP_NAME}\Uninstall ${APP_NAME}.lnk"
  Delete "$SMPROGRAMS\${APP_NAME}\卸载 ${APP_NAME}.lnk"
  RMDir "$SMPROGRAMS\${APP_NAME}"
  Delete "$DESKTOP\${APP_NAME}.lnk"
  Delete "$APPDATA\com.ohdmire.aria\install-language"

  ; 开机自启(程序自己写的 HKCU\...\Run 值)和安装记录
  DeleteRegValue HKCU "${RUN_KEY}" "${APP_NAME}"
  DeleteRegKey HKCU "${UNINST_KEY}"
  DeleteRegKey HKCU "Software\Aria"

  ; 用户数据(曲库缓存/设置)在 %APPDATA%\com.ohdmire.aria,卸载不删
SectionEnd

; 把安装时选的语言写成 ASCII 文件。Unicode NSIS 的 FileWrite 会写成 UTF-16,
; 程序认 UTF-8 / UTF-16,但字节写最稳。
Function WriteInstallLanguage
  CreateDirectory "$APPDATA\com.ohdmire.aria"
  FileOpen $R1 "$APPDATA\com.ohdmire.aria\install-language" w
  ${If} $LANGUAGE = ${LANG_ENGLISH}
    FileWriteByte $R1 101 ; e
    FileWriteByte $R1 110 ; n
  ${Else}
    FileWriteByte $R1 122 ; z
    FileWriteByte $R1 104 ; h
    FileWriteByte $R1 45  ; -
    FileWriteByte $R1 67  ; C
    FileWriteByte $R1 78  ; N
  ${EndIf}
  FileClose $R1
FunctionEnd

Function FinishShow
  ${If} $LANGUAGE = ${LANG_ENGLISH}
    SendMessage $mui.FinishPage.Run ${WM_SETTEXT} 0 "STR:Launch ${APP_NAME}"
  ${Else}
    SendMessage $mui.FinishPage.Run ${WM_SETTEXT} 0 "STR:立即启动 ${APP_NAME}"
  ${EndIf}
FunctionEnd

; 弹出语言列表。LangDLL 按系统 UI 语言预选;对不上时回落到第一种(English)。
Function SelectLanguage
  ${IfNot} ${Silent}
    LangDLL::LangDialog "Language / 语言" "请选择语言。$\r$\nSelect a language." A "English" ${LANG_ENGLISH} "简体中文" ${LANG_SIMPCHINESE} ""
    Pop $LANGUAGE
    StrCmp $LANGUAGE "cancel" 0 +2
      Abort
  ${EndIf}
FunctionEnd

Function .onInit
  ; 64 位系统照常安装到 32 位进程可见的 LOCALAPPDATA,无需重定向
  SetShellVarContext current
  Call SelectLanguage
FunctionEnd

Function un.onInit
  SetShellVarContext current
  Call un.SelectLanguage
FunctionEnd

Function un.SelectLanguage
  ${IfNot} ${Silent}
    LangDLL::LangDialog "Language / 语言" "请选择语言。$\r$\nSelect a language." A "English" ${LANG_ENGLISH} "简体中文" ${LANG_SIMPCHINESE} ""
    Pop $LANGUAGE
    StrCmp $LANGUAGE "cancel" 0 +2
      Abort
  ${EndIf}
FunctionEnd
