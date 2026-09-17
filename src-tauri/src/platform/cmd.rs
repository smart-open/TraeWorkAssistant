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
    let mut c = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    c
}

/// Windows 专用：构建无窗口子进程，先追加若干**常规**参数、再追加一条**原生**命令行
/// 参数（不经 MSVC 转义）。`raw_arg` 同属 Windows-only `CommandExt`，故一并圈进本模块——
/// 典型消费方：`cmd /c start "" "<url>"`（`/c` 常规参数 + URL 内嵌引号必须原样直达 cmd）。
#[cfg(windows)]
pub fn sys_command_raw(program: &str, args: &[&str], raw: &str) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    let mut c = sys_command(program);
    c.args(args);
    c.raw_arg(raw);
    c
}

/// 运行命令并收集 stdout（utf-8 宽容解码 + 隐藏窗口）。
/// 失败（非零退出码 / 启动失败）返回 Err，错误尾带 stderr 内容。
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
        }
        let out = sys_output(&mut c).unwrap();
        assert!(out.contains("ok"));
    }
}
