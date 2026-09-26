; AI Work 助手 — NSIS 安装钩子（installerHooks）
; 品牌并存策略：老品牌（Trae Work 助手）与新版本 AI Work 助手 **并存运行、互不干扰**。
; 安装/升级本产品时绝不卸载老品牌应用，也绝不清理其安装目录、卸载键与快捷方式；
; 用户数据目录（%APPDATA%\TraeWorkAssistant → %APPDATA%\AIWorkAssistant）
; 由应用首次启动时自动**复制**迁移（复制语义，老版本数据原地保留），此处同样绝不删除。
; 注意：本文件需保存为 UTF-8 with BOM，否则 NSIS Unicode 编译中文会乱码。

!macro NSIS_HOOK_PREINSTALL
  ; 结束本产品线历史命名进程，避免文件占用导致升级安装失败
  ;    （含本地重打包的过渡版主程序 "AI Work 助手.exe"，防止其运行中锁住 POSTINSTALL 清理）
  ;    注意：不结束老品牌进程 "Trae Work 助手.exe"——两版并存，不得干扰老版本运行。
  ;
  ;    修复 Issue #37（应用内自动更新时好时坏）：/UPDATE 模式下安装器由应用作为子进程
  ;    拉起，而应用需约 800ms 后才自行退出。旧逻辑立即 taskkill /T 按「父进程树」递归
  ;    终止——此刻应用仍在运行，安装器尚在其子进程树内，被连带杀死导致安装中断。
  ;    新策略（仅 /UPDATE 模式）：
  ;      1) 先给应用 1.2s 宽限自行退出（应用侧 800ms 延迟退出，正常此间已完成）；
  ;      2) 再以【无 /T】单进程轮询兜底结束残留：taskkill 退出码 0=进程存在并已终止
  ;         （继续等），非 0=进程已不存在（结束等待）；上限 30 次 x 500ms，计数器拉满
  ;         跳出（不用 ${Break}）；全程不用 /T，绝不波及安装器自身。
  ${If} $UpdateMode = 1
    Sleep 1200
    StrCpy $R8 0
    ${While} $R8 < 30
      nsExec::Exec 'taskkill /F /IM "ai-work-assistant.exe"'
      Pop $R9
      nsExec::Exec 'taskkill /F /IM "AI Work 助手.exe"'
      Pop $R10
      ${If} $R9 == 0
      ${OrIf} $R10 == 0
        ; 进程仍在运行：推进计数，500ms 后再查
        IntOp $R8 $R8 + 1
        Sleep 500
      ${Else}
        ; 进程已不存在：以计数器满值跳出循环
        StrCpy $R8 30
      ${EndIf}
    ${EndWhile}
    ; 旧命名 trae-work-assistant.exe 与本安装器无父子关系，保持立即结束
    nsExec::Exec 'taskkill /F /IM "trae-work-assistant.exe" /T'
    Pop $R9
  ${Else}
    ; 手动安装：安装器由用户直接启动，与应用无父子关系，/T 安全且更彻底
    nsExec::Exec 'taskkill /F /IM "AI Work 助手.exe" /T'
    Pop $R9
    nsExec::Exec 'taskkill /F /IM "trae-work-assistant.exe" /T'
    Pop $R9
    nsExec::Exec 'taskkill /F /IM "ai-work-assistant.exe" /T'
    Pop $R9
  ${EndIf}
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; 主程序已更名为 ai-work-assistant.exe，清理同一安装目录内旧命名的主程序残留
  ; （仅限本产品安装目录内的历史文件，不触及老品牌独立的安装目录）
  Delete "$INSTDIR\AI Work 助手.exe"
  Delete "$INSTDIR\Trae Work 助手.exe"
  Delete "$INSTDIR\trae-work-assistant.exe"
!macroend
