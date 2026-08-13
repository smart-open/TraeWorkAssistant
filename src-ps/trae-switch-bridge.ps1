<#
.SYNOPSIS
    Trae Work 账号切换集成桥（非交互模式）
.DESCRIPTION
    供 Trae Work 助手（Tauri）调用的非交互切换层。
    封装「关闭 TRAE → 恢复目标账号登录态 → 重置机器码 → 启动 TRAE」流程，
    并以 NDJSON 逐行输出进度，供桌面端渲染步骤条。

    注意：本脚本是集成层，原 traework-switcher 的 TraeWorkAccountSwitcher.ps1
    仍作为参考实现保留；如需要可在此桥中复用其函数。

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
    [ValidateSet('Switch', 'ResetMachineId', 'BackupCurrent')]
    [string]$Action,

    [Parameter(Mandatory = $false)]
    [string]$UserId,

    [Parameter(Mandatory = $false)]
    [switch]$Json
)

# 注意：不要在此加 `#Requires -RunAsAdministrator`。
# 仅 Reset-MachineId 写 HKLM 需管理员；Switch/Backup 普通用户即可运行。
# 若强制要求管理员，普通权限启动的 App 调起脚本会直接 ScriptRequiresElevation 失败。

$ErrorActionPreference = 'Stop'

$Script:TraeDataDir = "$env:APPDATA\TRAE SOLO CN"
$Script:AppDataDir = "$env:APPDATA\TraeWorkAssistant"
$Script:ProfilesDir = "$Script:AppDataDir\profiles"
$Script:LogFile = "$Script:AppDataDir\logs\switcher.log"
$Script:_TraeExeCache = $null

function Find-TraeExe {
    if ($Script:_TraeExeCache) { return $Script:_TraeExeCache }

    # 1. 从 app_settings.json 读取用户自定义路径
    $settingsFile = Join-Path $Script:AppDataDir 'app_settings.json'
    if (Test-Path $settingsFile) {
        try {
            $settings = Get-Content $settingsFile -Raw | ConvertFrom-Json
            if ($settings.trae_path -and (Test-Path $settings.trae_path)) {
                $Script:_TraeExeCache = $settings.trae_path
                return $Script:_TraeExeCache
            }
        } catch {}
    }

    # 2. 多候选路径探测（与 Rust env.rs 保持一致）
    $candidates = @(
        "$env:LOCALAPPDATA\Programs\TRAE SOLO CN\TRAE SOLO CN.exe",
        "$env:LOCALAPPDATA\Programs\TRAE SOLO\TRAE SOLO.exe",
        "$env:ProgramFiles\TRAE SOLO CN\TRAE SOLO CN.exe",
        "$env:ProgramFiles\TRAE SOLO\TRAE SOLO.exe",
        "$env:LOCALAPPDATA\Programs\Trae\Trae.exe",
        "$env:ProgramFiles\Trae\Trae.exe"
    )
    # 也检查 D 盘等非系统盘
    if ($env:ProgramFiles -notlike 'D:\*') {
        $candidates += 'D:\Programs\TRAE SOLO CN\TRAE SOLO CN.exe'
    }
    foreach ($c in $candidates) {
        if (Test-Path $c) {
            $Script:_TraeExeCache = $c
            return $Script:_TraeExeCache
        }
    }

    # 3. 注册表回退
    try {
        $regKeys = @(
            'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*',
            'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*',
            'HKCU:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*'
        )
        foreach ($key in $regKeys) {
            $items = Get-ItemProperty $key -ErrorAction SilentlyContinue |
                Where-Object { $_.DisplayName -like '*TRAE*' -or $_.DisplayName -like '*Trae*' }
            foreach ($item in $items) {
                # 尝试 DisplayIcon
                if ($item.DisplayIcon) {
                    $iconPath = $item.DisplayIcon -replace ',', ''
                    $iconPath = $iconPath.Trim()
                    if (Test-Path $iconPath) {
                        $Script:_TraeExeCache = $iconPath
                        return $Script:_TraeExeCache
                    }
                }
                # 尝试 InstallLocation
                if ($item.InstallLocation) {
                    $loc = $item.InstallLocation.Trim()
                    $exeCandidates = @(
                        (Join-Path $loc 'TRAE SOLO CN.exe'),
                        (Join-Path $loc 'TRAE SOLO.exe'),
                        (Join-Path $loc 'Trae.exe')
                    )
                    foreach ($exe in $exeCandidates) {
                        if (Test-Path $exe) {
                            $Script:_TraeExeCache = $exe
                            return $Script:_TraeExeCache
                        }
                    }
                }
            }
        }
    } catch {}

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
        Add-Content -Path $Script:LogFile -Value "[$((Get-Date -Format 'yyyy-MM-dd HH:mm:ss'))] [$Stage] $Message"
    } catch {}
}

function Stop-Trae {
    # 兼容多种进程名
    $p = Get-Process -Name 'Trae*' -ErrorAction SilentlyContinue | Where-Object { $_.Name -match '^(Trae|TRAE.*CN.*)$' }
    if ($p) {
        Write-Step -Stage 'stop' -Message '正在关闭 Trae Work' -Status 'running'
        $p | Stop-Process -Force
        Start-Sleep -Seconds 2
    } else {
        Write-Step -Stage 'stop' -Message 'Trae Work 未运行' -Status 'skip'
    }
}

function Start-Trae {
    $exe = Find-TraeExe
    if (-not $exe) {
        Write-Step -Stage 'start' -Message '未找到 TRAE 安装路径，请在设置中指定' -Status 'error'
        throw '未找到 TRAE 可执行文件'
    }
    Write-Step -Stage 'start' -Message "正在启动 Trae Work: $exe" -Status 'running'
    Start-Process -FilePath $exe -WindowStyle Normal
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

function Backup-CurrentProfile {
    param([string]$Slot)
    $dest = Join-Path $Script:ProfilesDir $Slot
    if (Test-Path $Script:TraeDataDir) {
        if (-not (Test-Path $dest)) { New-Item -ItemType Directory -Path $dest -Force | Out-Null }
        $excludeDirs = @('Cache', 'Code Cache', 'GPUCache', 'Service Worker')
        Get-ChildItem -Path $Script:TraeDataDir | Where-Object { -not ($_.PSIsContainer -and $_.Name -in $excludeDirs) } | Copy-Item -Destination $dest -Recurse -Force
        Write-Step -Stage 'backup' -Message "已备份当前登录态到 $Slot" -Status 'ok'
    } else {
        Write-Step -Stage 'backup' -Message '当前数据目录不存在，跳过备份' -Status 'skip'
    }
}

function Restore-Profile {
    param([string]$Slot)
    $src = Join-Path $Script:ProfilesDir $Slot
    if (-not (Test-Path $src)) {
        # 首次切换该账号：以当前登录态作为它的初始快照
        Write-Step -Stage 'restore' -Message "目标账号无快照，使用当前登录态初始化" -Status 'info'
        Backup-CurrentProfile -Slot $Slot
        return
    }
    if (-not (Test-Path $Script:TraeDataDir)) { New-Item -ItemType Directory -Path $Script:TraeDataDir -Force | Out-Null }
    # 先清空现有，再写入目标快照
    try {
        Get-ChildItem -Path $Script:TraeDataDir -ErrorAction SilentlyContinue | Remove-Item -Recurse -Force -ErrorAction SilentlyContinue
        Copy-Item -Path (Join-Path $src '*') -Destination $Script:TraeDataDir -Recurse -Force
        Write-Step -Stage 'restore' -Message "已恢复账号 $Slot 的登录态" -Status 'ok'
    } catch {
        Write-Step -Stage 'restore' -Message "恢复登录态失败，已跳过: $_" -Status 'error'
    }
}

# ============ 入口 ============
try {
    if (-not $UserId -and $Action -ne 'ResetMachineId') {
        Write-Step -Stage 'init' -Message '缺少 -UserId 参数' -Status 'error'
        exit 1
    }
    Write-Step -Stage 'init' -Message "开始操作: $Action (userId=$UserId)" -Status 'info'

    switch ($Action) {
        'Switch' {
            Stop-Trae
            Backup-CurrentProfile -Slot 'last'
            Restore-Profile -Slot $UserId
            Reset-MachineId
            Start-Trae
            Write-Step -Stage 'done' -Message "已切换至账号 $UserId" -Status 'ok'
        }
        'ResetMachineId' {
            Reset-MachineId
            Write-Step -Stage 'done' -Message '机器码已重置' -Status 'ok'
        }
        'BackupCurrent' {
            Backup-CurrentProfile -Slot $UserId
            Write-Step -Stage 'done' -Message '备份完成' -Status 'ok'
        }
    }
    exit 0
} catch {
    Write-Step -Stage 'fatal' -Message "失败: $_" -Status 'error'
    exit 1
}
