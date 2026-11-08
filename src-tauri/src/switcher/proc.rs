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

/// 审查修复（M5/W5，2026-10-16 双客户端并存实测）：共用映像名 "CodeBuddy" 的
/// 归属仲裁（ImageArk 布尔等式 is_self = marker.exists() == owner_is_self）。
/// 返回 false = 本档案在当前安装布局下无权操作该映像（并存布局杀国际会误杀
/// 国内实例、老单布局杀国内会误杀国际实例），调用方必须跳过该进程。
/// 仅歧义映像名受仲裁；白名单内其余无歧义名（"CodeBuddy CN" 等）直通。
/// 四消费点统一接入：list_procs / running_exe_of / pid_alive_matching / kill_pids。
/// 2026-10-19：mac 构建首次可编译后显形——仲裁仅 Windows 映像名链路消费
///（mac 走 bundle 段 + 档案 mac_proc_names 收窄），按库内 cfg_attr 惯例静默
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn ark_allows(prof: &AppProfile, name: &str) -> bool {
    let Some(ark) = &prof.image_ark else {
        return true;
    };
    let base = strip_exe_suffix(name);
    if !ark.ambiguous.iter().any(|a| base.eq_ignore_ascii_case(a)) {
        return true;
    }
    ark.marker.exists() == ark.owner_is_self
}

/// 剥离映像名尾部的 ".exe"（大小写不敏感；无后缀原样返回）。
/// 用 get 防多字节字符下标越界（非字符边界时 get 返回 None → 原样返回）。
/// 跨平台（2026-10-18 合并修复）：ark_allows 无平台门控且引用本函数，
/// strip 门控会导致 macOS 编译失败——纯字符串操作，两平台通用。
/// 2026-10-19：mac 构建首次可编译后显形——mac 消费点均走 bundle 段信号，
/// 本函数仅 Windows 链路调用，按库内 cfg_attr 惯例静默
#[cfg_attr(target_os = "macos", allow(dead_code))]
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
fn mac_exe_is_main_of(p: &sysinfo::Process, name: &str) -> bool {
    p.exe()
        .map(|e| {
            e.to_string_lossy()
                .to_lowercase()
                .contains(&format!("/{}.app/contents/macos/", name.to_lowercase()))
        })
        .unwrap_or(false)
}

/// 档案级匹配：任一 proc_names（.app 主干名）命中即为主进程。
/// 2026-10-19 双客户端并存实测：优先消费档案 mac_proc_names（国内 CodeBuddy
/// 收窄为 ["CodeBuddy CN"]——裸名 "CodeBuddy" 段恰是并存的国际版
/// /Applications/CodeBuddy.app，不收窄 stop_app 会 SIGTERM 误杀国际版），
/// 未配置回落 proc_names
#[cfg(target_os = "macos")]
fn mac_main_matches(prof: &AppProfile, p: &sysinfo::Process) -> bool {
    prof.mac_proc_names
        .unwrap_or(prof.proc_names)
        .iter()
        .any(|n| mac_exe_is_main_of(p, n))
}

/// 进程匹配分派：Windows 按映像名（剥 .exe），macOS 按主 bundle 路径段
fn proc_matches(prof: &AppProfile, p: &sysinfo::Process) -> bool {
    #[cfg(windows)]
    {
        // M5/W5：白名单命中后仍须经 ark 归属仲裁（歧义映像名双客户端并存防误杀）
        let name = p.name().to_string_lossy();
        name_matches(prof, &name) && ark_allows(prof, &name)
    }
    #[cfg(target_os = "macos")]
    {
        // mac 主进程身份信号 = exe 路径 `/<App>.app/Contents/MacOS/` 段 + 档案
        // mac_proc_names 收窄。原注释「路径段天然消歧」对裸名不成立（2026-10-19
        // 实测："/codebuddy.app/" 恰是并存国际版 bundle，须按档案收窄）；mac 无
        // 共用映像名，无需 Windows 的 ark 仲裁
        mac_main_matches(prof, p)
    }
}

/// 枚举目标应用进程（精确映像名匹配）。返回 (pid, exe 全路径)——
/// Stop 前缓存 exe、Find-TraeExe 第 5 级复用。
pub fn list_procs(prof: &AppProfile) -> Vec<(u32, Option<std::path::PathBuf>)> {
    let sys = snapshot();
    sys.processes()
        .iter()
        // proc_matches 平台分派 + ark 仲裁（mac bundle 段 / Windows 映像名+ark）
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
                // M5/W5：glob 命中后仍须经映像归属仲裁（"CodeBuddy.exe" glob 在老单
                // 布局下会命中国内同名映像，国际档案不得取其 exe）
                prof.proc_patterns.iter().any(|pat| super::glob_match_ci(pat, &name)) && ark_allows(prof, &name)
            })
            .find_map(|(_, p)| p.exe().map(|e| e.to_path_buf()))
    }
}

// ── 环境检测用进程查询（零子进程）──────────────────────────────────────────

/// 进程映像名集合快照的复用时长：环境检测在「一轮界面切换」里会被概览页与顶栏各问
/// 一次（Buddy 一次切换实测 5 次），短 TTL 复用同一份进程表快照即可。
#[cfg(windows)]
const PROC_NAME_TTL: Duration = Duration::from_secs(2);

/// 全进程映像名集合（剥离 `.exe` 后缀、统一小写）。
/// 原实现用 `tasklist` 子进程逐次探测（单次实测 ~260ms），改为 sysinfo 枚举一次 +
/// 短 TTL 复用。注意：关闭/强杀轮询需实时快照，走 `list_procs`，不要用本缓存。
/// Windows 专属：mac 映像名均为 Electron（M-1 ⑥），进程检测走 bundle 路径段
/// 匹配（commands::process::mac_app_running），不走映像名白名单。
#[cfg(windows)]
pub fn running_name_set() -> std::sync::Arc<std::collections::HashSet<String>> {
    type Entry = (Instant, std::sync::Arc<std::collections::HashSet<String>>);
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Option<Entry>>> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(None));
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, set)) = guard.as_ref() {
        if at.elapsed() < PROC_NAME_TTL {
            return set.clone();
        }
    }
    let set: std::sync::Arc<std::collections::HashSet<String>> = std::sync::Arc::new(
        snapshot()
            .processes()
            .values()
            .map(|p| {
                let name = p.name().to_string_lossy();
                strip_exe_suffix(&name).to_ascii_lowercase()
            })
            .collect(),
    );
    *guard = Some((Instant::now(), set.clone()));
    set
}

/// 是否有任一目标进程在运行。环境检测/迁移守卫用，零子进程。
/// Windows：Get-Process -Name 语义（精确名、大小写不敏感、自动兼容 sysinfo 的
/// `.exe` 后缀形态）；macOS：映像名恒为 Electron（M-1 ⑥），`names` 视为 .app
/// bundle 主干名，按主进程 bundle 路径段匹配（与 mac_main_matches 同源）。
pub fn any_running(names: &[&str]) -> bool {
    #[cfg(windows)]
    {
        let set = running_name_set();
        names
            .iter()
            .any(|n| set.contains(strip_exe_suffix(n).to_ascii_lowercase().as_str()))
    }
    #[cfg(target_os = "macos")]
    {
        snapshot()
            .processes()
            .values()
            .any(|p| names.iter().any(|n| mac_exe_is_main_of(p, n)))
    }
}

/// 精确映像名命中的进程 exe 全路径（首个命中）：`Get-Process -Name @(...) | Select -First 1`
/// 的进程内等价实现（原 `env::process_exe_path` 走 powershell，单次 ~1.2s）。
/// 需要返回路径，故取实时快照，不复用只存名字的 `running_name_set`。
#[cfg(windows)]
pub fn running_exe_exact(names: &[&str]) -> Option<std::path::PathBuf> {
    snapshot()
        .processes()
        .values()
        .filter(|p| {
            let name = p.name().to_string_lossy();
            names
                .iter()
                .any(|want| strip_exe_suffix(&name).eq_ignore_ascii_case(strip_exe_suffix(want)))
        })
        .find_map(|p| p.exe().map(|e| e.to_path_buf()))
}

/// Stop-Trae 对译：三级关闭。
/// 自身/父进程排除逻辑不再需要：精确映像名匹配（Trae CN.exe 等）永不命中
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
    // L12 修复：优雅等待与强杀收窄到 stop 前捕获的 PID 集合（原 wait_gone/kill_all
    // 按「映像名全量匹配」——等待窗口内用户新启的同应用实例会被当作未退出的旧
    // 实例误杀；对齐 stop_spawned 的 PID 精确语义，含 PID 复用防护）
    let stop_pids: Vec<u32> = procs.iter().map(|(pid, _)| *pid).collect();
    if !wait_gone_pids(&sess.prof, &stop_pids, Duration::from_secs(sess.prof.graceful_wait_secs)) {
        // 二级：仍有存活进程 → 强杀（仅限捕获的 PID）
        sink.step("stop", super::StepStatus::Warn, "优雅关闭超时，强制结束进程");
        kill_pids(&sess.prof, &stop_pids);
    }
    // 三级：等待完全退出 ≤5 秒（强杀后 handle 释放）
    if !wait_gone_pids(&sess.prof, &stop_pids, Duration::from_secs(5)) {
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
    if !wait_gone_pids(&sess.prof, &alive, Duration::from_secs(sess.prof.graceful_wait_secs)) {
        // 二级：仍有存活 PID → 按 PID 强杀（不使用映像名 kill_all，防误伤）
        sink.step("stop", super::StepStatus::Warn, "优雅关闭超时，按 PID 强制结束");
        kill_pids(&sess.prof, &alive);
    }
    // 三级：等待完全退出 ≤5 秒
    if !wait_gone_pids(&sess.prof, &alive, Duration::from_secs(5)) {
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

/// 在 deadline 内等待给定 PID 全部退出（1s 轮询；PID 存在但身份不匹配视为已退出，
/// 与 pid_alive_matching 同口径——审查 L12 PID 复用防护：原进程退出后 PID 被
/// 无关进程复用时身份不匹配视同已退出，不再无谓等满超时）
fn wait_gone_pids(prof: &AppProfile, pids: &[u32], timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        let sys = snapshot();
        if !pids.iter().any(|pid| pid_alive_matching(&sys, *pid, prof)) {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// 按 PID 强杀（sysinfo TerminateProcess；审查 L12 PID 复用防护：PID 已退出或
/// 身份不匹配——复用为无关进程——时静默跳过，绝不误杀）
fn kill_pids(prof: &AppProfile, pids: &[u32]) {
    let sys = snapshot();
    for pid in pids {
        if !pid_alive_matching(&sys, *pid, prof) {
            continue;
        }
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
        // stop_spawned 的存活重检（mac_main_matches）对 0 恒 false → 自然 Skip。
        // L 修复：open 对已运行应用只做激活——先记录既有实例 PID，等待阶段仅认
        // 新出现的 PID（原实现返回既有 PID，stop_spawned 会关掉用户自己打开的应用）
        let pre_existing: std::collections::HashSet<u32> = list_procs(&sess.prof)
            .iter()
            .map(|(pid, _)| *pid)
            .collect();
        let mut child = cmd.spawn().map_err(|e| {
            super::thrown(sink, &format!("启动 {} 失败: {e}", sess.prof.app_name))
        })?;
        let mut spawned_pid = 0u32;
        for _ in 0..10 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            if let Some((pid, _)) = list_procs(&sess.prof)
                .iter()
                .find(|(pid, _)| !pre_existing.contains(pid))
            {
                spawned_pid = *pid;
                break;
            }
        }
        // 收尸（审查修复）：短命 open 进程退出后 spawn 句柄即弃会成为僵尸存活至
        // 应用退出——已退出 try_wait 直接收尸；仍存活（LaunchServices 异常挂起）
        // kill+wait 兜底防残留（对齐 cmd.rs 收尸红线）
        match child.try_wait() {
            Ok(Some(_)) => {}
            _ => {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        Ok(spawned_pid)
    }

    #[cfg(not(target_os = "macos"))]
    {
        // 收口（审查修复）：子进程经 platform::cmd::sys_command_path 构建——Windows
        // 自动获得 CREATE_NO_WINDOW（对齐 env.rs 客户端启动链口径）
        let mut cmd = crate::platform::cmd::sys_command_path(&exe);
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
        // 静默子进程 stdio：客户端（Electron）自身的弃用警告/RequestStore 日志
        // 不再混入应用控制台（客户端有自己的日志文件，排障不受影响）
        let child = cmd
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| {
                super::thrown(sink, &format!("启动 {} 失败: {e}", sess.prof.app_name))
            })?;
        Ok(child.id())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::switcher::TargetApp;

    fn prof(app: TargetApp) -> AppProfile {
        profile::profile_for(app, std::env::temp_dir().as_path())
    }

    #[cfg(windows)]
    #[test]
    fn image_ark_归属仲裁双向() {
        let _lock = crate::switcher::test_io_lock();
        let dir = std::env::temp_dir().join(format!("sw-ark-test-{}", std::process::id()));
        let marker = dir.join("CodeBuddy.exe");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 覆盖为测试专用 marker（档案默认 marker 指向本机真实安装路径，
        // 存在性随环境变化，测试必须确定性）
        let mk = |owner_is_self: bool| profile::ImageArk {
            marker: marker.clone(),
            owner_is_self,
            ambiguous: &["CodeBuddy"],
        };
        let mut intl = prof(TargetApp::CodeBuddyAI);
        intl.image_ark = Some(mk(true));
        let mut cn = prof(TargetApp::CodeBuddy);
        cn.image_ark = Some(mk(false));
        // 无仲裁档案（image_ark=None）恒允许
        assert!(ark_allows(&prof(TargetApp::Doubao), "Doubao.exe"));
        // 布局一：国际 exe 在盘（并存布局）——歧义映像 "CodeBuddy" 归国际版
        std::fs::write(&marker, b"").unwrap();
        assert!(ark_allows(&intl, "CodeBuddy.exe"));
        assert!(ark_allows(&intl, "codebuddy.EXE"));
        assert!(!ark_allows(&cn, "CodeBuddy"));
        // 无歧义名（国内 CN 映像）不受仲裁直通——并存时国内端仍可正常关闭
        assert!(ark_allows(&cn, "CodeBuddy CN.exe"));
        // 布局二：国际 exe 不在盘（老单布局）——歧义映像 "CodeBuddy" 归国内
        std::fs::remove_file(&marker).unwrap();
        assert!(!ark_allows(&intl, "CodeBuddy.exe"));
        assert!(ark_allows(&cn, "CodeBuddy"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // 2026-10-19 mac 测试链修复：本用例只锁 Windows 映像名语义（name_matches 为
    // cfg(windows) 专属），无门控导致 mac 测试构建恒失败（E0425），加门控对齐
    #[cfg(windows)]
    #[test]
    fn name_matches_兼容sysinfo带exe后缀() {
        // 实测根因回归：sysinfo ImageName 带 .exe 后缀，白名单不带——必须能命中
        let p = prof(TargetApp::Doubao);
        assert!(name_matches(&p, "Doubao.exe"));
        assert!(name_matches(&p, "DOUBAO.EXE"));
        assert!(name_matches(&p, "Doubao"));
        assert!(!name_matches(&p, "DoubaoUpdate.exe"));
        let trae = prof(TargetApp::Trae);
        assert!(name_matches(&trae, "Trae CN.exe"));
        // 白名单外（TRAE SOLO CN 随 Trae Work 退役）拒绝
        assert!(!name_matches(&trae, "TRAE SOLO CN.exe"));
        // 自身应用不应命中任何白名单
        let cb = prof(TargetApp::CodeBuddy);
        assert!(!name_matches(&cb, "ai-work-assistant.exe"));
    }

    #[test]
    fn list_procs_不误杀前缀相似进程() {
        // 本测试进程名（ai-work-assistant 或其测试变体）不在任何白名单内；
        // Trae 白名单 "Trae CN" 与其无交集——精确映像名匹配永不误命中
        let p = prof(TargetApp::Trae);
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
    fn any_running_命中自身且不误命中() {
        // 环境检测进程查询（原 tasklist 子进程）零子进程化的语义锁定：
        // 自身测试进程必命中；sysinfo 带 .exe 后缀形态同样命中；不存在名字不误命中
        let own = std::env::current_exe().expect("取当前测试进程路径");
        let stem = own
            .file_stem()
            .expect("测试进程应有文件名")
            .to_string_lossy()
            .to_string();
        assert!(any_running(&[&stem]), "自身进程应命中: {stem}");
        assert!(any_running(&[&format!("{stem}.exe")]), "带 .exe 后缀应命中");
        assert!(!any_running(&["aiwork-definitely-not-a-process"]));
    }

    #[cfg(windows)]
    #[test]
    fn running_name_set_ttl内复用同一份快照() {
        let a = running_name_set();
        let b = running_name_set();
        assert!(std::sync::Arc::ptr_eq(&a, &b));
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
            machine_id_override: None,
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
        // 第一轮枚举即无命中（PID 不存在 → 身份匹配 false）→ 立即 true，不做 1s 轮询空转
        assert!(wait_gone_pids(
            &prof(TargetApp::Doubao),
            &[u32::MAX - 1, u32::MAX - 2],
            Duration::from_secs(5)
        ));
    }

    // 负路径需「存活且身份匹配」的进程：win 用自身测试进程（白名单临时替换映像名）；
    // mac 身份信号是 exe 路径 bundle 段（测试进程不在 .app 内，不可伪造），
    // 以真实常驻的 Finder 对测（Finder 未运行的极端环境则跳过）
    #[cfg(windows)]
    #[test]
    fn wait_gone_pids_存活pid超时返回false() {
        // 身份匹配口径引入后，「存活」必须同时映像名匹配：白名单临时替换为本测试
        // 进程映像名（leak 成 'static 适配 &'static [&'static str]，同
        // pid_alive_matching_白名单命中存活pid返回true）
        let stem = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .expect("无法获取测试进程映像名");
        let leaked: &'static str = Box::leak(stem.into_boxed_str());
        let whitelist: &'static [&'static str] = Box::leak(vec![leaked].into_boxed_slice());
        let mut p = prof(TargetApp::Doubao);
        p.proc_names = whitelist;
        // 自身进程在等待期间必然存活且匹配 → 走满超时返回 false（负路径）
        assert!(!wait_gone_pids(&p, &[std::process::id()], Duration::from_millis(100)));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn wait_gone_pids_存活pid超时返回false() {
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
        assert!(!wait_gone_pids(&p, &[pid], Duration::from_millis(100)));
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
