//! 子进程构建 helper（F-75 M0-0.2）：
//! 全仓所有对系统命令行工具（reg/certutil/schtasks/tasklist/taskkill/powershell/
//! networksetup/security/scutil/mdfind/open…）的调用必须经本模块构建，
//! 禁止再直接 `Command::new` + `creation_flags`。
//!
//! 背景：`std::os::windows::process::CommandExt` 仅存在于 Windows，
//! 直接散写 `creation_flags(0x08000000)` 会让 mac 构建在编译期失败（25 处/9 文件）。
//! 收敛后 `grep -r "CommandExt" src/` 只剩本文件一处。

/// 构建无窗口子进程：
/// - Windows：CREATE_NO_WINDOW（0x08000000）隐藏控制台，语义与既有散写完全一致；
/// - macOS：无控制台弹窗问题，原生即安静，直接返回。
pub fn sys_command(program: &str) -> std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut c = std::process::Command::new(program);
        c.creation_flags(0x08000000); // CREATE_NO_WINDOW
        c
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new(program)
    }
}

/// 构建无窗口子进程（exe **路径**版）：`Command::new` 接受任意可执行文件路径，
/// 平台标志语义与 `sys_command` 完全一致。典型消费方：启动客户端应用二进制
/// （qoder/common.rs `spawn_first_existing` 的手动路径候选）。
pub fn sys_command_path(program: &std::path::Path) -> std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut c = std::process::Command::new(program);
        c.creation_flags(0x08000000); // CREATE_NO_WINDOW
        c
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new(program)
    }
}

/// Windows 专用：构建无窗口子进程，先追加若干**常规**参数、再追加一条**原生**命令行
/// 参数（不经 MSVC 转义）。`raw_arg` 同属 Windows-only `CommandExt`，故一并圈进本模块——
/// 典型消费方：`cmd /c start "" "<url>"`（`/c` 常规参数 + URL 内嵌引号必须原样直达 cmd）。
/// 3.8.6 合并（issue #153 修复 4）：浏览器打开改 ShellExecuteW 后暂无生产消费方，
/// 保留工具备用（与下方 sys_output 同款 allow）
#[cfg(windows)]
#[allow(dead_code)]
pub fn sys_command_raw(program: &str, args: &[&str], raw: &str) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    let mut c = sys_command(program);
    c.args(args);
    c.raw_arg(raw);
    c
}

/// 运行命令并收集 stdout（utf-8 宽容解码 + 隐藏窗口）。
/// 失败（非零退出码 / 启动失败）返回 Err，错误尾带 stderr 内容。
/// Windows 构建下生产调用方均为 cfg(macos) 分支（cert_ctl/proxy_ctl/bypass），
/// 仅测试消费；mac 构建下有生产调用——allow 避免 Windows 构建误报
#[allow(dead_code)]
pub fn sys_output(cmd: &mut std::process::Command) -> Result<String, String> {
    let out = cmd.output().map_err(|e| format!("执行失败: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        return Err(format!(
            "退出码 {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(stdout.to_string())
}

/// macOS 专用：运行命令并在超时内等待退出（轮询 try_wait，超时 kill+收尸），
/// 返回退出码供调用方可观测。审查修复（Low）：`open` 走 LaunchServices 常态
/// <100ms，但异常挂起时 `.output()` 无超时会无限阻塞命令线程——本 helper 保留
/// 退出码校验能力的同时兜底超时（手法对齐 qoder_common 的 3s 子进程上限）
#[cfg(target_os = "macos")]
pub fn sys_status_timeout(
    cmd: &mut std::process::Command,
    timeout: std::time::Duration,
) -> Result<std::process::ExitStatus, String> {
    use std::time::{Duration, Instant};
    let mut child = cmd.spawn().map_err(|e| format!("执行失败: {e}"))?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("执行超时（>{timeout:?}），已强制终止"));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(format!("进程等待失败: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sys_command_builds_without_flags_leak() {
        // 两平台均应能构建并成功执行一个无参数命令（echo 经 cmd/shell 由调用方决定，
        // 这里仅验证构建与 output 通路）
        let mut c = sys_command(if cfg!(windows) { "cmd" } else { "echo" });
        if cfg!(windows) {
            c.args(["/C", "echo", "ok"]);
        } else {
            c.arg("ok"); // POSIX echo：参数直出
        }
        let out = sys_output(&mut c).unwrap();
        assert!(out.contains("ok"));
    }
}
