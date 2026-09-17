//! 进程管理（原 PS Stop-Trae / Start-Trae 对译）：
//! 三级关闭 = 优雅关闭（EnumWindows→PostMessageW(WM_CLOSE)，让 Electron 正常落盘
//! 避免 leveldb/vscdb 文件锁）→ 强杀（TerminateProcess）→ 等待完全退出；
//! 启动支持 --proxy-server 注入（C1 一键以账号打开走代理抓包）。

use std::time::{Duration, Instant};

use sysinfo::{ProcessesToUpdate, System};

use super::{profile, profile::AppProfile, ProgressSink, Session};

/// sysinfo System 句柄持有成本高（快照全进程表），封装短生命周期枚举
fn snapshot() -> System {
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    sys
}

/// 进程名是否 ∈ 精确白名单（Get-Process -Name 语义，大小写不敏感）
/// 审查修复（实测 2026-09-15）：sysinfo（NtQuerySystemInformation ImageName）在
/// Windows 上返回的映像名**带 .exe 后缀**（如 "Doubao.exe"），而白名单为不带后缀
/// 形态——精确比较恒不命中 → list_procs 恒空 → stop_app 恒报「未运行」从不关闭
/// 客户端，快照在运行中被覆盖 + 启动变多开（豆包/CodeBuddy/TRAE 全线「切换不生效」
/// 的根因）。匹配前统一剥离 .exe 后缀（大小写不敏感）。
fn name_matches(prof: &AppProfile, name: &str) -> bool {
    let base = strip_exe_suffix(name);
    prof.proc_names.iter().any(|n| base.eq_ignore_ascii_case(n))
}

/// 剥离映像名尾部的 ".exe"（大小写不敏感；无后缀原样返回）。
/// 用 get 防多字节字符下标越界（非字符边界时 get 返回 None → 原样返回）。
fn strip_exe_suffix(name: &str) -> &str {
    if name.len() > 4 {
        if let Some(base) = name.get(..name.len() - 4) {
            if name[base.len()..].eq_ignore_ascii_case(".exe") {
                return base;
            }
        }
    }
    name
}

/// 枚举目标应用进程（精确映像名匹配）。返回 (pid, exe 全路径)——
/// Stop 前缓存 exe、Find-TraeExe 第 5 级复用。
pub fn list_procs(prof: &AppProfile) -> Vec<(u32, Option<std::path::PathBuf>)> {
    let sys = snapshot();
    sys.processes()
        .iter()
        .filter(|(_, p)| name_matches(prof, &p.name().to_string_lossy()))
        .map(|(pid, p)| (pid.as_u32(), p.exe().map(|e| e.to_path_buf())))
        .collect()
}

pub fn is_running(sess: &Session) -> bool {
    !list_procs(&sess.prof).is_empty()
}

/// exe 发现第 5 级专用（PS 322-333）：proc_patterns 通配组命中（大小写不敏感，
/// PS Get-Process -Name 'Trae*'/'TRAE*' 语义）的进程 → 取 exe 全路径（首个命中）。
/// 与 list_procs 的精确 proc_names 组双轨并存（PS 同款：Find 用 ProcPatterns、
/// Stop 用 ProcNames）。
pub fn running_exe_of(prof: &AppProfile) -> Option<std::path::PathBuf> {
    let sys = snapshot();
    sys.processes()
        .iter()
        .filter(|(_, p)| {
            let name = p.name().to_string_lossy();
            prof.proc_patterns.iter().any(|pat| super::glob_match_ci(pat, &name))
        })
        .find_map(|(_, p)| p.exe().map(|e| e.to_path_buf()))
}

/// Stop-Trae 对译：三级关闭。
/// 自身/父进程排除逻辑不再需要：精确映像名匹配（TRAE SOLO CN.exe 等）永不命中
/// ai-work-assistant.exe；旧版进程名「Trae Work 助手」的兼容排除为死代码，删除
///（8.2 #2 有意差异）。
pub fn stop_app(sess: &mut Session, sink: &dyn ProgressSink) -> Result<(), String> {
    let procs = list_procs(&sess.prof);
    if procs.is_empty() {
        sink.step("stop", super::StepStatus::Skip, &format!("{} 未运行", sess.prof.app_name));
        // 进程未运行时也尝试查找 exe 路径并缓存（PS 450-457）
        if sess.exe_cache.is_none() {
            sess.exe_cache = super::locate::find_exe(sess);
        }
        return Ok(());
    }
    sink.step(
        "stop",
        super::StepStatus::Running,
        &format!("正在关闭 {}", sess.prof.app_name),
    );

    // 在关闭前缓存 exe 路径，供 Start 复用（进程退出后第 5 级发现失效）。
    // sysinfo exe() 可能返回短名/大小写形态不同的同一路径——经 path_eq 规范化比对
    // 防重复覆盖缓存（规则 R4）
    if let Some((_, Some(exe))) = procs
        .iter()
        .find(|(_, e)| e.as_ref().map(|p| profile::exe_matches(p, &sess.prof)).unwrap_or(false))
    {
        let same = sess
            .exe_cache
            .as_ref()
            .map(|c| super::path_eq(c, exe))
            .unwrap_or(false);
        if !same {
            sess.exe_cache = Some(exe.clone());
        }
    }

    // 一级：优雅关闭 —— Windows：EnumWindows→PostMessageW(WM_CLOSE)，等价 PS
    // CloseMainWindow 的全窗口版（覆盖多窗口 Electron 应用）；macOS：SIGTERM
    // （Electron 收到后走正常 quit 流程，落盘语义等价 WM_CLOSE，待 M-1 侦察 4 实测验证）。
    // 等待 graceful_wait_secs
    #[cfg(windows)]
    for (pid, _) in &procs {
        post_wm_close(*pid);
    }
    #[cfg(target_os = "macos")]
    {
        let sys = snapshot();
        for (pid, _) in &procs {
            if let Some(p) = sys.process(sysinfo::Pid::from_u32(*pid)) {
                let _ = p.kill_with(sysinfo::Signal::Term); // SIGTERM
            }
        }
    }
    sink.step(
        "stop",
        super::StepStatus::Running,
        &format!(
            "已发送优雅关闭请求，等待进程退出（最长 {} 秒）",
            sess.prof.graceful_wait_secs
        ),
    );
    if !wait_gone(&sess.prof, Duration::from_secs(sess.prof.graceful_wait_secs)) {
        // 二级：仍有存活进程 → 强杀
        sink.step("stop", super::StepStatus::Warn, "优雅关闭超时，强制结束进程");
        kill_all(&sess.prof);
    }
    // 三级：等待完全退出 ≤5 秒（强杀后 handle 释放）
    if !wait_gone(&sess.prof, Duration::from_secs(5)) {
        sink.step(
            "stop",
            super::StepStatus::Error,
            "进程未在 5 秒内完全退出，可能仍有文件锁，请手动关闭后重试",
        );
    }
    Ok(())
}

/// Start-Trae 对译（PS 460-473）：可选 --proxy-server 注入。
/// PS 双行输出对译：start error「未找到 X 安装路径，请在设置中指定」
/// + throw「未找到 X 可执行文件」→ 顶层 catch fatal「失败: …」（经 thrown 补发）。
pub fn start_app(sess: &mut Session, sink: &dyn ProgressSink) -> Result<(), String> {
    let Some(exe) = super::locate::find_exe(sess) else {
        sink.step(
            "start",
            super::StepStatus::Error,
            &format!("未找到 {} 安装路径，请在设置中指定", sess.prof.app_name),
        );
        return Err(super::thrown(
            sink,
            &format!("未找到 {} 可执行文件", sess.prof.app_name),
        ));
    };

    // F-75 M1-1.2 macOS：不需要 exe 路径——`open <bundle>` 让 LaunchServices 处理
    // （单实例、激活前台，天然正确）；代理注入经 `--args` 透传给 Electron 主进程。
    // exe 在 mac 定位链上即 .app bundle 路径（locate.rs find_bundle）。
    #[cfg(target_os = "macos")]
    {
        let mut cmd = crate::platform::cmd::sys_command("open");
        cmd.arg(&exe);
        if let Some(port) = sess.launch_proxy_port.filter(|p| *p > 0) {
            sink.step(
                "start",
                super::StepStatus::Running,
                &format!(
                    "正在启动 {}（注入代理 127.0.0.1:{port}）: {}",
                    sess.prof.app_name,
                    exe.display()
                ),
            );
            cmd.args(["--args", &format!("--proxy-server=http://127.0.0.1:{port}")]);
        } else {
            sink.step(
                "start",
                super::StepStatus::Running,
                &format!("正在启动 {}: {}", sess.prof.app_name, exe.display()),
            );
        }
        return cmd.spawn().map(|_| ()).map_err(|e| {
            super::thrown(sink, &format!("启动 {} 失败: {e}", sess.prof.app_name))
        });
    }

    #[cfg(not(target_os = "macos"))]
    {
        let mut cmd = std::process::Command::new(&exe);
        if let Some(port) = sess.launch_proxy_port.filter(|p| *p > 0) {
            sink.step(
                "start",
                super::StepStatus::Running,
                &format!(
                    "正在启动 {}（注入代理 127.0.0.1:{port}）: {}",
                    sess.prof.app_name,
                    exe.display()
                ),
            );
            // 规则 R1：参数数组传值（标准库按 MSVCRT 规则自动加引号，空格路径安全），
            // 禁止手拼命令行字符串
            cmd.arg(format!("--proxy-server=http://127.0.0.1:{port}"));
        } else {
            sink.step(
                "start",
                super::StepStatus::Running,
                &format!("正在启动 {}: {}", sess.prof.app_name, exe.display()),
            );
        }
        // 分离启动（对齐 PS Start-Process，不等待）；启动失败 throw → 顶层 catch fatal
        cmd.spawn().map_err(|e| {
            super::thrown(sink, &format!("启动 {} 失败: {e}", sess.prof.app_name))
        })?;
        Ok(())
    }
}

/// 在 deadline 内等待目标进程全部退出（1s 轮询，对齐 PS Start-Sleep 1s 循环）
fn wait_gone(prof: &AppProfile, timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        if list_procs(prof).is_empty() {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// 向 pid 的所有顶层窗口投递 WM_CLOSE（PS $proc.CloseMainWindow() 的全窗口版，
/// 覆盖多窗口 Electron 应用；投递不阻塞）。
/// F-75 M1-1.1：Windows 专属实现（windows-sys），mac 走上方 SIGTERM 分支。
#[cfg(windows)]
pub fn post_wm_close(pid: u32) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowThreadProcessId, PostMessageW, WM_CLOSE,
    };
    // windows-sys 0.59：LPARAM 为 isize 类型别名、BOOL 为 i32 类型别名
    unsafe extern "system" fn cb(hwnd: windows_sys::Win32::Foundation::HWND, lparam: isize) -> i32 {
        let pid_of_interest = lparam as u32;
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == pid_of_interest {
            PostMessageW(hwnd, WM_CLOSE, 0, 0);
        }
        1 // TRUE：继续枚举
    }
    unsafe { EnumWindows(Some(cb), pid as isize) };
}

/// 强杀全部白名单进程（sysinfo TerminateProcess，等价 PS Stop-Process -Force）
fn kill_all(prof: &AppProfile) {
    let sys = snapshot();
    for (_, p) in sys.processes() {
        if name_matches(prof, &p.name().to_string_lossy()) {
            let _ = p.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::switcher::TargetApp;

    fn prof(app: TargetApp) -> AppProfile {
        profile::profile_for(app, std::env::temp_dir().as_path())
    }

    #[test]
    fn name_matches_兼容sysinfo带exe后缀() {
        // 实测根因回归：sysinfo ImageName 带 .exe 后缀，白名单不带——必须能命中
        let p = prof(TargetApp::Doubao);
        assert!(name_matches(&p, "Doubao.exe"));
        assert!(name_matches(&p, "DOUBAO.EXE"));
        assert!(name_matches(&p, "Doubao"));
        assert!(!name_matches(&p, "DoubaoUpdate.exe"));
        let tw = prof(TargetApp::TraeWork);
        assert!(name_matches(&tw, "TRAE SOLO CN.exe"));
        assert!(name_matches(&tw, "Trae.exe"));
        // Trae 档案的白名单不含 Trae CN（防串台）
        let trae = prof(TargetApp::Trae);
        assert!(name_matches(&trae, "Trae CN.exe"));
        assert!(!name_matches(&trae, "TRAE SOLO CN.exe"));
        // 自身应用不应命中任何白名单
        let cb = prof(TargetApp::CodeBuddy);
        assert!(!name_matches(&cb, "ai-work-assistant.exe"));
    }

    #[test]
    fn list_procs_不误杀前缀相似进程() {
        // 本测试进程名（ai-work-assistant 或其测试变体）不在任何白名单内；
        // TraeWork 白名单含 "Trae"——若按前缀匹配会误命中 ai-work-assistant，
        // 精确匹配则不会
        let p = prof(TargetApp::TraeWork);
        let procs = list_procs(&p);
        // 自身测试进程绝不应命中任何档案白名单
        assert!(procs.iter().all(|(_, exe)| {
            exe.as_ref()
                .map(|e| profile::exe_matches(e, &p))
                .unwrap_or(true)
        }));
    }

    #[test]
    fn running_exe_of_通配组语义() {
        // Doubao 通配组 Doubao* 不应命中本测试进程
        assert!(running_exe_of(&prof(TargetApp::Doubao))
            .map_or(true, |p| profile::exe_matches(&p, &prof(TargetApp::Doubao))));
    }

    #[cfg(windows)]
    #[test]
    fn post_wm_close_对不存在pid无副作用() {
        post_wm_close(u32::MAX - 1);
    }
}
