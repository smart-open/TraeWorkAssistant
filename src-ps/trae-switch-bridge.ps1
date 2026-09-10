<#
.SYNOPSIS
    Trae Work 账号切换集成桥（非交互模式）
.DESCRIPTION
    供 AI Work 助手（Tauri）调用的非交互切换层。
    封装「关闭 TRAE → 恢复目标账号登录态 → 重置机器码 → 启动 TRAE」流程，
    并以 NDJSON 逐行输出进度，供桌面端渲染步骤条。

    注意：本脚本是集成层，封装账号切换与设备标识重置的全部逻辑，
    供 AI Work 助手（Tauri）以非交互模式调用。

.PARAMETER Action
    Switch（切换账号）/ ResetMachineId（仅重置机器码）/ BackupCurrent（备份当前）

.PARAMETER UserId
    目标账号的 UserID（16 位数字，与 checkin_accounts.json 的 UserID 对齐）

.PARAMETER Json
    以 NDJSON 输出进度（每行一个 JSON 对象）
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('Switch', 'ResetMachineId', 'BackupCurrent', 'RestoreOnly', 'ResetDeviceIds', 'SaveCurrentLogin', 'KeepAlive')]
    [string]$Action,

    [Parameter(Mandatory = $false)]
    [string]$UserId,

    [Parameter(Mandatory = $false)]
    # F-48：档案表驱动四应用（Work/Ide 已完整接入；Doubao/WorkBuddy 定位/启停已就绪，快照管线随各自批次接入）
    [ValidateSet('TraeWork', 'Trae', 'Doubao', 'WorkBuddy')]
    [string]$TargetApp = 'TraeWork',

    [Parameter(Mandatory = $false)]
    # C1：>0 时启动应用注入 --proxy-server（一键以账号打开走代理抓包，行为对齐 open_doubao_app）
    [int]$ProxyPort = 0,

    [Parameter(Mandatory = $false)]
    # C4：快照备份时纳入 Default/IndexedDB（对话历史等完整状态随账号迁移，体积代价大）
    [switch]$IncludeIndexedDB,

    [Parameter(Mandatory = $false)]
    # 防误覆盖守卫（豆包）：桌面端在关闭应用前检测到的当前登录 uid（Local Storage
    # client_device_info / 抓包 multi_sids）。Switch 回写账号槽前须与 current_account.txt
    # 一致才执行——标记文件残留旧 uid（客户端手动重登）时，旧流程会把"当前态"反复刷进
    # 错误槽位，实测曾把 B 账号快照覆盖成混乱状态。为空或不一致 = 只备份 last 并警告。
    [string]$ExpectedCurrentUid = '',

    [Parameter(Mandatory = $false)]
    [switch]$Json
)

# 注意：不要在此加 `#Requires -RunAsAdministrator`。
# 仅 Reset-MachineId 写 HKLM 需管理员；Switch/Backup 普通用户即可运行。
# 若强制要求管理员，普通权限启动的 App 调起脚本会直接 ScriptRequiresElevation 失败。

$ErrorActionPreference = 'Stop'

# stdout 被重定向时 PS 5.1 默认用 OEM 代码页（中文系统 GBK/936），下游 Tauri 按 UTF-8 解码会乱码。
# 统一切 UTF-8：NDJSON 进度行中文在桌面端正常显示（读取端 lossy 解码兼容）。
try {
    [Console]::OutputEncoding = [System.Text.Encoding]::UTF8
    $OutputEncoding = [System.Text.Encoding]::UTF8
} catch {}

# 数据目录：优先 AIWORKDATA_DIR（由桌面端注入），否则回退 %APPDATA%\AIWorkAssistant
$Script:AppDataDir = if ($env:AIWORKDATA_DIR) { $env:AIWORKDATA_DIR } else { "$env:APPDATA\AIWorkAssistant" }
$Script:LogFile = "$Script:AppDataDir\logs\switcher.log"
$Script:_TraeExeCache = $null

# ── 目标应用档案（F-48 表驱动）─────────────────────────────────────────────
# icube 布局（TraeWork/Trae）：同为 icube 内核的 VSCode fork，登录态文件结构完全同构
# （storage.json / state.vscdb / machineid / aha / Network 等），
# 仅 exe 名称、数据目录与快照存储位置不同，按档案参数化即可复用全部切换逻辑。
# chromium/authfile 布局（Doubao/WorkBuddy）：登录态结构不同——
#   Doubao  = Chromium User Data 目录级快照（Local State + Default/Network/Cookies，方案见 doubao-trae-switch-plan.md §2.1）
#   WorkBuddy = auth 文件快照 + 用户数据目录双层恢复（方案见 workbuddy-switch-plan.md §2.2）
# 两者的快照/恢复/设备重置管线随各自应用批次接入（SnapshotLayout 非 icube 时显式报错）。
switch ($TargetApp) {
    'Trae' {
        $Script:AppName         = 'Trae'
        $Script:SnapshotLayout  = 'icube'
        $Script:TraeDataDir     = "$env:APPDATA\Trae CN"
        $Script:ProfilesDir     = "$Script:AppDataDir\data\profiles_trae"
        $Script:SettingsPathKey = 'trae_cn_path'
        $Script:ProcNames       = @('Trae CN')
        $Script:ExeNames        = @('Trae CN.exe')
        $Script:LnkPatterns     = @('*TRAE*', '*Trae*')
        $Script:RegPatterns     = @('*TRAE*', '*Trae*')
        $Script:ProcPatterns    = @('Trae*', 'TRAE*')
        $Script:ExeCandidates   = @(
            "$env:LOCALAPPDATA\Programs\Trae CN\Trae CN.exe",
            "$env:ProgramFiles\Trae CN\Trae CN.exe",
            'D:\Programs\Trae CN\Trae CN.exe'
        )
    }
    'Doubao' {
        # 豆包桌面版（doubao-trae-switch-plan.md §1.1 实测布局）
        $Script:AppName         = '豆包'
        $Script:SnapshotLayout  = 'chromium'
        $Script:TraeDataDir     = "$env:LOCALAPPDATA\Doubao\User Data"
        $Script:ProfilesDir     = "$Script:AppDataDir\data\profiles_doubao"
        $Script:SettingsPathKey = 'doubao_path'
        $Script:ProcNames       = @('Doubao')
        $Script:ExeNames        = @('Doubao.exe')
        # 豆包优雅关闭等待 8 秒（chromium 壳退出前要落盘 leveldb/cookie，3 秒实测经常不够，
        # 强杀导致文件锁 → 备份静默缺文件 → 恢复后登录态丢失）
        $Script:GracefulWaitSecs = 8
        # P2：lnk/注册表/进程回退的过滤词随应用参数化（旧版写死 Trae，对豆包三步回退全部失效）
        $Script:LnkPatterns     = @('*Doubao*', '*豆包*')
        $Script:RegPatterns     = @('*Doubao*', '*豆包*')
        $Script:ProcPatterns    = @('Doubao*')
        $Script:ExeCandidates   = @(
            "$env:LOCALAPPDATA\Doubao\Application\Doubao.exe",
            "$env:ProgramFiles\Doubao\Application\Doubao.exe"
        )
    }
    'WorkBuddy' {
        # WorkBuddy 桌面版（workbuddy-switch-plan.md §1.1 实测布局）
        $Script:AppName         = 'WorkBuddy'
        $Script:SnapshotLayout  = 'authfile'
        $Script:TraeDataDir     = "$env:USERPROFILE\.workbuddy"
        $Script:ProfilesDir     = "$Script:AppDataDir\data\profiles_workbuddy"
        $Script:SettingsPathKey = 'workbuddy_path'
        $Script:ProcNames       = @('WorkBuddy')
        $Script:ExeNames        = @('WorkBuddy.exe')
        $Script:LnkPatterns     = @('*WorkBuddy*')
        $Script:RegPatterns     = @('*WorkBuddy*')
        $Script:ProcPatterns    = @('WorkBuddy*')
        $Script:ExeCandidates   = @(
            "$env:LOCALAPPDATA\Programs\WorkBuddy\WorkBuddy.exe"
        )
    }
    default {
        $Script:AppName         = 'Trae Work'
        $Script:SnapshotLayout  = 'icube'
        $Script:GracefulWaitSecs = 3
        $Script:TraeDataDir     = "$env:APPDATA\TRAE SOLO CN"
        $Script:ProfilesDir     = "$Script:AppDataDir\data\profiles"
        $Script:SettingsPathKey = 'trae_path'
        $Script:ProcNames       = @('TRAE SOLO CN', 'TRAE SOLO', 'Trae')
        $Script:ExeNames        = @('TRAE SOLO CN.exe', 'TRAE SOLO.exe', 'Trae.exe')
        $Script:LnkPatterns     = @('*TRAE*', '*Trae*')
        $Script:RegPatterns     = @('*TRAE*', '*Trae*')
        $Script:ProcPatterns    = @('Trae*', 'TRAE*')
        $Script:ExeCandidates   = @(
            "$env:LOCALAPPDATA\Programs\TRAE SOLO CN\TRAE SOLO CN.exe",
            "$env:LOCALAPPDATA\Programs\TRAE SOLO\TRAE SOLO.exe",
            "$env:ProgramFiles\TRAE SOLO CN\TRAE SOLO CN.exe",
            "$env:ProgramFiles\TRAE SOLO\TRAE SOLO.exe",
            "$env:LOCALAPPDATA\Programs\Trae\Trae.exe",
            "$env:ProgramFiles\Trae\Trae.exe",
            'D:\Programs\TRAE SOLO CN\TRAE SOLO CN.exe'
        )
    }
}
$Script:CurrentAccountFile = "$Script:ProfilesDir\current_account.txt"
# C1：启动代理端口（>0 = 启动时注入 --proxy-server，供一键以账号打开复用切换管线）
$Script:LaunchProxyPort = $ProxyPort

# 校验候选 exe 路径是否属于当前目标应用（防止 lnk/注册表/进程回退解析到另一个应用）
function Test-ExeMatchesApp {
    param([string]$Path)
    if (-not $Path) { return $false }
    $name = [System.IO.Path]::GetFileName($Path)
    return $Script:ExeNames -contains $name
}

function Find-TraeExe {
    # ── 顺序原则（修复「首次切换误用 Trae CN.exe」）─────────────────────────────
    # 旧逻辑把「运行中进程」作为最高优先级，导致残留/错误的 Trae 进程（如旧的
    # Trae CN.exe）被优先采用，从而启动错误的 exe。现改为：
    #   1) 用户显式配置 > 2) 候选路径 > 3) 开始菜单/桌面 lnk > 4) 注册表
    #   > 5) 运行中进程（最后回退）> 6) 进程缓存（兜底，仅自定义安装且当前未运行时）
    # 这样正常情况下总是解析到用户真实安装的 TRAE SOLO CN，而非被残留进程带偏。

    # 1. 用户显式配置路径（最高优先级）
    $settingsFile = Join-Path $Script:AppDataDir 'conf\app_settings.json'
    if (Test-Path $settingsFile) {
        try {
            $settings = Get-Content $settingsFile -Raw | ConvertFrom-Json
            $customPath = $settings.$($Script:SettingsPathKey)
            if ($customPath -and (Test-Path $customPath)) {
                $Script:_TraeExeCache = $customPath
                return $Script:_TraeExeCache
            }
        } catch {}
    }

    # 2. 多候选路径探测（与 Rust env.rs 保持一致）
    foreach ($c in $Script:ExeCandidates) {
        if (Test-Path $c) {
            $Script:_TraeExeCache = $c
            return $Script:_TraeExeCache
        }
    }

    # 3. .lnk 快捷方式解析（开始菜单 / 桌面）
    try {
        $lnkDirs = @(
            "$env:APPDATA\Microsoft\Windows\Start Menu\Programs",
            "$env:ProgramData\Microsoft\Windows\Start Menu\Programs",
            "$env:USERPROFILE\Desktop",
            "$env:PUBLIC\Desktop"
        )
        $shell = New-Object -ComObject WScript.Shell
        foreach ($dir in $lnkDirs) {
            if (-not (Test-Path $dir)) { continue }
            $lnks = Get-ChildItem -Path $dir -Filter '*.lnk' -Recurse -ErrorAction SilentlyContinue |
                Where-Object {
                    $lnkName = $_.Name
                    foreach ($p in $Script:LnkPatterns) { if ($lnkName -like $p) { return $true } }
                    return $false
                }
            foreach ($lnk in $lnks) {
                $shortcut = $shell.CreateShortcut($lnk.FullName)
                if ($shortcut.TargetPath -and (Test-Path $shortcut.TargetPath) -and (Test-ExeMatchesApp -Path $shortcut.TargetPath)) {
                    $Script:_TraeExeCache = $shortcut.TargetPath
                    return $Script:_TraeExeCache
                }
            }
        }
    } catch {}

    # 4. 注册表回退
    try {
        $regKeys = @(
            'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*',
            'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*',
            'HKCU:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*'
        )
        foreach ($key in $regKeys) {
            $items = Get-ItemProperty $key -ErrorAction SilentlyContinue |
                Where-Object {
                    $dn = $_.DisplayName
                    foreach ($p in $Script:RegPatterns) { if ($dn -like $p) { return $true } }
                    return $false
                }
            foreach ($item in $items) {
                # 尝试 DisplayIcon
                if ($item.DisplayIcon) {
                    $iconPath = $item.DisplayIcon -replace ',', ''
                    $iconPath = $iconPath.Trim()
                    if ((Test-Path $iconPath) -and (Test-ExeMatchesApp -Path $iconPath)) {
                        $Script:_TraeExeCache = $iconPath
                        return $Script:_TraeExeCache
                    }
                }
                # 尝试 InstallLocation
                if ($item.InstallLocation) {
                    $loc = $item.InstallLocation.Trim()
                    foreach ($exeName in $Script:ExeNames) {
                        $exe = Join-Path $loc $exeName
                        if (Test-Path $exe) {
                            $Script:_TraeExeCache = $exe
                            return $Script:_TraeExeCache
                        }
                    }
                }
            }
        }
    } catch {}

    # 5. 运行中进程（最后回退之一）：仅当以上都找不到时才用，
    #    避免残留/错误的 Trae 进程误导启动路径。同时排除本助手自身进程
    #    （本应用进程名为 "ai-work-assistant"，不以 Trae 开头，不会被 Trae* 过滤命中，
    #    保留排除逻辑以防旧版运行残留），避免把 App 本体当成 Trae 启动。
    try {
        $selfPid = $PID
        $parentPid = $selfPid
        $KnownAppName = 'ai-work-assistant'
        $parentName = $KnownAppName
        try {
            $pp = (Get-CimInstance -ClassName Win32_Process -Filter "ProcessId = $selfPid" -ErrorAction SilentlyContinue).ParentProcessId
            if ($pp) {
                $parentPid = $pp
                $pproc = Get-Process -Id $parentPid -ErrorAction SilentlyContinue
                if ($pproc) { $parentName = $pproc.Name }
            }
        } catch {}
        $proc = Get-Process -Name $Script:ProcPatterns -ErrorAction SilentlyContinue |
            Where-Object {
                $_.Path -and $_.Id -ne $selfPid -and $_.Id -ne $parentPid -and $_.Name -ne $parentName -and
                (Test-ExeMatchesApp -Path $_.Path)
            }
        if ($proc) {
            $exePath = $proc | Select-Object -First 1 -ExpandProperty Path
            if ((Test-Path $exePath) -and (Test-ExeMatchesApp -Path $exePath)) {
                $Script:_TraeExeCache = $exePath
                return $Script:_TraeExeCache
            }
        }
    } catch {}

    # 6. 进程缓存兜底（自定义安装、当前未运行、且上述均未命中时）
    if ($Script:_TraeExeCache -and (Test-Path $Script:_TraeExeCache)) {
        return $Script:_TraeExeCache
    }

    return $null
}

function Write-Step {
    param([string]$Stage, [string]$Message, [string]$Status = 'info')
    $obj = [ordered]@{
        stage   = $Stage
        status  = $Status
        message = $Message
        time    = (Get-Date -Format 'yyyy-MM-dd HH:mm:ss')
    } | ConvertTo-Json -Compress
    if ($Json) {
        $obj | Out-Host
    } else {
        Write-Host "[$Stage] $Message"
    }
    try {
        if (-not (Test-Path (Split-Path $Script:LogFile))) { New-Item -ItemType Directory -Path (Split-Path $Script:LogFile) -Force | Out-Null }
        Add-Content -Path $Script:LogFile -Value "[$((Get-Date -Format 'yyyy-MM-dd HH:mm:ss'))] [$Stage] $Message" -Encoding UTF8
    } catch {}
}

function Get-CurrentAccount {
    if (Test-Path $Script:CurrentAccountFile) {
        try {
            $id = (Get-Content $Script:CurrentAccountFile -Raw).Trim()
            if ($id) { return $id }
        } catch {}
    }
    return $null
}

function Set-CurrentAccount {
    param([string]$AccountId)
    try {
        $dir = Split-Path $Script:CurrentAccountFile
        if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }
        Set-Content -Path $Script:CurrentAccountFile -Value $AccountId -NoNewline -Encoding UTF8
    } catch {}
}

function Stop-Trae {
    # 排除本助手自身进程：本应用进程名为 "ai-work-assistant"（旧版为 "Trae Work 助手"，以
    # "Trae" 开头、会被 Get-Process -Name 'Trae*' 命中并被 Stop-Process 误杀导致 App 退出；
    # 新版不以 Trae 开头，保留排除逻辑兼容旧版运行场景）。
    $selfPid = $PID
    $parentPid = $selfPid
    $KnownAppName = 'ai-work-assistant'
    $parentName = $KnownAppName
    try {
        $pp = (Get-CimInstance -ClassName Win32_Process -Filter "ProcessId = $selfPid" -ErrorAction SilentlyContinue).ParentProcessId
        if ($pp) {
            $parentPid = $pp
            $pproc = Get-Process -Id $parentPid -ErrorAction SilentlyContinue
            if ($pproc) { $parentName = $pproc.Name }
        }
    } catch {}
    $p = Get-Process -Name $Script:ProcNames -ErrorAction SilentlyContinue | Where-Object {
        $_.Id -ne $selfPid -and $_.Id -ne $parentPid -and $_.Name -ne $parentName
    }
    if ($p) {
        Write-Step -Stage 'stop' -Message "正在关闭 $($Script:AppName)" -Status 'running'
        # 在关闭前缓存 exe 路径，供 Start-Trae 使用
        $exePath = $p | Select-Object -First 1 -ExpandProperty Path -ErrorAction SilentlyContinue
        if ((Test-Path $exePath) -and (Test-ExeMatchesApp -Path $exePath)) {
            $Script:_TraeExeCache = $exePath
        }
        # F-47 三级关闭：先优雅关闭（CloseMainWindow 发送 WM_CLOSE，让 Electron 正常落盘，
        # 避免强杀导致 leveldb/vscdb 文件锁），等待最长 3 秒（实测通常 1s 内退出）；
        # 仍未退出再强制结束。
        $graceful = $p | Where-Object { -not $_.HasExited } | ForEach-Object {
            try { $_.CloseMainWindow() | Out-Null; $_ } catch {}
        }
        if ($graceful) {
            $gw = if ($Script:GracefulWaitSecs) { $Script:GracefulWaitSecs } else { 3 }
            Write-Step -Stage 'stop' -Message "已发送优雅关闭请求，等待进程退出（最长 $gw 秒）" -Status 'running'
            $waited = 0
            while ($waited -lt $gw) {
                Start-Sleep -Seconds 1
                $waited++
                $still = Get-Process -Name $Script:ProcNames -ErrorAction SilentlyContinue | Where-Object {
                    $_.Id -ne $selfPid -and $_.Id -ne $parentPid -and $_.Name -ne $parentName
                }
                if (-not $still) { break }
            }
        }
        # 第二级：仍有存活进程 → 强杀
        $p = Get-Process -Name $Script:ProcNames -ErrorAction SilentlyContinue | Where-Object {
            $_.Id -ne $selfPid -and $_.Id -ne $parentPid -and $_.Name -ne $parentName
        }
        if ($p) {
            Write-Step -Stage 'stop' -Message '优雅关闭超时，强制结束进程' -Status 'warn'
            $p | Stop-Process -Force
        }
        # 等待进程完全退出，最多再等 5 秒（强杀后 handle 释放）
        $waited = 0
        while ($waited -lt 5) {
            Start-Sleep -Seconds 1
            $waited++
            $still = Get-Process -Name $Script:ProcNames -ErrorAction SilentlyContinue | Where-Object {
                $_.Id -ne $selfPid -and $_.Id -ne $parentPid -and $_.Name -ne $parentName
            }
            if (-not $still) { break }
        }
        if ($waited -ge 5) {
            Write-Step -Stage 'stop' -Message "进程未在 $waited 秒内完全退出，可能仍有文件锁，请手动关闭后重试" -Status 'error'
        }
    } else {
        Write-Step -Stage 'stop' -Message "$($Script:AppName) 未运行" -Status 'skip'
        # 进程未运行时也尝试查找 exe 路径并缓存
        if (-not $Script:_TraeExeCache) {
            $found = Find-TraeExe
            if ($found) {
                $Script:_TraeExeCache = $found
            }
        }
    }
}

function Start-Trae {
    $exe = Find-TraeExe
    if (-not $exe) {
        Write-Step -Stage 'start' -Message "未找到 $($Script:AppName) 安装路径，请在设置中指定" -Status 'error'
        throw "未找到 $($Script:AppName) 可执行文件"
    }
    if ($Script:LaunchProxyPort -gt 0) {
        Write-Step -Stage 'start' -Message "正在启动 $($Script:AppName)（注入代理 127.0.0.1:$($Script:LaunchProxyPort)）: $exe" -Status 'running'
        Start-Process -FilePath $exe -ArgumentList "--proxy-server=http://127.0.0.1:$($Script:LaunchProxyPort)" -WindowStyle Normal
    } else {
        Write-Step -Stage 'start' -Message "正在启动 $($Script:AppName): $exe" -Status 'running'
        Start-Process -FilePath $exe -WindowStyle Normal
    }
}

function Reset-MachineId {
    # 重置 6 层机器码中的 MachineGuid（需管理员）。非管理员时跳过并提示，不阻断切换。
    $newGuid = (New-Guid).Guid
    try {
        Set-ItemProperty -Path 'HKLM:\SOFTWARE\Microsoft\Cryptography' -Name 'MachineGuid' -Value $newGuid -Force
        Write-Step -Stage 'machine' -Message "机器码已重置为 $newGuid" -Status 'ok'
    } catch {
        Write-Step -Stage 'machine' -Message "重置机器码需要管理员权限，已跳过（不影响账号切换）: $_" -Status 'skip'
    }
}

function Reset-DeviceIdsOnly {
    # F-48：6 层重置针对 icube 布局（storage.json/machineid/vscdb），其他布局随各自批次接入
    if ($Script:SnapshotLayout -eq 'chromium') {
        # 豆包为 Chromium 壳，登录态与 machineId 等设备标识无强绑定（plan §0：设备隔离风险低），
        # 目录级快照恢复即完成账号隔离，无需（也没有）6 层重置语义。
        Write-Step -Stage 'device' -Message "$($Script:AppName) 为 Chromium 布局，登录态与设备标识无强绑定，无需重置（快照恢复即完成隔离）" -Status 'skip'
        return
    }
    if ($Script:SnapshotLayout -ne 'icube') {
        Write-Step -Stage 'device' -Message "$($Script:AppName) 布局为 '$($Script:SnapshotLayout)'，设备重置管线尚未接入（F-48 预留）" -Status 'error'
        throw "$($Script:AppName) 的设备重置尚未实现（布局=$($Script:SnapshotLayout)）"
    }
    <#
    .SYNOPSIS
        6 层设备标识重置（本项目自主设计）
    .DESCRIPTION
        1. machineid 文件 → 新 hex32 UUID
        2. storage.json telemetry.machineId / telemetry.sqmId → 替换
        3. storage.json aha.device.device_id → 替换
        4. aha/TinyStorage device_id → 清除
        5. 注册表 MachineGuid → 替换（需管理员）
        6. trae-webview 追踪数据 → 清除
        额外：删除 has_device_id_updated_to_aha 标记位
    #>
    $traeDir = $Script:TraeDataDir
    if (-not (Test-Path $traeDir)) {
        Write-Step -Stage 'device' -Message "TRAE 数据目录不存在: $traeDir" -Status 'error'
        return
    }

    $newMachineId = -join ((1..32) | ForEach-Object { '{0:x}' -f (Get-Random -Maximum 16) })
    $newDeviceId = -join ((1..15) | ForEach-Object { Get-Random -Maximum 10 })
    $newSqmId = (New-Guid).Guid
    $resetCount = 0

    # 1. machineid 文件
    $machineIdFile = Join-Path $traeDir 'machineid'
    if (Test-Path $machineIdFile) {
        try {
            Set-Content -Path $machineIdFile -Value $newMachineId -NoNewline -Encoding UTF8
            Write-Step -Stage 'device' -Message "[1/6] machineid 已重置" -Status 'ok'
            $resetCount++
        } catch {
            Write-Step -Stage 'device' -Message "[1/6] machineid 重置失败: $_" -Status 'skip'
        }
    } else {
        Write-Step -Stage 'device' -Message "[1/6] machineid 文件不存在，跳过" -Status 'skip'
    }

    # 2 & 3. storage.json — telemetry.machineId / sqmId + aha.device.device_id
    # 注意：storage.json 在 User\globalStorage\ 下，且使用点号键名（非嵌套对象）
    $storageFile = Join-Path $traeDir 'User\globalStorage\storage.json'
    if (Test-Path $storageFile) {
        try {
            $raw = Get-Content $storageFile -Raw
            $storage = $raw | ConvertFrom-Json
            $changed = $false
            # 点号键名访问：$storage.'telemetry.machineId' 而非 $storage.telemetry.machineId
            if ($storage.'telemetry.machineId' -ne $null) {
                $storage.'telemetry.machineId' = $newMachineId
                $changed = $true
            }
            if ($storage.'telemetry.sqmId' -ne $null) {
                $storage.'telemetry.sqmId' = $newSqmId
                $changed = $true
            }
            if ($storage.'aha.device.device_id' -ne $null) {
                $storage.'aha.device.device_id' = $newDeviceId
                $changed = $true
            }
            # 删除 has_device_id_updated_to_aha 标记位
            if ($storage.'has_device_id_updated_to_aha' -ne $null) {
                $storage.PSObject.Properties.Remove('has_device_id_updated_to_aha')
                $changed = $true
            }
            if ($changed) {
                $storage | ConvertTo-Json -Depth 20 | Set-Content -Path $storageFile -Encoding UTF8
                Write-Step -Stage 'device' -Message "[2/3] storage.json 设备标识已重置" -Status 'ok'
                $resetCount++
            } else {
                Write-Step -Stage 'device' -Message "[2/3] storage.json 无需修改" -Status 'skip'
            }
        } catch {
            Write-Step -Stage 'device' -Message "[2/3] storage.json 重置失败: $_" -Status 'skip'
        }
    } else {
        Write-Step -Stage 'device' -Message "[2/3] storage.json 不存在，跳过" -Status 'skip'
    }

    # 4. aha/TinyStorage device_id — 清除
    $tinyStorageDir = Join-Path $traeDir 'aha\TinyStorage'
    if (Test-Path $tinyStorageDir) {
        try {
            $tinyFiles = Get-ChildItem -Path $tinyStorageDir -Recurse -File -ErrorAction SilentlyContinue
            foreach ($f in $tinyFiles) {
                $content = Get-Content $f.FullName -Raw -ErrorAction SilentlyContinue
                if ($content -and $content -match 'device_id') {
                    Remove-Item $f.FullName -Force
                }
            }
            Write-Step -Stage 'device' -Message "[4/6] aha/TinyStorage device_id 已清除" -Status 'ok'
            $resetCount++
        } catch {
            Write-Step -Stage 'device' -Message "[4/6] aha/TinyStorage 清除失败: $_" -Status 'skip'
        }
    } else {
        Write-Step -Stage 'device' -Message "[4/6] aha/TinyStorage 目录不存在，跳过" -Status 'skip'
    }

    # 5. 注册表 MachineGuid（需管理员）
    try {
        Set-ItemProperty -Path 'HKLM:\SOFTWARE\Microsoft\Cryptography' -Name 'MachineGuid' -Value $newSqmId -Force
        Write-Step -Stage 'device' -Message "[5/6] 注册表 MachineGuid 已重置" -Status 'ok'
        $resetCount++
    } catch {
        Write-Step -Stage 'device' -Message "[5/6] 注册表 MachineGuid 重置需要管理员权限，已跳过" -Status 'skip'
    }

    # 6. trae-webview 追踪数据（Cookies/Local Storage/Session Storage）
    $webviewDir = Join-Path $traeDir 'Partitions\trae-webview'
    if (Test-Path $webviewDir) {
        try {
            $clearDirs = @('Network', 'Local Storage', 'Session Storage')
            foreach ($d in $clearDirs) {
                $target = Join-Path $webviewDir $d
                if (Test-Path $target) {
                    Remove-Item $target -Recurse -Force -ErrorAction SilentlyContinue
                }
            }
            Write-Step -Stage 'device' -Message "[6/6] trae-webview 追踪数据已清除" -Status 'ok'
            $resetCount++
        } catch {
            Write-Step -Stage 'device' -Message "[6/6] trae-webview 清除失败: $_" -Status 'skip'
        }
    } else {
        Write-Step -Stage 'device' -Message "[6/6] trae-webview 目录不存在，跳过" -Status 'skip'
    }

    Write-Step -Stage 'device' -Message "6 层设备标识重置完成（$resetCount/6 层成功）" -Status $(if ($resetCount -ge 4) { 'ok' } else { 'info' })
}

# ── chromium 布局快照（豆包，P2）───────────────────────────────────────────
# 白名单依据 doubao-trae-switch-plan.md §2.1 + 多账号隔离实测（2026-09-09）：
#   必选  Local State（活跃 Profile 指针 profile.last_used + cookie 解密密钥元数据，缺失则恢复后 cookie 无法解密）
#         <每个 Profile>/Network/Cookies*（登录 cookie，含 journal；旧布局兜底 <profile>/Cookies*）
#         <每个 Profile>/Local Storage/leveldb/（web 侧登录/偏好 KV）
#   建议  <每个 Profile>/Session Storage/、DoubaoStorage/、Preferences、
#         saman_app_state、saman_shell_db_storage/（User Data 根，跨 Profile 共享）
#   排除  <每个 Profile>/IndexedDB/（体积大，默认排除；C4 可经 -IncludeIndexedDB 纳入，恢复时快照内含即回写）
#   元数据 snapshot_meta.json（C3）：schemaVersion + Chromium 版本 + Profile 数，恢复前做完整性校验
# 多 Profile：豆包自带账号隔离（saman.account_isolation_config），登录会话可能位于任意
# Profile——只抓 Default 会漏掉活跃会话，恢复后客户端打开的活跃 Profile 未登录（实测根因）。
# 结构相对 User Data 镜像存放，恢复时对称回写；切换流程的 'last' 槽位即回滚保护。

# 白名单项复制（文件/目录自适应）：返回复制后目标是否真实存在
function Copy-SnapshotItem {
    param([string]$SrcPath, [string]$DestPath)
    if (-not (Test-Path $SrcPath)) { return $false }
    if (Test-Path $SrcPath -PathType Container) {
        if (Test-Path $DestPath) { Remove-Item $DestPath -Recurse -Force -ErrorAction SilentlyContinue }
        $parent = Split-Path $DestPath -Parent
        if (-not (Test-Path $parent)) { New-Item -ItemType Directory -Path $parent -Force | Out-Null }
        Copy-Item $SrcPath $DestPath -Recurse -Force -ErrorAction SilentlyContinue | Out-Null
    } else {
        $parent = Split-Path $DestPath -Parent
        if (-not (Test-Path $parent)) { New-Item -ItemType Directory -Path $parent -Force | Out-Null }
        # 先删旧目标再拷贝：若源文件被锁导致 Copy-Item 失败，避免旧文件残留冒充"备份成功"
        # （曾导致快照里留下陈旧 cookie 文件且校验通过，恢复后登录态错乱）
        if (Test-Path $DestPath) { Remove-Item $DestPath -Force -ErrorAction SilentlyContinue }
        Copy-Item $SrcPath $DestPath -Force -ErrorAction SilentlyContinue | Out-Null
    }
    return (Test-Path $DestPath)
}

# 列出 Chromium User Data 布局下的 Profile 目录（Default + Profile N，与 doubao_chats.py 对齐）
function Get-ChromiumProfileDirs {
    param([string]$Base)
    if (-not (Test-Path $Base)) { return @() }
    try {
        return @(Get-ChildItem -Path $Base -Directory -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -eq 'Default' -or $_.Name -like 'Profile *' } |
            Sort-Object Name)
    } catch {
        return @()
    }
}

function Backup-ChromiumProfile {
    param([string]$Slot)
    $dest = Join-Path $Script:ProfilesDir $Slot
    if (-not (Test-Path $Script:TraeDataDir)) {
        Write-Step -Stage 'backup' -Message '当前数据目录不存在，跳过备份' -Status 'skip'
        return
    }
    # 单代回滚保护：覆盖已有槽位前，把现有快照整体挪到 <slot>.bak（上一代 .bak 直接淘汰）。
    # 背景：Switch 的"备份当前登录态到来源槽"依赖 current_account.txt 与客户端实际登录一致；
    # 一旦不一致（客户端手动重登/保存错槽），会把错误状态反复刷进该槽且不可恢复——实测曾把
    # B 账号的快照覆盖成混乱状态。有 .bak 后任何一次覆盖都可回退一代。
    if (Test-Path $dest) {
        $bakDir = "$dest.bak"
        try {
            if (Test-Path $bakDir) { Remove-Item $bakDir -Recurse -Force -ErrorAction SilentlyContinue }
            Move-Item $dest $bakDir -Force -ErrorAction Stop
            Write-Step -Stage 'backup' -Message "原 $Slot 快照已备份到 $Slot.bak（可回滚一代）" -Status 'info'
        } catch {
            Write-Step -Stage 'backup' -Message "旧快照挪移失败（将直接覆盖）: $_" -Status 'warn'
        }
    }
    if (-not (Test-Path $dest)) { New-Item -ItemType Directory -Path $dest -Force | Out-Null }
    $src = $Script:TraeDataDir
    $copied = 0

    # 必选 1: Local State（活跃 Profile 指针 profile.last_used + cookie 解密密钥元数据）
    if (Copy-SnapshotItem -SrcPath "$src\Local State" -DestPath "$dest\Local State") { $copied++ }

    # 多 Profile 遍历（豆包自带账号隔离，登录会话可能位于任意 Profile——只抓 Default 会
    # 漏掉活跃会话，恢复后客户端打开的活跃 Profile 未登录，实测根因）。
    # 结构相对 User Data 镜像存放（<snapshot>\<Profile>\...），恢复时对称回写。
    $profiles = Get-ChromiumProfileDirs -Base $src
    foreach ($p in $profiles) {
        $n = $p.Name
        # 必选 2: Cookies*（新布局 <profile>\Network\；旧布局兜底 <profile>\Cookies*，统一归位快照 Network\）
        $ckBase = if (Test-Path (Join-Path $p.FullName 'Network\Cookies')) { Join-Path $p.FullName 'Network' } else { $p.FullName }
        Get-ChildItem -Path $ckBase -Filter 'Cookies*' -File -ErrorAction SilentlyContinue | ForEach-Object {
            if (Copy-SnapshotItem -SrcPath $_.FullName -DestPath "$dest\$n\Network\$($_.Name)") { $copied++ }
        }
        # 必选 3: <profile>/Local Storage/leveldb（web 侧登录/偏好 KV）
        if (Copy-SnapshotItem -SrcPath (Join-Path $p.FullName 'Local Storage\leveldb') -DestPath "$dest\$n\Local Storage\leveldb") { $copied++ }
        # 建议: Session Storage、DoubaoStorage、Preferences（per-profile）
        if (Copy-SnapshotItem -SrcPath (Join-Path $p.FullName 'Session Storage') -DestPath "$dest\$n\Session Storage") { $copied++ }
        if (Copy-SnapshotItem -SrcPath (Join-Path $p.FullName 'DoubaoStorage') -DestPath "$dest\$n\DoubaoStorage") { $copied++ }
        if (Copy-SnapshotItem -SrcPath (Join-Path $p.FullName 'Preferences') -DestPath "$dest\$n\Preferences") { $copied++ }
        # 建议: <profile>/saman_shell_db_storage（saman 账号体系客户端级数据库 per-profile 存一份，
        # 实测 Live User Data 的 Default/Profile N 下均有同名目录——仅抓根级会丢各 Profile 的
        # shell 侧账号状态，切换后可能触发客户端重建该 Profile 的账号数据）
        if (Copy-SnapshotItem -SrcPath (Join-Path $p.FullName 'saman_shell_db_storage') -DestPath "$dest\$n\saman_shell_db_storage") { $copied++ }
        # C4：可选纳入 <profile>/IndexedDB（对话历史等完整状态；体积大，默认排除）
        if ($IncludeIndexedDB) {
            if (Copy-SnapshotItem -SrcPath (Join-Path $p.FullName 'IndexedDB') -DestPath "$dest\$n\IndexedDB") { $copied++ }
        }
    }
    if (@($profiles).Count -eq 0) {
        Write-Step -Stage 'backup' -Message '未发现任何 Profile 目录（Default / Profile N），豆包可能从未启动过' -Status 'warn'
    }

    # 建议: saman 账号体系状态（User Data 根，文件/目录均有，Copy-SnapshotItem 自适应）
    if (Copy-SnapshotItem -SrcPath "$src\saman_app_state" -DestPath "$dest\saman_app_state") { $copied++ }
    if (Copy-SnapshotItem -SrcPath "$src\saman_shell_db_storage" -DestPath "$dest\saman_shell_db_storage") { $copied++ }

    # C3：快照版本元数据（恢复前校验用，防豆包升级后旧快照损坏）
    $snapshotVer = ''
    if (Copy-SnapshotItem -SrcPath "$src\Last Version" -DestPath "$dest\Last Version") { $copied++ }
    try {
        if (Test-Path "$dest\Last Version") { $snapshotVer = (Get-Content "$dest\Last Version" -Raw -ErrorAction SilentlyContinue).Trim() }
    } catch {}
    try {
        $meta = [ordered]@{
            schemaVersion    = 1
            layout           = 'chromium'
            app              = $Script:AppName
            chromiumVersion  = $snapshotVer
            includeIndexedDB = [bool]$IncludeIndexedDB
            profileCount     = @($profiles).Count
            createdAt        = (Get-Date -Format 'yyyy-MM-dd HH:mm:ss')
        }
        $meta | ConvertTo-Json -Compress | Set-Content -Path (Join-Path $dest 'snapshot_meta.json') -Encoding UTF8
    } catch {
        Write-Step -Stage 'backup' -Message "快照元数据写入失败（不影响快照本身）: $_" -Status 'warn'
    }

    if ($copied -eq 0) {
        Write-Step -Stage 'backup' -Message '未发现任何可备份的登录态文件（豆包可能未登录或数据目录为空）' -Status 'warn'
    } else {
        Write-Step -Stage 'backup' -Message "已备份当前登录态到 $Slot ($copied 项, $($profiles.Count) 个 Profile)" -Status 'ok'
    }
}

# C3：恢复前快照完整性校验（防豆包升级/复制中断后旧快照损坏）：
#   ① schemaVersion：本工具仅支持 1，不兼容直接中止；
#   ② leveldb 完整性：快照内每个 Profile 的 Local Storage/leveldb 的 CURRENT 必须存在
#     且指向的 MANIFEST 文件在快照内（多 Profile 快照逐个校验）；
#   ②b 登录 Cookie 存在性：快照内所有 Profile 均无 Cookies 时警告（可能为未登录态保存）；
#   ③ 版本差异：快照 Chromium 版本 ≠ 当前安装版本时警告（继续恢复，异常时重新登录保存）。
function Test-SnapshotIntegrity {
    param([string]$Path)
    # $Path 为快照目录的最终路径（主槽或 .bak 回退槽，由调用方解析）
    $src = $Path
    $slotLabel = Split-Path $Path -Leaf

    # ① schemaVersion
    $metaFile = Join-Path $src 'snapshot_meta.json'
    $snapshotVer = ''
    try {
        if (Test-Path (Join-Path $src 'Last Version')) { $snapshotVer = (Get-Content (Join-Path $src 'Last Version') -Raw -ErrorAction SilentlyContinue).Trim() }
    } catch {}
    if (Test-Path $metaFile) {
        $meta = $null
        try { $meta = Get-Content $metaFile -Raw | ConvertFrom-Json } catch {}
        if ($meta -and $meta.schemaVersion -ne 1) {
            Write-Step -Stage 'restore' -Message "快照 schemaVersion=$($meta.schemaVersion)，本工具仅支持 1：快照由不兼容版本生成，已中止恢复（请重新登录该账号并保存登录态）" -Status 'error'
            throw "快照 schemaVersion 不兼容（$($meta.schemaVersion) != 1）"
        }
    } else {
        Write-Step -Stage 'restore' -Message '快照缺少版本元数据（旧版本工具生成），已跳过 schemaVersion 校验' -Status 'warn'
    }

    # ② leveldb 完整性（CURRENT → MANIFEST 指向校验，逐 Profile）
    $hasCookies = $false
    foreach ($p in (Get-ChromiumProfileDirs -Base $src)) {
        $ldb = Join-Path $p.FullName 'Local Storage\leveldb'
        if (Test-Path $ldb) {
            $currentFile = Join-Path $ldb 'CURRENT'
            if (-not (Test-Path $currentFile)) {
                Write-Step -Stage 'restore' -Message "快照 $($p.Name)/Local Storage/leveldb 缺少 CURRENT 文件，疑似不完整/损坏，已中止恢复（请重新登录该账号并保存登录态）" -Status 'error'
                throw "快照 leveldb 缺少 CURRENT（槽位 $slotLabel，Profile $($p.Name)）"
            }
            $manifestName = ''
            try { $manifestName = (Get-Content $currentFile -Raw -ErrorAction SilentlyContinue).Trim() } catch {}
            if ($manifestName -and -not (Test-Path (Join-Path $ldb $manifestName))) {
                Write-Step -Stage 'restore' -Message "快照 $($p.Name)/Local Storage/leveldb CURRENT 指向的 $manifestName 缺失，疑似不完整/损坏，已中止恢复（请重新登录该账号并保存登录态）" -Status 'error'
                throw "快照 leveldb MANIFEST 缺失（槽位 $slotLabel，Profile $($p.Name)）"
            }
        }
        # ②b 登录 Cookie 存在性统计（新布局 <profile>\Network\；旧布局 <profile>\Cookies）
        if (Test-Path (Join-Path $p.FullName 'Network\Cookies')) { $hasCookies = $true }
        elseif (Test-Path (Join-Path $p.FullName 'Cookies')) { $hasCookies = $true }
    }
    if (-not $hasCookies) {
        Write-Step -Stage 'restore' -Message "快照内所有 Profile 均未检测到 Cookies 文件——该快照可能保存的是未登录状态，恢复后豆包将未登录" -Status 'warn'
    }

    # ③ 版本差异警告（不阻断）
    $currentVer = ''
    try {
        if (Test-Path "$($Script:TraeDataDir)\Last Version") { $currentVer = (Get-Content "$($Script:TraeDataDir)\Last Version" -Raw -ErrorAction SilentlyContinue).Trim() }
    } catch {}
    if ($snapshotVer -and $currentVer -and ($snapshotVer -ne $currentVer)) {
        Write-Step -Stage 'restore' -Message "豆包版本已从快照的 $snapshotVer 升级到 $currentVer：旧快照通常兼容，若恢复后登录异常请重新登录并保存登录态" -Status 'warn'
    }
}

# 快照 Local State 的 profile.last_used 指向的 Profile 不在快照内时（旧版快照只抓 Default、
# 而客户端活跃 Profile 已漂移到 Profile N），改写为快照内存在的 Profile（优先 Default）。
# 否则客户端启动会打开一个空白 Profile → 未登录（实测"切换成功但没登录"根因之一）。
# 仅在需要时改写（新代码快照的活跃 Profile 必在快照内，不触发 JSON 重写，零风险）。
function Repair-LocalStateActiveProfile {
    param([string]$UserdataDir, [string[]]$SnapshotProfiles)
    if (-not $SnapshotProfiles -or @($SnapshotProfiles).Count -eq 0) { return }
    $lsPath = Join-Path $UserdataDir 'Local State'
    if (-not (Test-Path $lsPath)) { return }
    try {
        # 必须显式 UTF-8：Chromium Local State 为无 BOM UTF-8，PS 5.1 默认按 ANSI/GBK 解码，
        # 中文昵称（如"周天伟"）的 UTF-8 字节被 GBK 成对吞并时会吃掉闭合引号 → JSON 解析必败
        # （实测 A 槽恢复每次都报"传入的对象无效，应为":"或"}""，改写活跃 Profile 指针被静默跳过）
        $j = Get-Content $lsPath -Raw -Encoding UTF8 | ConvertFrom-Json
        if (-not $j.profile) { return }
        $used = ''
        try { $used = [string]$j.profile.last_used } catch {}
        if (-not $used -or (@($SnapshotProfiles) -contains $used)) { return }
        $fallback = if (@($SnapshotProfiles) -contains 'Default') { 'Default' } else { [string]@($SnapshotProfiles)[0] }
        $j.profile.last_used = $fallback
        # last_active_profiles 同步过滤到快照内存在的 Profile，避免客户端恢复陈旧多开列表
        try {
            $lap = @($j.profile.last_active_profiles) | Where-Object { @($SnapshotProfiles) -contains $_ }
            if (@($lap).Count -gt 0) { $j.profile.last_active_profiles = @($lap) }
        } catch {}
        # 无 BOM UTF-8 写临时文件再替换（PS 5.1 Set-Content -Encoding UTF8 带 BOM，避免 Chromium 解析异常）
        $json = $j | ConvertTo-Json -Depth 100
        $tmp = "$lsPath.awtmp"
        [System.IO.File]::WriteAllText($tmp, $json, (New-Object System.Text.UTF8Encoding($false)))
        Move-Item $tmp $lsPath -Force
        Write-Step -Stage 'restore' -Message "快照活跃 Profile '$used' 不在快照内，已改写 Local State 指向 '$fallback'（防客户端启动打开空 Profile 未登录）" -Status 'info'
    } catch {
        Write-Step -Stage 'restore' -Message "Local State 活跃 Profile 校验/改写失败（忽略）: $_" -Status 'warn'
    }
}

function Restore-ChromiumProfile {
    param([string]$Slot)
    $src = Join-Path $Script:ProfilesDir $Slot
    if (-not (Test-Path $src)) {
        # 单代回滚保护：主槽不存在时回退用 .bak（上次覆盖前的旧快照）
        $bakDir = "$src.bak"
        if (Test-Path $bakDir) {
            Write-Step -Stage 'restore' -Message "账号 $Slot 主快照缺失，回退使用上一次覆盖前的备份（$Slot.bak）" -Status 'warn'
            $src = $bakDir
        } else {
            Write-Step -Stage 'restore' -Message "目标账号 $Slot 无快照，请先登录该账号并保存登录态" -Status 'error'
            throw "目标账号 $Slot 无快照"
        }
    }
    # C3：恢复前完整性校验（schemaVersion / leveldb CURRENT→MANIFEST / Cookies 存在性 / 版本差异警告）
    Test-SnapshotIntegrity -Path $src
    $dest = $Script:TraeDataDir
    if (-not (Test-Path $dest)) { New-Item -ItemType Directory -Path $dest -Force | Out-Null }
    $restored = 0

    # 顶层: Local State / saman_* / Last Version
    if (Copy-SnapshotItem -SrcPath "$src\Local State" -DestPath "$dest\Local State") { $restored++ }
    if (Copy-SnapshotItem -SrcPath "$src\saman_app_state" -DestPath "$dest\saman_app_state") { $restored++ }
    if (Copy-SnapshotItem -SrcPath "$src\saman_shell_db_storage" -DestPath "$dest\saman_shell_db_storage") { $restored++ }
    if (Copy-SnapshotItem -SrcPath "$src\Last Version" -DestPath "$dest\Last Version") { $restored++ }

    # 多 Profile 对称回写：快照里有哪些 Profile 就恢复哪些（旧版快照只有 Default 也适用）
    $snapProfiles = Get-ChromiumProfileDirs -Base $src
    foreach ($p in $snapProfiles) {
        $n = $p.Name
        # Cookies*（快照统一存于 <profile>\Network\；兼容旧布局 <profile>\Cookies*）
        $ckBase = if (Test-Path (Join-Path $p.FullName 'Network\Cookies')) { Join-Path $p.FullName 'Network' } else { $p.FullName }
        Get-ChildItem -Path $ckBase -Filter 'Cookies*' -File -ErrorAction SilentlyContinue | ForEach-Object {
            if (Copy-SnapshotItem -SrcPath $_.FullName -DestPath "$dest\$n\Network\$($_.Name)") { $restored++ }
        }
        if (Copy-SnapshotItem -SrcPath (Join-Path $p.FullName 'Local Storage\leveldb') -DestPath "$dest\$n\Local Storage\leveldb") { $restored++ }
        if (Copy-SnapshotItem -SrcPath (Join-Path $p.FullName 'Session Storage') -DestPath "$dest\$n\Session Storage") { $restored++ }
        if (Copy-SnapshotItem -SrcPath (Join-Path $p.FullName 'DoubaoStorage') -DestPath "$dest\$n\DoubaoStorage") { $restored++ }
        if (Copy-SnapshotItem -SrcPath (Join-Path $p.FullName 'Preferences') -DestPath "$dest\$n\Preferences") { $restored++ }
        # 建议: <profile>/saman_shell_db_storage（与 Backup-ChromiumProfile 白名单对称，快照没有则跳过）
        if (Copy-SnapshotItem -SrcPath (Join-Path $p.FullName 'saman_shell_db_storage') -DestPath "$dest\$n\saman_shell_db_storage") { $restored++ }
        # C4：快照内含 IndexedDB 时一并恢复（无论当前开关状态，保证快照内容完整回写）
        if (Test-Path (Join-Path $p.FullName 'IndexedDB')) {
            if (Copy-SnapshotItem -SrcPath (Join-Path $p.FullName 'IndexedDB') -DestPath "$dest\$n\IndexedDB") { $restored++ }
        }
    }

    # 快照活跃 Profile 指针修复（防客户端启动打开快照外的空 Profile → 未登录）
    Repair-LocalStateActiveProfile -UserdataDir $dest -SnapshotProfiles @($snapProfiles | ForEach-Object { $_.Name })

    Write-Step -Stage 'restore' -Message "已恢复账号 $Slot 的登录态 ($restored 项, $($snapProfiles.Count) 个 Profile)" -Status 'ok'
}

# ── authfile 布局快照（WorkBuddy，批次1）───────────────────────────────────
# 依据 workbuddy-product-design.md §3.3（M2 账号切换 authfile 布局）：
#   L1 必选  %LOCALAPPDATA%\CodeBuddyExtension\Data\Public\auth\workbuddy-desktop.info
#           （登录态明文 JSON；客户端启动时会重写并生成历史快照 workbuddy-desktop.<ts>.<pid>.<uuid>.info，
#             作交叉校验，不入快照槽）
#   L2 体验  ~\.workbuddy\storage\user-<uid>* 目录（用户级数据，随账号迁移）
#   元数据  slot\meta.json：uid / savedAt（供 Rust 端校验与账号池回填）
# 快照/恢复前客户端须已关闭（入口 Switch/SaveCurrentLogin 已先 Stop-Trae）。

$Script:WbAuthDir  = "$env:LOCALAPPDATA\CodeBuddyExtension\Data\Public\auth"
$Script:WbAuthFile = "$Script:WbAuthDir\workbuddy-desktop.info"

# 从 auth 文件 JSON 提取 uid（兼容 account.uid / uid / auth.account.uid 嵌套）
function Get-AuthFileUid {
    param([string]$Path)
    if (-not (Test-Path $Path)) { return $null }
    try {
        $j = Get-Content $Path -Raw -Encoding UTF8 | ConvertFrom-Json
        foreach ($v in @($j.account.uid, $j.uid, $j.auth.account.uid, $j.auth.uid)) {
            if ($v) { return [string]$v }
        }
    } catch {}
    return $null
}

function Backup-AuthFileProfile {
    param([string]$Slot)
    $dest = Join-Path $Script:ProfilesDir $Slot
    if (-not (Test-Path $Script:WbAuthFile)) {
        Write-Step -Stage 'backup' -Message "auth 文件不存在（可能从未登录）：$($Script:WbAuthFile)" -Status 'warn'
        return
    }
    # 单代回滚保护（对齐豆包：覆盖前挪 .bak）
    if (Test-Path $dest) {
        $bakDir = "$dest.bak"
        try {
            if (Test-Path $bakDir) { Remove-Item $bakDir -Recurse -Force -ErrorAction SilentlyContinue }
            Move-Item $dest $bakDir -Force -ErrorAction Stop
            Write-Step -Stage 'backup' -Message "原 $Slot 快照已备份到 $Slot.bak（可回滚一代）" -Status 'info'
        } catch {
            Write-Step -Stage 'backup' -Message "旧快照挪移失败（将直接覆盖）: $_" -Status 'warn'
        }
    }
    New-Item -ItemType Directory -Path (Join-Path $dest 'auth') -Force | Out-Null
    $copied = 0
    # L1 必选：auth 文件
    try {
        Copy-Item $Script:WbAuthFile (Join-Path $dest 'auth\workbuddy-desktop.info') -Force
        $copied++
        Write-Step -Stage 'backup' -Message 'L1 auth 文件已备份' -Status 'ok'
    } catch {
        Write-Step -Stage 'backup' -Message "auth 文件备份失败: $_" -Status 'error'
        throw "auth 文件备份失败"
    }
    # L2 体验：~\.workbuddy\storage\user-<uid>* 目录
    $uid = Get-AuthFileUid -Path $Script:WbAuthFile
    if ($uid) {
        $storageDir = Join-Path $Script:TraeDataDir 'storage'
        if (Test-Path $storageDir) {
            $userDirs = Get-ChildItem -Path $storageDir -Directory -ErrorAction SilentlyContinue |
                Where-Object { $_.Name -like "user-$uid*" }
            foreach ($d in $userDirs) {
                $target = Join-Path $dest ("storage\" + $d.Name)
                if (Test-Path $target) { Remove-Item $target -Recurse -Force -ErrorAction SilentlyContinue }
                New-Item -ItemType Directory -Path (Split-Path $target -Parent) -Force | Out-Null
                Copy-Item $d.FullName $target -Recurse -Force -ErrorAction SilentlyContinue
                if (Test-Path $target) { $copied++ }
            }
            if (@($userDirs).Count -gt 0) {
                Write-Step -Stage 'backup' -Message "L2 用户数据已备份（$(@($userDirs).Count) 个目录）" -Status 'ok'
            } else {
                Write-Step -Stage 'backup' -Message 'L2 用户数据目录不存在，跳过（首次登录前正常）' -Status 'skip'
            }
        }
    } else {
        Write-Step -Stage 'backup' -Message 'auth 文件中未能解析 uid（JSON 结构变化？），L2 跳过' -Status 'warn'
    }
    # 元数据
    try {
        $meta = [ordered]@{
            schemaVersion = 1
            layout        = 'authfile'
            app           = 'WorkBuddy'
            uid           = $uid
            savedAt       = (Get-Date -Format 'yyyy-MM-dd HH:mm:ss')
        }
        $meta | ConvertTo-Json -Compress | Set-Content -Path (Join-Path $dest 'meta.json') -Encoding UTF8
    } catch {
        Write-Step -Stage 'backup' -Message "meta.json 写入失败（不影响快照）: $_" -Status 'warn'
    }
    Write-Step -Stage 'backup' -Message "已备份当前登录态到 $Slot ($copied 项)" -Status 'ok'
}

function Restore-AuthFileProfile {
    param([string]$Slot)
    $src = Join-Path $Script:ProfilesDir $Slot
    if (-not (Test-Path $src)) {
        $bakDir = "$src.bak"
        if (Test-Path $bakDir) {
            Write-Step -Stage 'restore' -Message "账号 $Slot 主快照缺失，回退使用上一次覆盖前的备份（$Slot.bak）" -Status 'warn'
            $src = $bakDir
        } else {
            Write-Step -Stage 'restore' -Message "目标账号 $Slot 无快照，请先登录该账号并保存登录态" -Status 'error'
            throw "目标账号 $Slot 无快照"
        }
    }
    $authSrc = Join-Path $src 'auth\workbuddy-desktop.info'
    if (-not (Test-Path $authSrc)) {
        Write-Step -Stage 'restore' -Message "快照缺少 auth 文件，疑似不完整快照，已中止恢复" -Status 'error'
        throw "快照缺少 auth 文件（槽位 $Slot）"
    }
    $restored = 0
    # L1 必选：回写 auth 文件
    try {
        if (-not (Test-Path $Script:WbAuthDir)) { New-Item -ItemType Directory -Path $Script:WbAuthDir -Force | Out-Null }
        Copy-Item $authSrc $Script:WbAuthFile -Force
        $restored++
        Write-Step -Stage 'restore' -Message 'L1 auth 文件已恢复' -Status 'ok'
    } catch {
        Write-Step -Stage 'restore' -Message "auth 文件恢复失败: $_" -Status 'error'
        throw "auth 文件恢复失败"
    }
    # L2 体验：storage\user-* 目录对称回写
    $slotStorage = Join-Path $src 'storage'
    if (Test-Path $slotStorage) {
        $destStorage = Join-Path $Script:TraeDataDir 'storage'
        New-Item -ItemType Directory -Path $destStorage -Force | Out-Null
        Get-ChildItem -Path $slotStorage -Directory -ErrorAction SilentlyContinue | ForEach-Object {
            $target = Join-Path $destStorage $_.Name
            if (Test-Path $target) { Remove-Item $target -Recurse -Force -ErrorAction SilentlyContinue }
            Copy-Item $_.FullName $target -Recurse -Force -ErrorAction SilentlyContinue
            if (Test-Path $target) { $restored++ }
        }
        Write-Step -Stage 'restore' -Message 'L2 用户数据已恢复' -Status 'ok'
    }
    Write-Step -Stage 'restore' -Message "已恢复账号 $Slot 的登录态 ($restored 项)" -Status 'ok'
}

# 批次1：authfile 切换后轮询 account-snapshot.json.uid 确认（F-02 验收项，超时 30s）。
# 客户端启动后首次联网刷新快照；uid 与目标槽 meta.json 一致 = 切换真正生效（fail-open：超时仅警告）。
function Confirm-AuthFileSwitch {
    param([string]$Slot)
    $snapFile = Join-Path $Script:TraeDataDir 'storage\skeleton\account-snapshot.json'
    $metaFile = Join-Path (Join-Path $Script:ProfilesDir $Slot) 'meta.json'
    $expectUid = $null
    try { $expectUid = [string]((Get-Content $metaFile -Raw -Encoding UTF8 | ConvertFrom-Json).uid) } catch {}
    if (-not $expectUid) { $expectUid = $Slot }
    Write-Step -Stage 'verify' -Message '等待客户端刷新登录快照（最长 30 秒）…' -Status 'running'
    $deadline = (Get-Date).AddSeconds(30)
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Seconds 2
        if (Test-Path $snapFile) {
            try {
                $j = Get-Content $snapFile -Raw -Encoding UTF8 | ConvertFrom-Json
                $uid = $null
                foreach ($v in @($j.uid, $j.account.uid, $j.accountId)) { if ($v) { $uid = [string]$v; break } }
                if ($uid -eq $expectUid) {
                    Write-Step -Stage 'verify' -Message '登录身份已确认为目标账号' -Status 'ok'
                    return
                }
            } catch {}
        }
    }
    Write-Step -Stage 'verify' -Message '30 秒内未确认到目标 uid（客户端可能未启动/未联网），请打开客户端核实' -Status 'warn'
}

function Backup-CurrentProfile {
    param([string]$Slot)
    # 批次1：authfile 布局（WorkBuddy）走 auth 文件 + 用户数据双层快照
    if ($Script:SnapshotLayout -eq 'authfile') {
        Backup-AuthFileProfile -Slot $Slot
        return
    }
    # P2：chromium 布局（豆包）走白名单目录级快照
    if ($Script:SnapshotLayout -eq 'chromium') {
        Backup-ChromiumProfile -Slot $Slot
        return
    }
    # F-48：精准白名单快照针对 icube 布局
    if ($Script:SnapshotLayout -ne 'icube') {
        Write-Step -Stage 'backup' -Message "$($Script:AppName) 布局为 '$($Script:SnapshotLayout)'，快照管线尚未接入（F-48 预留）" -Status 'error'
        throw "$($Script:AppName) 的快照备份尚未实现（布局=$($Script:SnapshotLayout)）"
    }
    $dest = Join-Path $Script:ProfilesDir $Slot
    if (-not (Test-Path $Script:TraeDataDir)) {
        Write-Step -Stage 'backup' -Message '当前数据目录不存在，跳过备份' -Status 'skip'
        return
    }
    if (-not (Test-Path $dest)) { New-Item -ItemType Directory -Path $dest -Force | Out-Null }
    $src = $Script:TraeDataDir
    $copied = 0

    # 精准备份：仅复制登录态关键文件（参考 traework-switcher）
    # 1. storage.json — 设备标识、遥测、认证信息
    $storageSrc = "$src\User\globalStorage\storage.json"
    if (Test-Path $storageSrc) { $dir = Split-Path "$dest\User\globalStorage\storage.json" -Parent; New-Item -ItemType Directory -Force -Path $dir | Out-Null; Copy-Item $storageSrc "$dest\User\globalStorage\storage.json" -Force; $copied++ }

    # 2. state.vscdb — 登录令牌数据库
    $stateDbSrc = "$src\User\globalStorage\state.vscdb"
    if (Test-Path $stateDbSrc) { $dir = Split-Path "$dest\User\globalStorage\state.vscdb" -Parent; New-Item -ItemType Directory -Force -Path $dir | Out-Null; Copy-Item $stateDbSrc "$dest\User\globalStorage\state.vscdb" -Force; $copied++ }
    $stateDbBak = "$src\User\globalStorage\state.vscdb.backup"
    if (Test-Path $stateDbBak) { Copy-Item $stateDbBak "$dest\User\globalStorage\state.vscdb.backup" -Force; $copied++ }

    # 3. machineid — 机器标识
    $machineIdSrc = "$src\machineid"
    if (Test-Path $machineIdSrc) { Copy-Item $machineIdSrc "$dest\machineid" -Force; $copied++ }

    # 4. aha\ — 设备认证数据
    $ahaSrc = "$src\aha"
    if (Test-Path $ahaSrc) { $ahaDest = "$dest\aha"; if (Test-Path $ahaDest) { Remove-Item $ahaDest -Recurse -Force -ErrorAction SilentlyContinue }; Copy-Item $ahaSrc $ahaDest -Recurse -Force -ErrorAction SilentlyContinue; $copied++ }

    # 5. Preferences / Local State
    if (Test-Path "$src\Preferences") { Copy-Item "$src\Preferences" "$dest\Preferences" -Force; $copied++ }
    if (Test-Path "$src\Local State") { Copy-Item "$src\Local State" "$dest\Local State" -Force; $copied++ }

    # 6. Local Storage\leveldb + config.db
    $lsSrc = "$src\Local Storage\leveldb"
    if (Test-Path $lsSrc) { $lsDest = "$dest\Local Storage\leveldb"; New-Item -ItemType Directory -Force -Path $lsDest | Out-Null; Copy-Item "$lsSrc\*" $lsDest -Force -ErrorAction SilentlyContinue; $copied++ }
    $lsConfig = "$src\Local Storage\config.db"
    if (Test-Path $lsConfig) { $lsParent = "$dest\Local Storage"; if (-not (Test-Path $lsParent)) { New-Item -ItemType Directory -Force -Path $lsParent | Out-Null }; Copy-Item $lsConfig "$lsParent\config.db" -Force; $copied++ }

    # 7. Network\
    $netSrc = "$src\Network"
    if (Test-Path $netSrc) { $netDest = "$dest\Network"; if (Test-Path $netDest) { Remove-Item $netDest -Recurse -Force -ErrorAction SilentlyContinue }; Copy-Item $netSrc $netDest -Recurse -Force -ErrorAction SilentlyContinue; $copied++ }

    # 8. Partitions\trae-webview + icube-web-crawler
    $wvSrc = "$src\Partitions\trae-webview"
    if (Test-Path $wvSrc) { $wvDest = "$dest\Partitions\trae-webview"; if (Test-Path $wvDest) { Remove-Item $wvDest -Recurse -Force -ErrorAction SilentlyContinue }; $wvParent = Split-Path $wvDest -Parent; New-Item -ItemType Directory -Force -Path $wvParent | Out-Null; Copy-Item $wvSrc $wvDest -Recurse -Force -ErrorAction SilentlyContinue; $copied++ }
    $icSrc = "$src\Partitions\icube-web-crawler-shared-session-v1.0"
    if (Test-Path $icSrc) { $icDest = "$dest\Partitions\icube-web-crawler-shared-session-v1.0"; if (Test-Path $icDest) { Remove-Item $icDest -Recurse -Force -ErrorAction SilentlyContinue }; $icParent = Split-Path $icDest -Parent; New-Item -ItemType Directory -Force -Path $icParent | Out-Null; Copy-Item $icSrc $icDest -Recurse -Force -ErrorAction SilentlyContinue; $copied++ }

    # 9. Session Storage\
    $ssSrc = "$src\Session Storage"
    if (Test-Path $ssSrc) { $ssDest = "$dest\Session Storage"; if (Test-Path $ssDest) { Remove-Item $ssDest -Recurse -Force -ErrorAction SilentlyContinue }; Copy-Item $ssSrc $ssDest -Recurse -Force -ErrorAction SilentlyContinue; $copied++ }

    Write-Step -Stage 'backup' -Message "已备份当前登录态到 $Slot ($copied 项)" -Status 'ok'
}

function Restore-Profile {
    param([string]$Slot)
    # 批次1：authfile 布局（WorkBuddy）走 auth 文件 + 用户数据双层恢复
    if ($Script:SnapshotLayout -eq 'authfile') {
        Restore-AuthFileProfile -Slot $Slot
        return
    }
    # P2：chromium 布局（豆包）走白名单目录级恢复
    if ($Script:SnapshotLayout -eq 'chromium') {
        Restore-ChromiumProfile -Slot $Slot
        return
    }
    # F-48：精准白名单恢复针对 icube 布局
    if ($Script:SnapshotLayout -ne 'icube') {
        Write-Step -Stage 'restore' -Message "$($Script:AppName) 布局为 '$($Script:SnapshotLayout)'，快照管线尚未接入（F-48 预留）" -Status 'error'
        throw "$($Script:AppName) 的快照恢复尚未实现（布局=$($Script:SnapshotLayout)）"
    }
    $src = Join-Path $Script:ProfilesDir $Slot
    if (-not (Test-Path $src)) {
        Write-Step -Stage 'restore' -Message "目标账号 $Slot 无快照，请先登录该账号并保存登录态" -Status 'error'
        throw "目标账号 $Slot 无快照"
    }
    $dest = $Script:TraeDataDir
    if (-not (Test-Path $dest)) { New-Item -ItemType Directory -Path $dest -Force | Out-Null }
    $restored = 0

    # 删除 code.lock 防止启动冲突
    $codeLock = "$dest\code.lock"
    if (Test-Path $codeLock) { Remove-Item $codeLock -Force -ErrorAction SilentlyContinue }

    # 精准恢复：仅恢复登录态关键文件（与 Backup-CurrentProfile 对称）
    # 1. storage.json
    if (Test-Path "$src\User\globalStorage\storage.json") { $dir = "$dest\User\globalStorage"; if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Force -Path $dir | Out-Null }; Copy-Item "$src\User\globalStorage\storage.json" "$dir\storage.json" -Force; $restored++ }

    # 2. state.vscdb + backup
    if (Test-Path "$src\User\globalStorage\state.vscdb") { $dir = "$dest\User\globalStorage"; if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Force -Path $dir | Out-Null }; Copy-Item "$src\User\globalStorage\state.vscdb" "$dir\state.vscdb" -Force; $restored++ }
    if (Test-Path "$src\User\globalStorage\state.vscdb.backup") { Copy-Item "$src\User\globalStorage\state.vscdb.backup" "$dest\User\globalStorage\state.vscdb.backup" -Force; $restored++ }

    # 3. machineid
    if (Test-Path "$src\machineid") { Copy-Item "$src\machineid" "$dest\machineid" -Force; $restored++ }

    # 4. aha\
    if (Test-Path "$src\aha") { $target = "$dest\aha"; if (Test-Path $target) { Remove-Item $target -Recurse -Force -ErrorAction SilentlyContinue }; Copy-Item "$src\aha" $target -Recurse -Force -ErrorAction SilentlyContinue; $restored++ }

    # 5. Preferences / Local State
    if (Test-Path "$src\Preferences") { Copy-Item "$src\Preferences" "$dest\Preferences" -Force; $restored++ }
    if (Test-Path "$src\Local State") { Copy-Item "$src\Local State" "$dest\Local State" -Force; $restored++ }

    # 6. Local Storage\leveldb + config.db
    if (Test-Path "$src\Local Storage\leveldb") { $target = "$dest\Local Storage\leveldb"; if (-not (Test-Path $target)) { New-Item -ItemType Directory -Force -Path $target | Out-Null } else { Remove-Item "$target\*" -Force -ErrorAction SilentlyContinue }; Copy-Item "$src\Local Storage\leveldb\*" $target -Force -ErrorAction SilentlyContinue; $restored++ }
    if (Test-Path "$src\Local Storage\config.db") { $target = "$dest\Local Storage"; if (-not (Test-Path $target)) { New-Item -ItemType Directory -Force -Path $target | Out-Null }; Copy-Item "$src\Local Storage\config.db" "$target\config.db" -Force; $restored++ }

    # 7. Network\
    if (Test-Path "$src\Network") { $target = "$dest\Network"; if (Test-Path $target) { Remove-Item $target -Recurse -Force -ErrorAction SilentlyContinue }; Copy-Item "$src\Network" $target -Recurse -Force -ErrorAction SilentlyContinue; $restored++ }

    # 8. Partitions\trae-webview + icube-web-crawler
    if (Test-Path "$src\Partitions\trae-webview") { $target = "$dest\Partitions\trae-webview"; if (Test-Path $target) { Remove-Item $target -Recurse -Force -ErrorAction SilentlyContinue }; $pParent = Split-Path $target -Parent; if (-not (Test-Path $pParent)) { New-Item -ItemType Directory -Force -Path $pParent | Out-Null }; Copy-Item "$src\Partitions\trae-webview" $target -Recurse -Force -ErrorAction SilentlyContinue; $restored++ }
    if (Test-Path "$src\Partitions\icube-web-crawler-shared-session-v1.0") { $target = "$dest\Partitions\icube-web-crawler-shared-session-v1.0"; if (Test-Path $target) { Remove-Item $target -Recurse -Force -ErrorAction SilentlyContinue }; $pParent = Split-Path $target -Parent; if (-not (Test-Path $pParent)) { New-Item -ItemType Directory -Force -Path $pParent | Out-Null }; Copy-Item "$src\Partitions\icube-web-crawler-shared-session-v1.0" $target -Recurse -Force -ErrorAction SilentlyContinue; $restored++ }

    # 9. Session Storage\
    if (Test-Path "$src\Session Storage") { $target = "$dest\Session Storage"; if (Test-Path $target) { Remove-Item $target -Recurse -Force -ErrorAction SilentlyContinue }; Copy-Item "$src\Session Storage" $target -Recurse -Force -ErrorAction SilentlyContinue; $restored++ }

    Write-Step -Stage 'restore' -Message "已恢复账号 $Slot 的登录态 ($restored 项)" -Status 'ok'
}

# ============ 入口 ============
try {
    if (-not $UserId -and $Action -ne 'ResetMachineId' -and $Action -ne 'ResetDeviceIds' -and $Action -ne 'KeepAlive') {
        Write-Step -Stage 'init' -Message '缺少 -UserId 参数' -Status 'error'
        exit 1
    }
    Write-Step -Stage 'init' -Message "开始操作: $Action (userId=$UserId, targetApp=$TargetApp → $($Script:AppName))" -Status 'info'

    switch ($Action) {
        'Switch' {
            # 预检查：目标账号是否有快照（在关闭 Trae 之前检查；主槽缺失时允许 .bak 回退槽）
            $targetProfile = Join-Path $Script:ProfilesDir $UserId
            if (-not (Test-Path $targetProfile) -and -not (Test-Path "$targetProfile.bak")) {
                Write-Step -Stage 'fatal' -Message "目标账号 $UserId 无快照，请先登录该账号并点击「保存当前登录态」" -Status 'error'
                exit 1
            }
            Stop-Trae
            # 保存当前登录态到 "last" 槽位（安全备份）
            Backup-CurrentProfile -Slot 'last'
            # 如果知道当前账号 ID，也备份到该账号的槽位（用于下次切回）。
            # 防误覆盖守卫：仅当桌面端检测到的当前登录（-ExpectedCurrentUid）与标记文件
            # 一致时才回写账号槽。不一致/未检测到 = 客户端很可能已手动重登或处于未登录态，
            # 此时把"当前态"刷进标记槽会把错误内容覆盖掉该账号的快照（实测 B 被覆盖根因），
            # 故跳过并警告——last 槽始终有完整备份可回退。
            $currentAcct = Get-CurrentAccount
            if ($currentAcct -and $currentAcct -ne $UserId) {
                if ($ExpectedCurrentUid -and $ExpectedCurrentUid -eq $currentAcct) {
                    Backup-CurrentProfile -Slot $currentAcct
                    Write-Step -Stage 'backup' -Message "当前账号 $currentAcct 的登录态已备份" -Status 'ok'
                } else {
                    Write-Step -Stage 'backup' -Message "检测到的当前登录（$(if ($ExpectedCurrentUid) { $ExpectedCurrentUid } else { '未识别或未检测到登录会话' })）与标记账号 $currentAcct 不一致，已跳过回写该账号槽位（当前态仍备份到 last），防止误覆盖" -Status 'warn'
                }
            }
            # 恢复目标账号的登录态（含设备标识）
            Restore-Profile -Slot $UserId
            # 记录当前账号 ID
            Set-CurrentAccount -AccountId $UserId
            Start-Trae
            if ($Script:SnapshotLayout -eq 'authfile') { Confirm-AuthFileSwitch -Slot $UserId }
            Write-Step -Stage 'done' -Message "已切换至账号 $UserId" -Status 'ok'
        }
        'SaveCurrentLogin' {
            # 保存当前登录态：关闭 Trae → 备份 → 启动
            Stop-Trae
            Backup-CurrentProfile -Slot $UserId
            Set-CurrentAccount -AccountId $UserId
            Start-Trae
            Write-Step -Stage 'done' -Message "已保存账号 $UserId 的当前登录态" -Status 'ok'
        }
        'ResetMachineId' {
            Reset-MachineId
            Write-Step -Stage 'done' -Message '机器码已重置' -Status 'ok'
        }
        'ResetDeviceIds' {
            Reset-DeviceIdsOnly
            Write-Step -Stage 'done' -Message '6 层设备标识重置完成' -Status 'ok'
        }
        'BackupCurrent' {
            Backup-CurrentProfile -Slot $UserId
            Set-CurrentAccount -AccountId $UserId
            Write-Step -Stage 'done' -Message '备份完成' -Status 'ok'
        }
        'RestoreOnly' {
            # 与 Switch 的区别：只做「以目标快照覆盖当前」，不把当前登录态备份到原账号槽——
            # 适合当前登录态无需保留（或原账号快照不愿被覆盖）的场景。当前登录态仍会
            # 先备份到 "last" 槽（安全回退），避免未保存的登录被直接覆盖丢失。
            Stop-Trae
            Backup-CurrentProfile -Slot 'last'
            Restore-Profile -Slot $UserId
            Set-CurrentAccount -AccountId $UserId
            Start-Trae
            Write-Step -Stage 'done' -Message "已恢复账号 $UserId 的登录态（当前登录态已备份到 last 槽）" -Status 'ok'
        }
        'KeepAlive' {
            # P3 豆包会话保活：sid_guard 30 天滑动续期由豆包客户端自己完成（cookie 值为客户端级
            # 加密，外部无法离线续写），本动作仅负责"启动→等待联网刷新→关闭"。
            $proc = Get-Process -Name $Script:ProcNames -ErrorAction SilentlyContinue
            if ($proc) {
                Write-Step -Stage 'keepalive' -Message "$($Script:AppName) 正在运行，客户端会话活跃，本次跳过" -Status 'ok'
                Write-Step -Stage 'done' -Message '保活检查完成（应用运行中）' -Status 'ok'
                exit 0
            }
            Start-Trae
            Write-Step -Stage 'keepalive' -Message '已启动，等待会话联网刷新（8 秒）' -Status 'running'
            Start-Sleep -Seconds 8
            Stop-Trae
            Write-Step -Stage 'done' -Message '保活完成（启动 8 秒 → 优雅关闭，sid_guard 已滑动续期）' -Status 'ok'
        }
    }
    exit 0
} catch {
    Write-Step -Stage 'fatal' -Message "失败: $_" -Status 'error'
    exit 1
}
