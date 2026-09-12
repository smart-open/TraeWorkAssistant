<#
.SYNOPSIS
    src-ps/trae-switch-bridge.ps1 入口安全/健壮性黑盒测试（Pester v3.4 / v5 双兼容）。

.DESCRIPTION
    采用黑盒子进程方式（powershell -NoProfile -File <bridge> ... -Json），
    解析 stdout NDJSON 的 stage/status 断言，全部用例均不触发真实进程关闭或真实快照写入：
      - 子进程环境注入 AIWORKDATA_DIR=<临时目录>，日志/快照全部落在临时目录；
      - UserId 守卫用例在脚本动作分发之前即 fatal 早退；
      - 合法格式 UserId 用例依赖 Switch 的「无快照」预检（先于 Stop-Trae）。

    运行命令（盒装 Pester 3.4）：
      powershell -NoProfile -ExecutionPolicy Bypass -Command "Invoke-Pester -Script d:\code\AIWorkAssistant\tests\ps\trae-switch-bridge.Tests.ps1 -EnableExit"
    运行命令（Pester 5.x）：
      powershell -NoProfile -ExecutionPolicy Bypass -Command "Invoke-Pester -Path d:\code\AIWorkAssistant\tests\ps\trae-switch-bridge.Tests.ps1 -CI"

    注意：不可 dot-source 被测脚本做函数级单测——其入口 try 块位于脚本顶层（约 :1186），
    dot-source 会直接执行主流程（且 -Action 为 Mandatory，会交互式提示）。
#>

Describe 'trae-switch-bridge 入口校验（黑盒进程）' {

    BeforeAll {
        $script:BridgePath = (Resolve-Path (Join-Path $PSScriptRoot '..\..\src-ps\trae-switch-bridge.ps1')).Path

        # 每次运行使用全新临时数据目录：隔离日志/快照写入，避免真实副作用
        $script:TestDataDir = Join-Path ([System.IO.Path]::GetTempPath()) ('aiwork-bridge-tests-' + [guid]::NewGuid().ToString('N'))
        New-Item -ItemType Directory -Path $script:TestDataDir -Force | Out-Null

        # 调用桥脚本：黑盒子进程，30 秒超时强制 Kill
        function Invoke-Bridge {
            param(
                [string[]]$BridgeArgs = @(),
                [int]$TimeoutSeconds = 30
            )
            $psi = New-Object System.Diagnostics.ProcessStartInfo
            $psi.FileName = 'powershell.exe'
            $argLine = '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File "' + $script:BridgePath + '"'
            foreach ($a in $BridgeArgs) { $argLine += ' "' + ($a -replace '"', '\"') + '"' }
            $psi.Arguments = $argLine
            $psi.UseShellExecute = $false
            $psi.RedirectStandardOutput = $true
            $psi.RedirectStandardError = $true
            $psi.CreateNoWindow = $true
            $psi.StandardOutputEncoding = [System.Text.Encoding]::UTF8
            $psi.StandardErrorEncoding = [System.Text.Encoding]::UTF8
            # 隔离数据目录：日志/快照不落真实 %APPDATA%
            $psi.EnvironmentVariables['AIWORKDATA_DIR'] = $script:TestDataDir

            $proc = New-Object System.Diagnostics.Process
            $proc.StartInfo = $psi
            $null = $proc.Start()
            $stdoutTask = $proc.StandardOutput.ReadToEndAsync()
            $stderrTask = $proc.StandardError.ReadToEndAsync()
            if (-not $proc.WaitForExit($TimeoutSeconds * 1000)) {
                try { $proc.Kill() } catch {}
                throw "桥脚本执行超时（>$TimeoutSeconds 秒），已强制终止。参数: $($BridgeArgs -join ' ')"
            }
            [pscustomobject]@{
                ExitCode = $proc.ExitCode
                StdOut   = $stdoutTask.Result
                StdErr   = $stderrTask.Result
            }
        }

        # 取 stdout 中最后一行可解析的 NDJSON（stage/status/message/time）
        function Get-LastJsonLine {
            param([string]$Text)
            $lines = @($Text -split "`r?`n" | Where-Object { $_.Trim() })
            for ($i = $lines.Count - 1; $i -ge 0; $i--) {
                try {
                    $obj = $lines[$i] | ConvertFrom-Json
                    if ($obj.PSObject.Properties['stage'] -and $obj.PSObject.Properties['status']) { return $obj }
                } catch { }
            }
            return $null
        }
    }

    AfterAll {
        if ($script:TestDataDir -and (Test-Path $script:TestDataDir)) {
            Remove-Item $script:TestDataDir -Recurse -Force -ErrorAction SilentlyContinue
        }
    }

    Context 'UserId 白名单守卫（仅允许 A-Z a-z 0-9 _ -，长度 4~64）' {

        It '路径穿越 UserId 报 fatal 退出，且发生在任何进程操作之前' {
            $r = Invoke-Bridge @('-Action', 'Switch', '-UserId', '..\..\evil', '-Json')
            $r.ExitCode | Should Be 1
            $j = Get-LastJsonLine $r.StdOut
            $j | Should Not BeNullOrEmpty
            $j.stage  | Should Be 'fatal'
            $j.status | Should Be 'error'
            $j.message | Should Match 'UserId 参数格式非法'
            # 守卫先于动作分发：不应出现 init「开始操作」行，证明未进入任何进程/快照操作
            $r.StdOut | Should Not Match '开始操作'
        }

        It '含空格 UserId 报 fatal 退出' {
            $r = Invoke-Bridge @('-Action', 'Switch', '-UserId', 'a b', '-Json')
            $r.ExitCode | Should Be 1
            $j = Get-LastJsonLine $r.StdOut
            $j | Should Not BeNullOrEmpty
            $j.stage  | Should Be 'fatal'
            $j.status | Should Be 'error'
            $j.message | Should Match 'UserId 参数格式非法'
            $r.StdOut | Should Not Match '开始操作'
        }

        It '过短 UserId（2 位）报 fatal 退出' {
            $r = Invoke-Bridge @('-Action', 'Switch', '-UserId', 'ab', '-Json')
            $r.ExitCode | Should Be 1
            $j = Get-LastJsonLine $r.StdOut
            $j | Should Not BeNullOrEmpty
            $j.stage  | Should Be 'fatal'
            $j.status | Should Be 'error'
            $j.message | Should Match 'UserId 参数格式非法'
            $r.StdOut | Should Not Match '开始操作'
        }

        It '含注入字符 UserId（分号）报 fatal 退出' {
            $r = Invoke-Bridge @('-Action', 'Switch', '-UserId', 'user;rm', '-Json')
            $r.ExitCode | Should Be 1
            $j = Get-LastJsonLine $r.StdOut
            $j | Should Not BeNullOrEmpty
            $j.stage  | Should Be 'fatal'
            $j.status | Should Be 'error'
            $j.message | Should Match 'UserId 参数格式非法'
            $r.StdOut | Should Not Match '开始操作'
        }

        It '合法格式 UserId 不被格式守卫拒绝，因快照缺失在关进程之前 fatal 早退' {
            # 使用合法格式但确定不存在的 UserId；Switch 预检「无快照」先于 Stop-Trae，
            # 因此不会触发真实进程关闭/快照写入
            $r = Invoke-Bridge @('-Action', 'Switch', '-UserId', '1234567890123456', '-Json')
            $r.ExitCode | Should Be 1
            $r.StdOut | Should Not Match 'UserId 参数格式非法'
            $j = Get-LastJsonLine $r.StdOut
            $j | Should Not BeNullOrEmpty
            $j.stage  | Should Be 'fatal'
            $j.message | Should Match '无快照'
            # 未走到备份/恢复/进程阶段
            $r.StdOut | Should Not Match '"stage":"backup"'
            $r.StdOut | Should Not Match '"stage":"restore"'
            $r.StdOut | Should Not Match '"stage":"keepalive"'
        }
    }

    Context '必需参数校验与无效 Action' {

        It '-Action Switch 缺少 -UserId 时报错退出，且早于主流程' {
            $r = Invoke-Bridge @('-Action', 'Switch', '-Json')
            $r.ExitCode | Should Be 1
            $j = Get-LastJsonLine $r.StdOut
            $j | Should Not BeNullOrEmpty
            $j.stage  | Should Be 'init'
            $j.status | Should Be 'error'
            $j.message | Should Match '缺少 -UserId 参数'
            $r.StdOut | Should Not Match '开始操作'
        }

        It '-Action 缺失（Mandatory）时参数绑定失败，退出码非 0 且无 NDJSON' {
            $r = Invoke-Bridge @('-UserId', 'abcd1234', '-Json')
            $r.ExitCode | Should Not Be 0
            $j = Get-LastJsonLine $r.StdOut
            $j | Should BeNullOrEmpty
            $r.StdErr | Should Not BeNullOrEmpty
        }

        It '-Action 传无效值时 ValidateSet 拒绝，退出码非 0 且无 NDJSON' {
            $r = Invoke-Bridge @('-Action', 'Hack', '-UserId', 'abcd1234', '-Json')
            $r.ExitCode | Should Not Be 0
            $j = Get-LastJsonLine $r.StdOut
            $j | Should BeNullOrEmpty
            $r.StdErr | Should Not BeNullOrEmpty
        }
    }
}
