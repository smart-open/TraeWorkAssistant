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
  ;    注意：本产品两行禁用 /T（树杀，issue #135）——应用内更新场景下安装器经
  ;    updater 的 `cmd /c start` 中转启动（已脱离本应用进程树），但防树杀误伤
  ;    仍为红线（含旧版升级器/杀软中转等非常规父子形态）；主程序
  ;    文件锁由主进程映像持有，杀主进程即解锁，无需树杀（CheckIfAppIsRunning 会兜底）。
  ;    老品牌二进制行保留 /T：老品牌进程不可能是本安装器的祖先，树杀无副作用。
  ;    审查修复（D2）：当前主程序改为优雅关闭 + 宽限等待——应用内更新（/UPDATE）
  ;    场景下安装器先于应用退出启动（updater emit 后才 exit），覆盖安装时应用可能
  ;    仍在收尾退出；先发无 /F 的 WM_CLOSE 请求，宽限 Sleep 1.2s + 至多 30 次探测
  ;    轮询（存活则每轮 Sleep 500ms），满 30 次仍存活才 /F 兜底强杀，避免直接 /F
  ;    截断退出收尾（未保存状态丢失）；应用未运行时第一轮探测即通过，仅多耗 1.2s。
  ;    历史命名残留进程无退出收尾可言，直接 /F 强杀。
  nsExec::Exec 'taskkill /F /IM "AI Work 助手.exe"'
  Pop $R9
  nsExec::Exec 'taskkill /F /IM "trae-work-assistant.exe" /T'
  Pop $R9
  ; 1) 优雅关闭请求（无 /F → 发 WM_CLOSE，应用未运行则无副作用）
  nsExec::Exec 'taskkill /IM "ai-work-assistant.exe"'
  Pop $R9
  ; 2) 宽限等待：Sleep 1.2s + 至多 30 次探测轮询（存活则 Sleep 500ms 再探，
  ;    满 30 次仍存活直接转强杀，不白等末轮 Sleep——N1 审查修正；
  ;    tasklist 输出探测——审查修复：/FI 无匹配时退出码恒为 0（实测），
  ;    无法区分进程存在性；改按 /NH 表格首列映像名前缀判定（不依赖退出码
  ;    与系统语言）：有匹配 = 进程行 / 无匹配 = 本地化无任务提示（INFO:/信息:））
  Sleep 1200
  StrCpy $R8 0
hook_graceful_wait:
  IntOp $R8 $R8 + 1
  nsExec::ExecToStack 'tasklist /FI "IMAGENAME eq ai-work-assistant.exe" /NH'
  Pop $R9  ; 退出码（恒 0，不参与判定；ExecToStack 启动失败时为 "error"）
  Pop $R0  ; 输出：有匹配=进程行 / 无匹配=本地化提示（启动失败时为空）
  StrCmp $R9 "error" hook_graceful_kill  ; C5 审查加固：探测命令启动失败 →
                                         ; fail-safe 直接转强杀（taskkill 对
                                         ; 不存在进程无副作用），防空输出被
                                         ; 误判为"进程已退出"而跳过兜底
  StrCpy $R0 $R0 21  ; 截取前缀（映像名 "ai-work-assistant.exe" 长 21 字符）
  StrCmp $R0 "ai-work-assistant.exe" 0 hook_gone  ; 无匹配 = 进程已退出
  IntCmp $R8 30 hook_graceful_kill 0 0  ; 满 30 次探测仍存活 → 强杀（不白等）
  Sleep 500
  Goto hook_graceful_wait
hook_graceful_kill:
  ; 3) 超时兜底强杀解锁（无 /T，#135 红线）
  nsExec::Exec 'taskkill /F /IM "ai-work-assistant.exe"'
  Pop $R9
hook_gone:
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; 主程序已更名为 ai-work-assistant.exe，清理同一安装目录内旧命名的主程序残留
  ; （仅限本产品安装目录内的历史文件，不触及老品牌独立的安装目录）
  Delete "$INSTDIR\AI Work 助手.exe"
  Delete "$INSTDIR\Trae Work 助手.exe"
  Delete "$INSTDIR\trae-work-assistant.exe"
!macroend
