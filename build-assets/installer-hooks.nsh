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
  ;    （手动升级且应用已自行退出时首轮清场即完成）。
  ;    注意：不结束老品牌进程 "Trae Work 助手.exe"——两版并存，不得干扰老版本运行。
  ;    注意：本产品两行禁用 /T（树杀，issue #135）——应用内更新场景下安装器是本产品
  ;    主程序的子进程，/T 会连同安装器一起强杀，安装中途静默中断升级失败；主程序
  ;    文件锁由主进程映像持有，杀主进程即解锁，无需树杀（CheckIfAppIsRunning 会兜底）。
  ;    trae-work-assistant.exe 行保留 /T：考古确认该名是本产品 v2.4.4 时代的构建
  ;    二进制名（今 scripts/package_portable.mjs 的前身——v2.4.4 时代为
  ;    package_portable.py——直引 target/release/trae-work-assistant.exe），
  ;    强杀属本产品旧命名自清理（POSTINSTALL 同口径，兼防自定义/便携解压目录重合
  ;    的文件锁）；该时期无应用内更新器，进程不可能是本安装器的祖先，树杀无副作用。
  ;    真正不触碰的老品牌进程是 "Trae Work 助手.exe"（独立安装目录，两版并存）。
  nsExec::Exec 'taskkill /F /IM "AI Work 助手.exe"'
  Pop $R9
  nsExec::Exec 'taskkill /F /IM "trae-work-assistant.exe" /T'
  Pop $R9
  nsExec::Exec 'taskkill /F /IM "ai-work-assistant.exe"'
  Pop $R9

  ; ---- 升级宽限等待（D2 恢复回归 + M-1 修正存活判定）----
  ; 首轮已是 /F 强杀（无 WM_CLOSE 温和路径，被杀进程不走退出链）；静默 1.2s 给
  ; 强杀生效与文件锁释放留缓冲（updater 侧 emit 后 sleep 800ms 才走退出链，两值
  ; 互不依赖；退出链被强杀打断时，系统代理还原由下次在本应用内直开 Trae/豆包
  ; 等客户端（未注入代理）时的 cleanup_stale_local_proxy 自愈）。
  ; 存活探测不依赖退出码——tasklist /FI 无匹配时退出码恒为 0，IntCmp 退出码判定
  ; 是死逻辑（M-1 实证）；改用 nsExec::ExecToStack 捕获 /NH 输出，取首列定长映像名
  ; StrCmp 判定（Unicode NSIS 按字符计数；无匹配时输出为 INFO 行，首列 ≠ 映像名，
  ; 天然判「不在」）。任一本产品线进程仍存活 → Sleep 500 重试，上限 30 次（纯
  ; Sleep ≈15s，加计每轮 3 次 tasklist 子进程自身耗时，最坏实际约 20–30s）；
  ; 超时仍存活 → taskkill /F 兜底强杀后继续安装。
  ; 降级路径：nsExec 自身启动失败（输出栈为 "error"，首列 ≠ 映像名）按「不在」
  ; 跳过宽限直接继续安装——残留存活由后续文件覆盖阶段失败暴露，非静默损坏。
  Sleep 1200
  StrCpy $R8 0
preinstall_grace_loop:
  ; 探测 1/3：ai-work-assistant.exe（首列 21 字符）
  nsExec::ExecToStack 'tasklist /FI "IMAGENAME eq ai-work-assistant.exe" /NH'
  Pop $R9 ; 退出码（恒 0，不用于判定）
  Pop $R7 ; 输出首行
  StrCpy $R7 "$R7" 21
  StrCmp $R7 "ai-work-assistant.exe" preinstall_grace_alive
  ; 探测 2/3：AI Work 助手.exe（首列 14 字符）
  nsExec::ExecToStack 'tasklist /FI "IMAGENAME eq AI Work 助手.exe" /NH'
  Pop $R9
  Pop $R7
  StrCpy $R7 "$R7" 14
  StrCmp $R7 "AI Work 助手.exe" preinstall_grace_alive
  ; 探测 3/3：trae-work-assistant.exe（首列 23 字符）
  nsExec::ExecToStack 'tasklist /FI "IMAGENAME eq trae-work-assistant.exe" /NH'
  Pop $R9
  Pop $R7
  StrCpy $R7 "$R7" 23
  StrCmp $R7 "trae-work-assistant.exe" preinstall_grace_alive
  Goto preinstall_grace_done
preinstall_grace_alive:
  Sleep 500
  IntOp $R8 $R8 + 1
  IntCmp $R8 30 preinstall_grace_kill preinstall_grace_loop preinstall_grace_kill
preinstall_grace_kill:
  nsExec::Exec 'taskkill /F /IM "AI Work 助手.exe"'
  Pop $R9
  nsExec::Exec 'taskkill /F /IM "trae-work-assistant.exe" /T'
  Pop $R9
  nsExec::Exec 'taskkill /F /IM "ai-work-assistant.exe"'
  Pop $R9
preinstall_grace_done:
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; 主程序已更名为 ai-work-assistant.exe，清理同一安装目录内旧命名的主程序残留
  ; （仅限本产品安装目录内的历史文件，不触及老品牌独立的安装目录）
  Delete "$INSTDIR\AI Work 助手.exe"
  Delete "$INSTDIR\Trae Work 助手.exe"
  Delete "$INSTDIR\trae-work-assistant.exe"
!macroend
