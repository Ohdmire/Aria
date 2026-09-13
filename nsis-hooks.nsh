;; Tauri NSIS 钩子(bundle.windows.nsis.installerHooks,两个变体共用)。
;;
;; 目标:手动卸载(控制面板/双击 uninstall.exe)连 bin\ 内置 ffmpeg/ffprobe
;; 一起删除 —— 覆盖"先用 ffmpeg 变体安装、后用普通变体升级"的跨变体
;; 场景;升级/重装时由新安装器自动调起的旧卸载器则保留它们(免重新
;; 下载约 200MB)。
;;
;; 原理:安装器调起旧卸载器时命令行必带 _?= 参数,手动卸载没有 —— 以此
;; 区分。保留时把两个 exe 改名 .ariakeep 躒过模板的逐文件 Delete(ffmpeg
;; 变体的模板会删自己清单里的资源),POSTUNINSTALL 改回原名;手动卸载时
;; 显式 Delete(普通变体的模板不认识这些文件,必须自己删)。

!include "LogicLib.nsh"

!macro NSIS_HOOK_POSTINSTALL
  ;; 清理历史异常中断卸载残留的 .ariakeep(POSTUNINSTALL 未及执行)
  Delete "$INSTDIR\bin\ffmpeg.exe.ariakeep"
  Delete "$INSTDIR\bin\ffprobe.exe.ariakeep"
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ClearErrors
  ${GetOptions} $CMDLINE "_?=" $0
  ${If} ${Errors}
    ;; 手动卸载:连 ffmpeg/ffprobe 一起删
    Delete "$INSTDIR\bin\ffmpeg.exe"
    Delete "$INSTDIR\bin\ffprobe.exe"
    Delete "$INSTDIR\bin\ffmpeg.exe.ariakeep"
    Delete "$INSTDIR\bin\ffprobe.exe.ariakeep"
    RMDir "$INSTDIR\bin"
  ${Else}
    ;; 升级/重装:改名躲过模板删除,POSTUNINSTALL 改回
    Rename "$INSTDIR\bin\ffmpeg.exe" "$INSTDIR\bin\ffmpeg.exe.ariakeep"
    Rename "$INSTDIR\bin\ffprobe.exe" "$INSTDIR\bin\ffprobe.exe.ariakeep"
    ClearErrors
  ${EndIf}
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ;; 改回原名;bin 目录因非空躲过了模板的 RMDir,原样保留
  Rename "$INSTDIR\bin\ffmpeg.exe.ariakeep" "$INSTDIR\bin\ffmpeg.exe"
  Rename "$INSTDIR\bin\ffprobe.exe.ariakeep" "$INSTDIR\bin\ffprobe.exe"
  ClearErrors
!macroend
