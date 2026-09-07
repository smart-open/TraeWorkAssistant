; AI Work 助手 — NSIS 安装钩子（installerHooks）
; 用途：老品牌（Trae Work 助手 / trae-work-assistant）应用升级安装时自动清理旧版本，
;      用户数据目录（%APPDATA%\TraeWorkAssistant）由应用首次启动时自动迁移，此处绝不删除。
; 注意：本文件需保存为 UTF-8 with BOM，否则 NSIS Unicode 编译中文会乱码。

!macro NSIS_HOOK_PREINSTALL
  ; 1) 结束旧品牌/旧命名进程，避免文件占用导致清理失败
  ;    （含本地重打包的过渡版主程序 "AI Work 助手.exe"，防止其运行中锁住 POSTINSTALL 清理）
  ;    同时结束新命名主进程 ai-work-assistant.exe：应用内「检查更新」自动安装时
  ;    安装器先于应用退出启动，靠此兜底解锁文件占用（正常流程应用已自行退出）。
  nsExec::Exec 'taskkill /F /IM "Trae Work 助手.exe" /T'
  Pop $R9
  nsExec::Exec 'taskkill /F /IM "AI Work 助手.exe" /T'
  Pop $R9
  nsExec::Exec 'taskkill /F /IM "trae-work-assistant.exe" /T'
  Pop $R9
  nsExec::Exec 'taskkill /F /IM "ai-work-assistant.exe" /T'
  Pop $R9

  ; 2) 检测旧品牌产品「Trae Work 助手」→ 静默卸载
  ;    判定依据是安装时的产品名（卸载键），而非版本号：已发布的 v2.4.4 及更早
  ;    安装包均为旧品牌「Trae Work 助手」，同样在此处理；
  ;    仅「AI Work 助手」品牌安装（v3.0.0 起）走 NSIS 原生升级路径，不在此处理。
  ReadRegStr $R0 SHCTX "Software\Microsoft\Windows\CurrentVersion\Uninstall\Trae Work 助手" "UninstallString"
  ${If} $R0 != ""
    ExecWait '"$R0" /S _?=$LOCALAPPDATA\Trae Work 助手'
  ${EndIf}

  ; 3) 兜底清理：旧产品可能残留的安装目录 / 卸载键 / 快捷方式
  RmDir /r "$LOCALAPPDATA\Trae Work 助手"
  DeleteRegKey SHCTX "Software\Microsoft\Windows\CurrentVersion\Uninstall\Trae Work 助手"
  Delete "$DESKTOP\Trae Work 助手.lnk"
  Delete "$SMPROGRAMS\Trae Work 助手.lnk"
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; 主程序已更名为 ai-work-assistant.exe，清理同一安装目录内旧命名的主程序残留
  Delete "$INSTDIR\AI Work 助手.exe"
  Delete "$INSTDIR\Trae Work 助手.exe"
  Delete "$INSTDIR\trae-work-assistant.exe"
!macroend
