; AI Work 助手 — NSIS 安装钩子（installerHooks）
; 品牌并存策略：老品牌（Trae Work 助手）与新版本 AI Work 助手 **并存运行、互不干扰**。
; 安装/升级本产品时绝不卸载老品牌应用，也绝不清理其安装目录、卸载键与快捷方式；
; 用户数据目录（%APPDATA%\TraeWorkAssistant → %APPDATA%\AIWorkAssistant）
; 由应用首次启动时自动**复制**迁移（复制语义，老版本数据原地保留），此处同样绝不删除。
; 注意：本文件需保存为 UTF-8 with BOM，否则 NSIS Unicode 编译中文会乱码。

!macro NSIS_HOOK_PREINSTALL
  ; 结束本产品线历史命名进程，避免文件占用导致升级安装失败
  ;    （含本地重打包的过渡版主程序 "AI Work 助手.exe"，防止其运行中锁住 POSTINSTALL 清理）
  ;    应用内「检查更新」自动安装时安装器先于应用退出启动，靠此兜底解锁文件占用
  ;    （正常流程应用已自行退出）。
  ;    注意：不结束老品牌进程 "Trae Work 助手.exe"——两版并存，不得干扰老版本运行。
  nsExec::Exec 'taskkill /F /IM "AI Work 助手.exe" /T'
  Pop $R9
  nsExec::Exec 'taskkill /F /IM "trae-work-assistant.exe" /T'
  Pop $R9
  nsExec::Exec 'taskkill /F /IM "ai-work-assistant.exe" /T'
  Pop $R9
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; 主程序已更名为 ai-work-assistant.exe，清理同一安装目录内旧命名的主程序残留
  ; （仅限本产品安装目录内的历史文件，不触及老品牌独立的安装目录）
  Delete "$INSTDIR\AI Work 助手.exe"
  Delete "$INSTDIR\Trae Work 助手.exe"
  Delete "$INSTDIR\trae-work-assistant.exe"
!macroend
