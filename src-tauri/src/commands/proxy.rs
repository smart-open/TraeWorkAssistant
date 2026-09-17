//! 代理控制命令（P4-8 Rust 化）：直接驱动进程内 [`crate::device_proxy::ProxyServer`]，
//! 不再派发 python 子进程。本文件仅保留「系统代理编排 + 看门狗 + 托盘同步」编排层，
//! MITM/JWT 捕获/日志等细节全部在 device_proxy 模块内完成（日志经 ProxyLog 直接
//! emit `proxy-log` / `account-captured` 事件，与原 stdout 消费通路对齐）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::watch;

use crate::fs_utils;
use crate::platform::cmd::sys_command;
use crate::state::AppState;

/// 看门狗标记：用户主动停止代理时置 true，用于区分「主动停止」与「代理异常崩溃」。
/// 代理意外退出时，系统代理仍指向死端口 127.0.0.1:8899，需自动还原以避免全局断网。
static PROXY_INTENTIONAL_STOP: AtomicBool = AtomicBool::new(false);

/// 代理代际计数：每次启动递增。看门狗据此丢弃「迟到的崩溃报告」，
/// 避免崩溃后用户秒速重启新代理时，旧看门狗误还原新实例的系统代理。
static PROXY_GEN: AtomicU64 = AtomicU64::new(0);

/// 启动代理前已存在的系统代理（通常是用户的 VPN 梯子，如 Clash/v2rayN 的本地代理）。
/// 我们启动时会把系统代理全局指向本机 127.0.0.1:8899，并把这个外部代理作为「上游」透传，
/// 停止时再还原回去，避免覆盖/丢失用户原有的 VPN 代理设置。
/// 存储 (enabled, server, override)。
static PREV_SYSTEM_PROXY: Mutex<Option<(bool, String, String)>> = Mutex::new(None);

/// 运行中的代理句柄（进程内 ProxyServer + 启动元数据）。
/// Drop 不需要 kill 子进程：ProxyServer 析构即发送 shutdown 信号终止 accept 循环。
pub struct ProxyHandle {
    pub server: crate::device_proxy::ProxyServer,
    pub started_at: i64,
    pub gen: u64,
}

#[derive(serde::Serialize)]
pub struct ProxyStatus {
    pub running: bool,
    pub port: u16,
    pub captured: i64,
    pub started_at: Option<i64>,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 查询监听指定端口的进程 PID 列表（对齐 Python port_pids 诊断；netstat -ano 解析，
/// 探测失败返回空表，仅供错误信息展示）。列格式：TCP 本地地址 远程地址 状态 PID。
fn port_pids(port: u16) -> Vec<String> {
    // F-75 审查补齐：mac netstat 旗标为 `-an -p tcp`（小写协议名），输出列为
    // `tcp4 0 0 127.0.0.1.<port> *.* LISTEN`（PID 列不存在，仅端口占用提示用）——
    // mac 分支返回「占用」判定本身即可，PID 信息缺失无碍（仅错误文案展示）。
    #[cfg(target_os = "macos")]
    {
        let Ok(out) = sys_command("netstat").args(["-an", "-p", "tcp"]).output() else {
            return vec![];
        };
        let text = String::from_utf8_lossy(&out.stdout);
        let suffix = format!("127.0.0.1.{port}");
        // F-75 审查补齐：首列 tcp4/tcp6 前缀过滤——表头行等非连接行不得误判「占用」
        return if text.lines().any(|l| {
            let cols: Vec<&str> = l.split_whitespace().collect();
            cols.len() > 3 && cols[0].starts_with("tcp") && cols[3] == suffix
        }) {
            vec!["?".to_string()] // mac netstat 无 PID 列，占位表示「有监听」
        } else {
            vec![]
        };
    }
    #[cfg(not(target_os = "macos"))]
    {
        let Ok(out) = sys_command("netstat").args(["-ano", "-p", "TCP"]).output() else {
            return vec![];
        };
        let text = String::from_utf8_lossy(&out.stdout);
        let suffix = format!(":{port}");
        let mut pids: Vec<String> = Vec::new();
        for line in text.lines() {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() >= 5
                && cols[0].eq_ignore_ascii_case("TCP")
                && cols[1].ends_with(&suffix)
                && cols[3].eq_ignore_ascii_case("LISTENING")
                && !pids.iter().any(|p| p == cols[4])
            {
                pids.push(cols[4].to_string());
            }
        }
        pids
    }
}

/// 安全获取 Mutex 锁，即使中毒也能恢复（避免 panic 级联）。
fn safe_lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 同步托盘「代理」菜单文本（未运行显示"启动"，运行中显示"停止"）；
/// 托盘 / 前端命令 / 开机自启三条路径都汇聚到 proxy_start/proxy_stop，此处统一覆盖
fn sync_tray_proxy_text(app: &tauri::AppHandle, running: bool) {
    if let Some(tray) = app.try_state::<crate::TrayMenu>() {
        let _ = tray
            .proxy_item
            .set_text(if running { "停止代理" } else { "启动代理" });
    }
}

// ---------------- 启动 / 停止 核心逻辑（页面命令 / 托盘菜单共用） ----------------

/// 启动代理核心逻辑（对齐 Python 桌面端注入的环境变量集合，但全部改为进程内配置）
pub async fn do_start(
    app: &AppHandle,
    state: &AppState,
    proxy_state: &Mutex<Option<ProxyHandle>>,
    port: u16,
) -> Result<ProxyStatus, String> {
    // 标记「非主动停止」，供看门狗区分崩溃与用户停止
    PROXY_INTENTIONAL_STOP.store(false, Ordering::Relaxed);
    {
        let mut g = safe_lock(proxy_state);
        match &*g {
            Some(h) if h.server.is_running() => return Err("代理已在运行".into()),
            // 残留死句柄（崩溃后看门狗尚未清理）：直接回收后继续启动
            Some(_) => g.take(),
            None => None,
        };
    }
    // 兜底：端口为 0 时退化为固定端口 8899，避免注入 TRAE 的代理地址无效（见 store.ts 同款兜底）
    let port = if port == 0 { 8899 } else { port };
    // 记录本次使用的端口：cleanup_stale_local_proxy 据此识别「指向已停止本地代理的残留系统代理」
    let _ = std::fs::write(state.data_dir.join("last_proxy_port.txt"), port.to_string());
    let proxy_addr = format!("127.0.0.1:{port}");

    // 端口预检（友好报错；真正的独占绑定在 ProxyServer::start 内以
    // SO_EXCLUSIVEADDRUSE 完成，issue #7 防重复绑定「假启动」）。
    // mac 无需显式 SO_REUSEADDR：std 在非 Windows 平台的 TcpListener::bind
    // 内置该选项（TIME_WAIT 残留不会误报占用），Windows 则显式不设（防劫持）。
    match std::net::TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => drop(l),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // PID 诊断（对齐 Python 绑定失败时列出占用进程）
            let pids = port_pids(port);
            let pid_info = if pids.is_empty() { "未知".to_string() } else { pids.join(",") };
            return Err(format!(
                "端口 {port} 被其他程序占用（PID: {pid_info}），请修改代理端口或手动结束后重试"
            ));
        }
        Err(e) => return Err(format!("端口探测失败: {e}")),
    }

    // 捕获启动前的系统代理（通常是用户的 VPN 梯子，如 Clash/v2rayN 本地代理）。
    // 启动后我们会把系统代理全局指向本机 127.0.0.1:8899，从而拦截所有流量；
    // 若不把原本的 VPN 代理作为「上游」透传，外网(google/github)会直接连不通 ——
    // 这正是「开代理后外网打不开、但关代理+开VPN就正常」的根因。
    let upstream_proxy: Option<String> = {
        #[cfg(target_os = "windows")]
        {
            match get_existing_win_proxy() {
                Some((en, sv, ov))
                    if sv != proxy_addr && !sv.contains(&format!("127.0.0.1:{port}")) =>
                {
                    // 这是外部代理(VPN)，作为上游透传，并在停止时还原
                    *PREV_SYSTEM_PROXY
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) =
                        Some((en, sv.clone(), ov));
                    Some(sv)
                }
                _ => {
                    *PREV_SYSTEM_PROXY
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = None;
                    None
                }
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            // F-75 M2-2.1：mac 补齐 Windows 同等能力——读取用户 VPN（scutil）作为
            // 上游透传，并在停止时经 PREV_SYSTEM_PROXY 还原（语义与 Windows 分支一致）
            match crate::platform::proxy_ctl::get_system_proxy() {
                Some((en, sv, ov))
                    if sv != proxy_addr && !sv.contains(&format!("127.0.0.1:{port}")) =>
                {
                    *PREV_SYSTEM_PROXY
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) =
                        Some((en, sv.clone(), ov));
                    Some(sv)
                }
                _ => {
                    *PREV_SYSTEM_PROXY
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = None;
                    None
                }
            }
        }
    };

    let settings = state.settings();
    let cfg = crate::device_proxy::ProxyConfig {
        port,
        // 设置页 PROXY_DOMAINS：逗号分隔（空串/缺省由 settings() 回填默认域名）
        targets: settings
            .proxy_domains
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        // AUTO_CAPTURE_JWT 环境变量开关（对齐 Python 语义：0/false/False/空 = 关，缺省开）
        auto_capture_jwt: !std::env::var("AUTO_CAPTURE_JWT")
            .map(|v| matches!(v.as_str(), "0" | "false" | "False" | ""))
            .unwrap_or(false),
        // SQLite 化（P3）：账号/冷却/凭证快照经 store 读写，仅传数据根目录
        data_dir: state.data_dir.clone(),
        certs_dir: state.path("certs"),
        log_path: state.logs_dir().join("proxy.log"),
        req_log_dir: std::path::PathBuf::from(
            settings
                .proxy_log_path
                .clone()
                .unwrap_or_else(|| state.logs_dir().to_string_lossy().to_string()),
        ),
        upstream: upstream_proxy.as_deref().and_then(crate::device_proxy::upstream::parse_upstream),
    };

    let gen = PROXY_GEN.fetch_add(1, Ordering::Relaxed) + 1;
    let server = crate::device_proxy::ProxyServer::start(cfg, Some(app.clone()))
        .await
        .map_err(|e| {
            fs_utils::app_log(&state.data_dir, &format!("代理启动失败: {e}"));
            e
        })?;
    let started_at = now_secs();

    // 看门狗：代理任务异常退出（崩溃）时还原系统代理并报警（详见 spawn_watchdog）
    spawn_watchdog(app.clone(), state.data_dir.clone(), gen, server.exit_signal());

    {
        let mut g = safe_lock(proxy_state);
        *g = Some(ProxyHandle { server, started_at, gen });
    }

    fs_utils::app_log(&state.data_dir, &format!("代理已启动: port={port}, 进程内代理"));

    // 同步把 Windows 系统代理指向本机端口，使 TRAE 鉴权请求(api.trae.cn)汇入本代理
    match set_win_proxy(&proxy_addr) {
        Ok(()) => {
            let _ = app.emit(
                "proxy-log",
                &format!("已设置系统代理 -> {proxy_addr}（TRAE 鉴权流量将汇入本代理）"),
            );
            fs_utils::app_log(&state.data_dir, &format!("已设置系统代理 -> {proxy_addr}"));
        }
        Err(e) => {
            let _ = app.emit("proxy-log", &format!("[warn] 设置系统代理失败: {e}"));
        }
    }

    sync_tray_proxy_text(app, true);
    Ok(ProxyStatus {
        running: true,
        port,
        captured: 0,
        started_at: Some(started_at),
    })
}

/// 代理异常退出看门狗（对齐 Python 版 stdout EOF 看门狗语义）：
/// 代理任务退出（崩溃或主动 stop）都会收到 exit 信号；仅当「非主动停止」且
/// 代际未更替时判定为崩溃 —— 系统代理仍指向死端口会导致本机全局断网（签到、
/// Trae 流量全部 10061 失败），须立即还原并向前端报警。
fn spawn_watchdog(app: AppHandle, data_dir: PathBuf, gen: u64, mut exit_rx: watch::Receiver<bool>) {
    tauri::async_runtime::spawn(async move {
        // Ok(true)=shutdown 信号 / Err=发送端析构（句柄被 drop）——均表示代理任务已退出
        let _ = exit_rx.changed().await;
        if PROXY_INTENTIONAL_STOP.load(Ordering::Relaxed) {
            return;
        }
        // 代际已更替（崩溃后用户重启了新代理）：旧看门狗不得干扰新实例
        if PROXY_GEN.load(Ordering::Relaxed) != gen {
            return;
        }
        // 清理死句柄（仅当代际匹配，避免误清新实例）
        if let Some(ps) = app.try_state::<Mutex<Option<ProxyHandle>>>() {
            let mut g = ps.lock().unwrap_or_else(|e| e.into_inner());
            if g.as_ref().map(|h| h.gen) == Some(gen) {
                g.take();
            }
        }
        let _ = app.emit(
            "proxy-log",
            "[严重] 代理异常退出，正在还原系统代理以避免全局断网…",
        );
        fs_utils::app_log(&data_dir, "代理异常退出，自动还原系统代理");
        // 还原策略与 proxy_stop 一致：启动前存在启用的外部代理（用户 VPN 梯子）→ 原样还原，
        // 否则清空系统代理。peek 不消费快照：PREV_SYSTEM_PROXY 必须保留给后续
        // proxy_stop 继续还原（看门狗不消费，仅用其副本）。
        let res = restore_system_proxy(false);
        if let Err(e) = res {
            if let Some(s) = app.try_state::<AppState>() {
                fs_utils::app_log(
                    &s.data_dir,
                    &format!("还原系统代理失败(可手动在设置中关闭): {e}"),
                );
            }
        }
        let _ = app.emit("proxy-crashed", "");
        sync_tray_proxy_text(&app, false);
    });
}

#[tauri::command]
pub async fn proxy_start(
    app: AppHandle,
    state: State<'_, AppState>,
    proxy_state: State<'_, Mutex<Option<ProxyHandle>>>,
    port: u16,
) -> Result<ProxyStatus, String> {
    do_start(&app, &state, &proxy_state, port).await
}

#[tauri::command]
pub fn proxy_stop(
    _app: AppHandle,
    state: State<AppState>,
    proxy_state: State<Mutex<Option<ProxyHandle>>>,
) -> Result<ProxyStatus, String> {
    let mut g = safe_lock(&proxy_state);
    let (port, captured) = match &*g {
        Some(h) => (h.server.port, h.server.captured.load(Ordering::Relaxed)),
        None => (0, 0),
    };
    if let Some(h) = g.take() {
        // 标记「主动停止」，避免看门狗把正常停止误判为崩溃而重复还原代理
        PROXY_INTENTIONAL_STOP.store(true, Ordering::Relaxed);
        let c = h.server.captured.load(Ordering::Relaxed);
        // 发送 shutdown 信号终止 accept 循环并中止在途连接（句柄随即 drop，同样触发）
        h.server.stop();
        fs_utils::app_log(&state.data_dir, &format!("代理已停止: 共捕获 {c} 个账号"));
    }
    // 还原系统代理（#14 共用）：若启动前存在外部代理(VPN)，则还原之；否则清空，
    // 避免本机全局断网（consume=true 取走快照，本轮还原一次性）
    if let Err(e) = restore_system_proxy(true) {
        fs_utils::app_log(
            &state.data_dir,
            &format!("还原系统代理失败(可手动在设置中关闭): {e}"),
        );
    }
    sync_tray_proxy_text(&_app, false);
    Ok(ProxyStatus {
        running: false,
        port,
        captured,
        started_at: None,
    })
}

#[tauri::command]
pub fn proxy_status(
    _app: AppHandle,
    _state: State<AppState>,
    proxy_state: State<Mutex<Option<ProxyHandle>>>,
) -> ProxyStatus {
    let g = safe_lock(&proxy_state);
    match &*g {
        Some(h) => ProxyStatus {
            running: h.server.is_running(),
            port: h.server.port,
            captured: h.server.captured.load(Ordering::Relaxed),
            started_at: Some(h.started_at),
        },
        None => ProxyStatus {
            running: false,
            port: 0,
            captured: 0,
            started_at: None,
        },
    }
}

// ---------------- Windows 系统代理设置 ----------------
// TRAE 的鉴权请求(api.trae.cn)不走 Electron `--proxy-server` 命令行代理，但会读取
// Windows 系统代理(WinINet)。故启动本地代理时同步把系统代理指向本机端口，TRAE 的全部
// 流量(含鉴权)即汇入我们的 MITM 代理；停止时还原，避免全局断网。
#[cfg(target_os = "windows")]
pub(crate) fn apply_proxy(enable: bool, server: &str, override_: &str) -> Result<(), String> {
    let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings";
    run_reg(key, "ProxyEnable", "REG_DWORD", if enable { "1" } else { "0" })?;
    if enable {
        run_reg(key, "ProxyServer", "REG_SZ", server)?;
        // ProxyOverride 一律原样写回：还原路径（proxy_stop/看门狗）必须保留捕获到的
        // 用户原值（含空值），否则会把用户原本为空的排除列表改写成默认白名单。
        // 「空串填默认白名单（localhost 绕过代理、直连 127.0.0.1:7864 API 服务）」
        // 仅在 set_win_proxy 首次设置路径由调用方显式传入。
        run_reg(key, "ProxyOverride", "REG_SZ", override_)?;
    }
    notify_wininet_changed();
    Ok(())
}

#[cfg(target_os = "windows")]
pub(crate) fn set_win_proxy(addr: &str) -> Result<(), String> {
    apply_proxy(true, addr, "127.0.0.1;localhost;<local>")
}

#[cfg(target_os = "windows")]
pub(crate) fn clear_win_proxy() -> Result<(), String> {
    apply_proxy(false, "", "")
}

/// F-75 M2-2.1 mac 分支：networksetup 逐服务接管/还原（实现收口 platform::proxy_ctl）。
/// server 形态与 Windows 分支一致（"127.0.0.1:8899"）。
#[cfg(target_os = "macos")]
pub(crate) fn set_win_proxy(addr: &str) -> Result<(), String> {
    crate::platform::proxy_ctl::apply_system_proxy(true, addr, "127.0.0.1;localhost;<local>")
}

/// mac 还原清空：off 态逐服务关闭（bypass 列表不动——代理已关，列表不再生效，
/// 且保留用户 bypass 配置避免误清）
#[cfg(target_os = "macos")]
pub(crate) fn clear_win_proxy() -> Result<(), String> {
    crate::platform::proxy_ctl::apply_system_proxy(false, "", "")
}

/// 还原路径的平台分派（审查修复 P0：restore_system_proxy/_on_exit 此前直调
/// cfg(windows) 的 apply_proxy，mac 构建必然 E0425）：Windows 走注册表三键，
/// mac 走 networksetup 逐服务（PREV_SYSTEM_PROXY 快照即 get_system_proxy 返回形态，
/// 两平台 (enabled, server, bypass) 语义一致）。
fn apply_prev_proxy(enable: bool, server: &str, bypass: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        apply_proxy(enable, server, bypass)
    }
    #[cfg(target_os = "macos")]
    {
        crate::platform::proxy_ctl::apply_system_proxy(enable, server, bypass)
    }
}

/// 还原系统代理（#14 提取共用）：启动前存在启用的外部代理（用户 VPN 梯子）→ 原样
/// 还原，否则清空系统代理。`consume=true` 取走快照（proxy_stop / 应用退出，一次性）；
/// `consume=false` 仅窥视（看门狗崩溃路径不消费，保留给后续 proxy_stop 继续还原）。
pub(crate) fn restore_system_proxy(consume: bool) -> Result<(), String> {
    let prev = {
        let mut g = PREV_SYSTEM_PROXY
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if consume {
            g.take()
        } else {
            g.clone()
        }
    };
    match prev {
        Some((en, sv, ov)) if en => apply_prev_proxy(true, &sv, &ov),
        _ => clear_win_proxy(),
    }
}

/// 应用退出路径专用（#14）：仅当「我们曾接管系统代理」时才还原，避免误关用户自己的梯子。
/// `proxy_was_running`：退出清理时代理句柄是否存在（代理仍在运行）。
/// - 捕获到用户 VPN 原值 → 原样还原（consume，一次性）；
/// - 无 VPN 原值但代理在运行 → 我们曾把系统代理指向本机端口 → 清空；
/// - 两者皆无（代理从未启动 / 已正常停止并还原过）→ 不触碰系统代理。
pub(crate) fn restore_system_proxy_on_exit(proxy_was_running: bool) -> Result<(), String> {
    let prev = PREV_SYSTEM_PROXY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    match prev {
        Some((true, sv, ov)) => apply_prev_proxy(true, &sv, &ov),
        // 防御分支：快照存在但未启用（正常路径不会出现，get_existing 仅存启用项）
        Some(_) => clear_win_proxy(),
        None if proxy_was_running => clear_win_proxy(),
        None => Ok(()),
    }
}

/// 直开应用前的防御：若系统代理仍指向本机「我们上次使用的端口」而本地代理已停止
/// （应用异常退出等场景可能未还原），提前清除，避免 Trae 全部请求 ERR_CONNECTION_RESET。
/// 只匹配我们自己写盘记录的端口，不会误伤用户自己的 VPN 本地代理（如 Clash 7890）。
#[cfg(target_os = "windows")]
pub(crate) fn cleanup_stale_local_proxy(state: &AppState) {
    let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings";
    let enable = reg_query_value(key, "ProxyEnable")
        .map(|v| v.contains("1"))
        .unwrap_or(false);
    if !enable {
        return;
    }
    let server = reg_query_value(key, "ProxyServer").unwrap_or_default();
    let last_port = std::fs::read_to_string(state.data_dir.join("last_proxy_port.txt"))
        .ok()
        .and_then(|s| s.trim().parse::<u16>().ok());
    let Some(port) = last_port else { return };
    let ours = format!("127.0.0.1:{port}");
    if server.contains(&ours) {
        match clear_win_proxy() {
            Ok(()) => fs_utils::app_log(
                &state.data_dir,
                &format!("已清理指向已停止本地代理的残留系统代理({ours})"),
            ),
            Err(e) => fs_utils::app_log(&state.data_dir, &format!("清理残留系统代理失败: {e}")),
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn cleanup_stale_local_proxy(_state: &AppState) {}

/// 通知 WinINet 代理设置已变更，让运行中的进程立即生效
/// 不调用此函数的话，已有进程会继续使用缓存的旧代理设置
#[cfg(target_os = "windows")]
fn notify_wininet_changed() {
    #[link(name = "wininet")]
    extern "system" {
        fn InternetSetOptionW(
            h_internet: *mut std::ffi::c_void,
            option: u32,
            buffer: *mut std::ffi::c_void,
            buffer_length: u32,
        ) -> i32;
    }

    const INTERNET_OPTION_SETTINGS_CHANGED: u32 = 39;
    const INTERNET_OPTION_REFRESH: u32 = 37;

    unsafe {
        InternetSetOptionW(
            std::ptr::null_mut(),
            INTERNET_OPTION_SETTINGS_CHANGED,
            std::ptr::null_mut(),
            0,
        );
        InternetSetOptionW(
            std::ptr::null_mut(),
            INTERNET_OPTION_REFRESH,
            std::ptr::null_mut(),
            0,
        );
    }
}

#[cfg(target_os = "windows")]
fn reg_query_value(key: &str, name: &str) -> Option<String> {
    let out = sys_command("reg")
        .args(["query", key, "/v", name])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    for line in s.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix(name) {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            // parts[0] = 类型(REG_SZ/REG_DWORD)，parts[1..] = 值
            if parts.len() >= 2 {
                return Some(parts[1..].join(" "));
            }
        }
    }
    None
}

/// 读取启动前的系统代理设置。返回 (enabled, server, override)。
/// 若不存在或未启用则返回 None（表示用户本来就没有系统代理/VPN）。
#[cfg(target_os = "windows")]
pub(crate) fn get_existing_win_proxy() -> Option<(bool, String, String)> {
    let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings";
    let enable = reg_query_value(key, "ProxyEnable")
        .map(|v| v.contains("1"))
        .unwrap_or(false);
    if !enable {
        return None;
    }
    let server = reg_query_value(key, "ProxyServer").unwrap_or_default();
    if server.is_empty() {
        return None;
    }
    let override_ = reg_query_value(key, "ProxyOverride").unwrap_or_default();
    Some((true, server, override_))
}

#[cfg(target_os = "windows")]
fn run_reg(key: &str, name: &str, kind: &str, value: &str) -> Result<(), String> {
    let status = sys_command("reg")
        .args(["add", key, "/v", name, "/t", kind, "/d", value, "/f"])
        .status()
        .map_err(|e| format!("设置系统代理失败: {e}"))?;
    if !status.success() {
        return Err(format!("reg add 失败: {name}"));
    }
    Ok(())
}

// F-75 M2-2.1：原「仅 Windows 支持系统代理设置」stub 已由上方 mac 实现替代
// （set_win_proxy/clear_win_proxy 现为两平台真实实现，Windows 路径零变化）

/// 应用退出路径使用：标记「主动停止」，让看门狗静默（退出清理由 RunEvent::Exit 统一完成）
pub(crate) fn mark_intentional_stop() {
    PROXY_INTENTIONAL_STOP.store(true, Ordering::Relaxed);
}
