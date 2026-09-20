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
#[cfg(windows)]
fn name_matches(prof: &AppProfile, name: &str) -> bool {
    let base = strip_exe_suffix(name);
    prof.proc_names.iter().any(|n| base.eq_ignore_ascii_case(n))
}

/// 剥离映像名尾部的 ".exe"（大小写不敏感；无后缀原样返回）。
/// 用 get 防多字节字符下标越界（非字符边界时 get 返回 None → 原样返回）。
#[cfg(windows)]
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

/// M-1 侦察 ⑥（2026-09-20 真机实测）：mac 四应用主进程可执行名均为 "Electron"
///（<App>.app/Contents/MacOS/Electron），映像名精确匹配恒不命中且会跨应用串台。
/// 主进程判定改按 exe 路径 bundle 段：含 `/<App>.app/Contents/MacOS/`——
/// Helper 进程位于 `Contents/Frameworks/<X> Helper*.app/` 下不会命中（仅主进程），
/// SIGTERM 主进程后 Helper 随主退出（与 Windows 仅关停主 exe 的语义一致）。
#[cfg(target_os = "macos")]
fn mac_main_matches(prof: &AppProfile, p: &sysinfo::Process) -> bool {
    p.exe()
        .map(|e| {
            let s = e.to_string_lossy().to_lowercase();
            prof.proc_names
                .iter()
                .any(|n| s.contains(&format!("/{}.app/contents/macos/", n.to_lowercase())))
        })
        .unwrap_or(false)
}

/// 进程匹配分派：Windows 按映像名（剥 .exe），macOS 按主 bundle 路径段
fn proc_matches(prof: &AppProfile, p: &sysinfo::Process) -> bool {
    #[cfg(windows)]
    {
        name_matches(prof, &p.name().to_string_lossy())
    }
    #[cfg(target_os = "macos")]
    {
        mac_main_matches(prof, p)
    }
}

/// 枚举目标应用进程（精确映像名匹配）。返回 (pid, exe 全路径)——
/// Stop 前缓存 exe、Find-TraeExe 第 5 级复用。
pub fn list_procs(prof: &AppProfile) -> Vec<(u32, Option<std::path::PathBuf>)> {
    let sys = snapshot();
    sys.processes()
        .iter()
        .filter(|(_, p)| proc_matches(prof, p))
        .map(|(pid, p)| (pid.as_u32(), p.exe().map(|e| e.to_path_buf())))
        .collect()
}

pub fn is_running(sess: &Session) -> bool {
    !list_procs(&sess.prof).is_empty()
}

/// exe 发现第 5 级专用（PS 322-333）：proc_patterns 通配组命中（大小写不敏感，
/// PS Get-Process -Name 'Trae*'/'TRAE*' 语义）的进程 → 取 exe 全路径（首个命中）。
/// 与 list_procs 的精确 proc_names 组双轨并存（PS 同款：Find 用 ProcPatterns、
/// Stop 用 ProcNames）。mac 分支（M-1 ⑥）：映像名通配会命中 Helper 进程
///（"Trae CN Helper" ∈ "Trae*"）→ 归一回 Helper 内层 bundle 被白名单拒绝、发现
/// 失准——改用主 bundle 路径段匹配（与 list_procs 同源，仅主进程）。
pub fn running_exe_of(prof: &AppProfile) -> Option<std::path::PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let sys = snapshot();
        return sys
            .processes()
            .iter()
            .filter(|(_, p)| mac_main_matches(prof, p))
            .find_map(|(_, p)| p.exe().map(|e| e.to_path_buf()));
    }
    #[cfg(not(target_os = "macos"))]
    {
        let sys = snapshot();
        sys.processes()
            .iter()
            .filter(|(_, p)| {
                let name = p.name().to_string_lossy();
                prof.proc_patterns.iter().any(|pat| super::glob_match_ci(pat, &name))
            })
            .find_map(|(_, p)| p.exe().map(|e| e.to_path_buf()))
    }
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

/// 按 PID 精确关闭（保活专用）：只处理给定 PID 集合中「仍存活且映像名匹配」的
/// 进程，绝不触碰运行中的其他实例——消除 keepalive「启动后 8 秒等待窗口内
/// 用户手动开启应用 → stop 连带误关」的 TOCTOU 风险，同时防 PID 复用误杀
///（PID 存在但映像名不匹配 → 视为原进程已退出，跳过）。
/// 三级关闭策略与 stop_app 一致：WM_CLOSE 优雅关闭 → 按 PID 强杀 → 等待退出。
/// Electron 主进程收到 WM_CLOSE 后其子进程随之正常退出（避免 leveldb 文件锁）。
pub fn stop_spawned(sess: &mut Session, sink: &dyn ProgressSink, pids: &[u32]) -> Result<(), String> {
    // stop 前重检（重执行一次 is_running 语义）：比对启动时刻记录的 PID 集合，
    // 只保留「仍存活且映像名匹配」的 PID——spawn 实例已自行退出则无需关闭
    let sys = snapshot();
    let alive: Vec<u32> = pids
        .iter()
        .copied()
        .filter(|pid| pid_alive_matching(&sys, *pid, &sess.prof))
        .collect();
    drop(sys);
    if alive.is_empty() {
        sink.step(
            "stop",
            super::StepStatus::Skip,
            &format!("本次启动的 {} 实例已自行退出，无需关闭", sess.prof.app_name),
        );
        return Ok(());
    }
    let pid_list = alive
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");
    sink.step(
        "stop",
        super::StepStatus::Running,
        &format!("正在关闭本次启动的 {}（PID {}）", sess.prof.app_name, pid_list),
    );

    // 一级：优雅关闭（仅投递 spawn PID 的窗口，不波及其他实例）
    // Windows：WM_CLOSE；mac：SIGTERM（语义同 stop_app 一级，Electron 正常 quit 落盘）
    #[cfg(windows)]
    for pid in &alive {
        post_wm_close(*pid);
    }
    #[cfg(target_os = "macos")]
    {
        let sys = snapshot();
        for pid in &alive {
            if let Some(p) = sys.process(sysinfo::Pid::from_u32(*pid)) {
                let _ = p.kill_with(sysinfo::Signal::Term);
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
    if !wait_gone_pids(&alive, Duration::from_secs(sess.prof.graceful_wait_secs)) {
        // 二级：仍有存活 PID → 按 PID 强杀（不使用映像名 kill_all，防误伤）
        sink.step("stop", super::StepStatus::Warn, "优雅关闭超时，按 PID 强制结束");
        kill_pids(&alive);
    }
    // 三级：等待完全退出 ≤5 秒
    if !wait_gone_pids(&alive, Duration::from_secs(5)) {
        sink.step(
            "stop",
            super::StepStatus::Error,
            "本次启动实例未在 5 秒内完全退出，可能仍有文件锁，请手动关闭后重试",
        );
    }
    Ok(())
}

/// PID 是否存活且身份匹配白名单（PID 复用防护：复用后身份不同 → 视为已退出）。
/// 经 proc_matches 平台分派：Windows 按映像名（原语义），mac 按主 bundle 路径段
fn pid_alive_matching(sys: &System, pid: u32, prof: &AppProfile) -> bool {
    sys.process(sysinfo::Pid::from_u32(pid))
        .map(|p| proc_matches(prof, p))
        .unwrap_or(false)
}

/// 在 deadline 内等待给定 PID 全部退出（1s 轮询）
fn wait_gone_pids(pids: &[u32], timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        let sys = snapshot();
        if !pids.iter().any(|pid| sys.process(sysinfo::Pid::from_u32(*pid)).is_some()) {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// 按 PID 强杀（sysinfo TerminateProcess；PID 已退出时静默跳过）
fn kill_pids(pids: &[u32]) {
    let sys = snapshot();
    for pid in pids {
        if let Some(p) = sys.process(sysinfo::Pid::from_u32(*pid)) {
            let _ = p.kill();
        }
    }
}

/// Start-Trae 对译（PS 460-473）：可选 --proxy-server 注入。
/// PS 双行输出对译：start error「未找到 X 安装路径，请在设置中指定」
/// + throw「未找到 X 可执行文件」→ 顶层 catch fatal「失败: …」（经 thrown 补发）。
pub fn start_app(sess: &mut Session, sink: &dyn ProgressSink) -> Result<(), String> {
    start_app_pid(sess, sink).map(|_| ())
}

/// start_app 的 PID 版（保活精确关闭用）：spawn 后返回主进程 PID，
/// 供 keepalive 流程在关闭阶段按 PID 精确命中本次自己启动的实例。
pub fn start_app_pid(sess: &mut Session, sink: &dyn ProgressSink) -> Result<u32, String> {
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
        // 分离启动：`open` 经 LaunchServices 异步拉起（spawn 到的是短命 open 进程，
        // 其 PID 无意义）——轮询等待真正的 Electron 主进程出现（≤5s）返回其 PID，
        // 保活 stop 阶段按该 PID 精确关闭；未观测到（启动极慢）返回 0，
        // stop_spawned 的存活重检（mac_main_matches）对 0 恒 false → 自然 Skip
        cmd.spawn().map_err(|e| {
            super::thrown(sink, &format!("启动 {} 失败: {e}", sess.prof.app_name))
        })?;
        for _ in 0..10 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            if let Some((pid, _)) = list_procs(&sess.prof).first() {
                return Ok(*pid);
            }
        }
        Ok(0)
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
        // 分离启动（对齐 PS Start-Process，不等待）；启动失败 throw → 顶层 catch fatal。
        // 记录 spawn 主进程 PID：Electron 主进程关闭时其子进程（GPU/renderer 等）随之退出，
        // 关闭阶段按此 PID 精确命中本次实例。
        let child = cmd.spawn().map_err(|e| {
            super::thrown(sink, &format!("启动 {} 失败: {e}", sess.prof.app_name))
        })?;
        Ok(child.id())
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
        if proc_matches(prof, p) {
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

    #[cfg(windows)]
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

    // ── PID 精确关闭（保活优化）测试 ─────────────────────────────────────────
    // 优化点：stop_spawned 只处理「仍存活且映像名匹配」的 spawn PID，
    // 消除 8s 等待窗口 TOCTOU + PID 复用误杀；以下测试锁定双重防护语义。

    use crate::switcher::{Action, RunArgs, StepStatus};

    /// 收集步骤的内存 Sink（与 switcher::mod tests 的 MemSink 同构，模块私有不可复用）
    struct MemSink {
        steps: std::sync::Mutex<Vec<(String, String, String)>>,
    }
    impl MemSink {
        fn new() -> MemSink {
            MemSink { steps: std::sync::Mutex::new(Vec::new()) }
        }
    }
    impl ProgressSink for MemSink {
        fn step(&self, stage: &str, status: StepStatus, message: &str) {
            self.steps
                .lock()
                .unwrap()
                .push((stage.to_string(), status.as_str().to_string(), message.to_string()));
        }
    }

    /// 构造豆包会话（data_dir 用临时目录，本组测试不做文件 IO）
    fn doubao_session() -> Session {
        Session::new(&RunArgs {
            action: Action::KeepAlive,
            target_app: TargetApp::Doubao,
            user_id: None,
            proxy_port: None,
            include_indexeddb: false,
            expected_current_uid: String::new(),
            data_dir: std::env::temp_dir(),
        })
    }

    #[test]
    fn pid_alive_matching_不存在pid返回false() {
        // u32::MAX - 1 实际不存在：sys.process 返回 None → false
        let sys = snapshot();
        assert!(!pid_alive_matching(&sys, u32::MAX - 1, &prof(TargetApp::Doubao)));
    }

    #[test]
    fn pid_alive_matching_pid复用映像名不匹配返回false() {
        // 本测试进程 PID 真实存在，但映像名不在豆包白名单内：
        // 复现「PID 复用 → 原实例已退出」防护路径，必须判 false（防误杀）
        let sys = snapshot();
        assert!(!pid_alive_matching(&sys, std::process::id(), &prof(TargetApp::Doubao)));
    }

    #[cfg(windows)] // mac 身份信号是 exe 路径 bundle 段（不可对测试进程伪造），对测见下
    #[test]
    fn pid_alive_matching_白名单命中存活pid返回true() {
        // 白名单临时替换为本测试进程映像名（leak 成 'static 以适配
        // &'static [&'static str]）：验证「存活 + 匹配」正路径
        let stem = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .expect("无法获取测试进程映像名");
        let leaked: &'static str = Box::leak(stem.into_boxed_str());
        let whitelist: &'static [&'static str] = Box::leak(vec![leaked].into_boxed_slice());
        let mut p = prof(TargetApp::Doubao);
        p.proc_names = whitelist;
        let sys = snapshot();
        assert!(pid_alive_matching(&sys, std::process::id(), &p));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn pid_alive_matching_mac_bundle段命中存活pid返回true() {
        // mac 正路径对测（M-1 ⑥：映像名均为 Electron，身份信号是路径段）：
        // 以真实常驻的 Finder（/System/.../Finder.app/Contents/MacOS/Finder）
        // 验证「存活 + bundle 段匹配」；Finder 未运行（极端环境）则跳过
        let leaked: &'static str = Box::leak("Finder".to_string().into_boxed_str());
        let whitelist: &'static [&'static str] = Box::leak(vec![leaked].into_boxed_slice());
        let mut p = prof(TargetApp::Doubao);
        p.proc_names = whitelist;
        let sys = snapshot();
        let Some(pid) = sys
            .processes()
            .values()
            .find(|pr| {
                pr.exe()
                    .map(|e| {
                        e.to_string_lossy().to_lowercase().contains("/finder.app/contents/macos/")
                    })
                    .unwrap_or(false)
            })
            .map(|pr| pr.pid().as_u32())
        else {
            return;
        };
        assert!(pid_alive_matching(&sys, pid, &p));
    }

    #[test]
    fn wait_gone_pids_不存在pid立即返回true() {
        // 第一轮枚举即无命中 → 立即 true，不做 1s 轮询空转
        assert!(wait_gone_pids(&[u32::MAX - 1, u32::MAX - 2], Duration::from_secs(5)));
    }

    #[test]
    fn wait_gone_pids_存活pid超时返回false() {
        // 自身进程在等待期间必然存活 → 走满超时返回 false（负路径）
        assert!(!wait_gone_pids(&[std::process::id()], Duration::from_millis(100)));
    }

    #[test]
    fn stop_spawned_本次实例已全部退出时跳过关闭() {
        // spawn PID 已自行退出（用不存在的 PID 模拟）：alive 为空 →
        // 仅发一条 Skip 步骤且返回 Ok，绝不触碰运行中的其他实例
        let mut sess = doubao_session();
        let sink = MemSink::new();
        stop_spawned(&mut sess, &sink, &[u32::MAX - 1, u32::MAX - 2]).unwrap();
        let steps = sink.steps.lock().unwrap();
        assert_eq!(steps.len(), 1, "空 alive 集合只应有一条 Skip 步骤");
        assert_eq!(steps[0].0, "stop");
        assert_eq!(steps[0].1, "skip");
        assert!(steps[0].2.contains("已自行退出"), "实际消息: {}", steps[0].2);
    }
}
