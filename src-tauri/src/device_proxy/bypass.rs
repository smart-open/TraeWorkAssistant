//! OAuth 登录代理直连豁免（F-78 批次 2，issue #10 根因①）。
//! MITM 代理运行时系统代理指向 127.0.0.1:<port>，浏览器 OAuth 登录页流量会被
//! 自签 CA 解密，CA 未受信即报 ERR_CERT_AUTHORITY_INVALID。
//! 发起 OAuth 前把登录/鉴权域名追加进系统代理 ProxyOverride（直连白名单），
//! 登录结束只移除本次追加的条目，绝不触碰用户原有配置。
//! 仅当系统代理确实指向本软件 MITM 端口（data/last_proxy_port.txt 记录）时才改写；
//! 用户自己的远程代理（VPN 等）不会被本软件解密，无需也绝不改动。

use std::path::Path;
use std::sync::Mutex;

/// OAuth 登录链路需直连的域名（登录页 / 鉴权 API / 本机回调）
pub const OAUTH_BYPASS_ENTRIES: &[&str] = &[
    "www.trae.cn",
    "api.trae.cn",
    "api.trae.com.cn",
    "127.0.0.1",
    "localhost",
];

/// 本次已追加进 ProxyOverride 的条目（disable 时只删这些，保证幂等且不误删用户配置）
static ADDED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// 崩溃残留标记（enable 时写入 / disable 时删除）：进程异常退出未来得及还原时，
/// 下次启动据此只清理本软件追加过的条目（缺陷13）
fn marker_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("oauth_bypass_pending.json")
}

fn save_marker(data_dir: &Path, added: &[String]) {
    if let Ok(json) = serde_json::to_string(added) {
        let _ = std::fs::write(marker_path(data_dir), json);
    }
}

/// 启动时清理崩溃残留（main.rs setup 调用）：上次进程未正常还原 ProxyOverride 时，
/// 按标记文件只移除本软件追加的条目；无标记则幂等空操作
#[cfg(target_os = "windows")]
pub fn cleanup_residual_bypass(data_dir: &Path) {
    let marker = marker_path(data_dir);
    let Ok(json) = std::fs::read_to_string(&marker) else {
        return;
    };
    let Ok(added) = serde_json::from_str::<Vec<String>>(&json) else {
        // 标记损坏：删掉即可，残留条目（若有）由用户手动处理，不再二次猜测
        let _ = std::fs::remove_file(&marker);
        return;
    };
    if added.is_empty() {
        let _ = std::fs::remove_file(&marker);
        return;
    }
    let key = win::KEY_PATH;
    if win::reg_query_value(key, "ProxyEnable").as_deref() == Some("0x1") {
        let current = win::reg_query_value(key, "ProxyOverride").unwrap_or_default();
        let remaining: Vec<&str> = current
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty() && !added.iter().any(|a| a.eq_ignore_ascii_case(s)))
            .collect();
        if win::run_reg(key, "ProxyOverride", "REG_SZ", &remaining.join(";")).is_err() {
            // 失败保留标记，下次启动重试
            return;
        }
        let _ = win::notify_wininet_changed();
    }
    let _ = std::fs::remove_file(&marker);
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn cleanup_residual_bypass(_data_dir: &Path) {}

// ---------------------------------------------------------------------------
// macOS 实现（F-75 M2-2.1）：bypass 列表经 networksetup 逐服务读改写。
// enable 仅当系统代理确实指向本软件 MITM 端口时改写（与 Windows 语义一致，
// 用户自己的代理绝不碰）；列表整体覆盖写，先读当前列表再合并。
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn mac_services() -> Result<Vec<String>, String> {
    crate::platform::proxy_ctl::network_services()
}

/// 读某服务的 bypass 域列表（未设置时 networksetup 输出说明行，过滤之）
#[cfg(target_os = "macos")]
fn mac_bypass_list(svc: &str) -> Vec<String> {
    crate::platform::cmd::sys_output(
        crate::platform::cmd::sys_command("networksetup").args(["-getproxybypassdomains", svc]),
    )
    .map(|out| {
        out.lines()
            .map(str::trim)
            .filter(|s| {
                !s.is_empty() && !s.contains("aren't any bypass domains")
            })
            .map(String::from)
            .collect()
    })
    .unwrap_or_default()
}

/// 写某服务的 bypass 域列表（空列表 → man 页 Empty 关键字清空）
#[cfg(target_os = "macos")]
fn mac_set_bypass(svc: &str, domains: &[String]) -> Result<(), String> {
    let mut args: Vec<&str> = vec!["-setproxybypassdomains", svc];
    if domains.is_empty() {
        args.push("Empty");
    } else {
        args.extend(domains.iter().map(|s| s.as_str()));
    }
    crate::platform::cmd::sys_output(
        crate::platform::cmd::sys_command("networksetup").args(&args),
    )
    .map(|_| ())
}

#[cfg(target_os = "macos")]
pub fn cleanup_residual_bypass(data_dir: &Path) {
    let marker = marker_path(data_dir);
    let Ok(json) = std::fs::read_to_string(&marker) else {
        return;
    };
    let Ok(added) = serde_json::from_str::<Vec<String>>(&json) else {
        let _ = std::fs::remove_file(&marker);
        return;
    };
    if added.is_empty() {
        let _ = std::fs::remove_file(&marker);
        return;
    }
    // 审查修复（P1）：枚举/清理任一步失败 → 保留 marker 下次启动重试（对齐 Windows 版：
    // run_reg 失败即 return 保留标记），失败仍删 marker 会「失忆」致用户 bypass 永久残留
    let Ok(services) = mac_services() else {
        return;
    };
    for svc in &services {
        let current = mac_bypass_list(svc);
        let remaining: Vec<String> = current
            .into_iter()
            .filter(|s| !added.iter().any(|a| a.eq_ignore_ascii_case(s)))
            .collect();
        if mac_set_bypass(svc, &remaining).is_err() {
            return;
        }
    }
    let _ = std::fs::remove_file(&marker);
}

/// 当前生效的 MITM 代理端口（与 commands::proxy 写入的 last_proxy_port.txt 对齐）
fn mitm_port(data_dir: &Path) -> Option<u16> {
    std::fs::read_to_string(data_dir.join("last_proxy_port.txt"))
        .ok()
        .and_then(|s| s.trim().parse::<u16>().ok())
}

/// 发起 OAuth 前启用直连豁免。返回 Ok(true) 表示本次实际修改了系统代理白名单。
#[cfg(target_os = "windows")]
pub fn enable_oauth_bypass(data_dir: &Path) -> Result<bool, String> {
    let key = win::KEY_PATH;

    let Some(port) = mitm_port(data_dir) else {
        // 本软件未启动过 MITM 代理，无需豁免
        return Ok(false);
    };
    if win::reg_query_value(key, "ProxyEnable").as_deref() != Some("0x1") {
        return Ok(false);
    }
    // 仅当系统代理确实指向本软件 MITM 端口时才改写（用户自己的代理不碰）
    let server = win::reg_query_value(key, "ProxyServer").unwrap_or_default();
    if server != format!("127.0.0.1:{port}") {
        return Ok(false);
    }

    let mut added = ADDED.lock().unwrap_or_else(|e| e.into_inner());
    if !added.is_empty() {
        // 已豁免（幂等）：上次 enable 后未还原
        return Ok(true);
    }
    let current = win::reg_query_value(key, "ProxyOverride").unwrap_or_default();
    let existing: Vec<String> = current
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    let mut merged = existing.clone();
    for e in OAUTH_BYPASS_ENTRIES {
        if !existing.iter().any(|x| x.eq_ignore_ascii_case(e)) {
            merged.push((*e).to_string());
            added.push((*e).to_string());
        }
    }
    if added.is_empty() {
        return Ok(false);
    }
    win::run_reg(key, "ProxyOverride", "REG_SZ", &merged.join(";"))?;
    win::notify_wininet_changed();
    // 持久化追加清单：进程崩溃未还原时下次启动清理（缺陷13）
    save_marker(data_dir, &added);
    Ok(true)
}

/// OAuth 结束后还原直连豁免：只移除本次追加的条目（未修改过则幂等成功）
#[cfg(target_os = "windows")]
pub fn disable_oauth_bypass(data_dir: &Path) -> Result<(), String> {
    let key = win::KEY_PATH;

    let mut added = ADDED.lock().unwrap_or_else(|e| e.into_inner());
    if added.is_empty() {
        return Ok(());
    }
    // 系统代理已被关闭/还原（如 MITM 代理停止时整体还原了快照）时豁免已随之失效，直接清账
    if win::reg_query_value(key, "ProxyEnable").as_deref() == Some("0x1") {
        let current = win::reg_query_value(key, "ProxyOverride").unwrap_or_default();
        let remaining: Vec<&str> = current
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty() && !added.iter().any(|a| a.eq_ignore_ascii_case(s)))
            .collect();
        win::run_reg(key, "ProxyOverride", "REG_SZ", &remaining.join(";"))?;
        win::notify_wininet_changed();
    }
    added.clear();
    let _ = std::fs::remove_file(marker_path(data_dir));
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn enable_oauth_bypass(data_dir: &Path) -> Result<bool, String> {
    let Some(port) = mitm_port(data_dir) else {
        // 本软件未启动过 MITM 代理，无需豁免
        return Ok(false);
    };
    // 仅当系统代理确实指向本软件 MITM 端口时才改写（用户自己的代理不碰）
    let ours = format!("127.0.0.1:{port}");
    if crate::platform::proxy_ctl::get_system_proxy()
        .map(|(_, sv, _)| sv != ours)
        .unwrap_or(true)
    {
        return Ok(false);
    }

    let mut added = ADDED.lock().unwrap_or_else(|e| e.into_inner());
    if !added.is_empty() {
        // 已豁免（幂等）：上次 enable 后未还原
        return Ok(true);
    }
    // 审查修复（P1）：逐服务聚合错误，不用 `?` 短路——部分服务写成功后若直接返回 Err，
    // 已写入条目不进 ADDED/marker，disable/cleanup「失忆」致用户 bypass 永久残留
    let mut collected: Vec<String> = Vec::new();
    let mut errs: Vec<String> = Vec::new();
    for svc in &mac_services()? {
        let current = mac_bypass_list(svc);
        let mut merged = current.clone();
        for e in OAUTH_BYPASS_ENTRIES {
            if !current.iter().any(|x| x.eq_ignore_ascii_case(e)) {
                merged.push((*e).to_string());
                if !collected.iter().any(|x| x.eq_ignore_ascii_case(e)) {
                    collected.push((*e).to_string());
                }
            }
        }
        if merged.len() != current.len() {
            if let Err(e) = mac_set_bypass(svc, &merged) {
                errs.push(format!("{svc}: {e}"));
            }
        }
    }
    if collected.is_empty() {
        return Ok(false);
    }
    // 先落账再报错：已写入服务的条目必须可被 disable/cleanup 还原；
    // 调用方（oauth_loopback）对 Err 仅记日志不阻断登录，登录结束 disable 按清单还原
    *added = collected;
    // 持久化追加清单：进程崩溃未还原时下次启动清理（缺陷13，与 Windows 同机制）
    save_marker(data_dir, &added);
    if errs.is_empty() {
        Ok(true)
    } else {
        Err(format!("部分网络服务写入 bypass 失败: {}", errs.join("; ")))
    }
}

#[cfg(target_os = "macos")]
pub fn disable_oauth_bypass(data_dir: &Path) -> Result<(), String> {
    let mut added = ADDED.lock().unwrap_or_else(|e| e.into_inner());
    if added.is_empty() {
        return Ok(());
    }
    // 还原：只移除本次追加的条目（对齐 Windows 全局清单语义）。任一服务读写失败
    // 即整体报错且不清账（保留 ADDED + marker 供重试）——半还原状态下「失忆」
    // 会让用户 bypass 永久残留（对齐 Windows 注册表路径的 ? 传播语义）
    for svc in mac_services()? {
        let current = mac_bypass_list(&svc);
        let remaining: Vec<String> = current
            .into_iter()
            .filter(|s| !added.iter().any(|a| a.eq_ignore_ascii_case(s)))
            .collect();
        mac_set_bypass(&svc, &remaining)?;
    }
    added.clear();
    let _ = std::fs::remove_file(marker_path(data_dir));
    Ok(())
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn enable_oauth_bypass(_data_dir: &Path) -> Result<bool, String> {
    Ok(false)
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn disable_oauth_bypass(_data_dir: &Path) -> Result<(), String> {
    Ok(())
}

// ---------------------------------------------------------------------------
// Windows 注册表工具（与 commands::proxy 同款实现；该模块函数为私有，此处复刻）
// ---------------------------------------------------------------------------
#[cfg(target_os = "windows")]
mod win {
    const KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings";

    pub(super) fn reg_query_value(key: &str, name: &str) -> Option<String> {
        let out = crate::platform::cmd::sys_command("reg")
            .args(["query", key, "/v", name])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        text.lines()
            .find(|l| l.trim_start().starts_with(name))
            .and_then(|l| l.split_whitespace().last().map(|s| s.to_string()))
    }

    pub(super) fn run_reg(key: &str, name: &str, ty: &str, value: &str) -> Result<(), String> {
        let out = crate::platform::cmd::sys_command("reg")
            .args(["add", key, "/v", name, "/t", ty, "/d", value, "/f"])
            .output()
            .map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).to_string())
        }
    }

    /// 通知 WinINet 系统代理设置已变化（否则浏览器可能继续用旧配置）
    pub(super) fn notify_wininet_changed() {
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

    pub(super) const KEY_PATH: &str = KEY;
}
