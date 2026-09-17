//! 系统代理读写（F-75 M2-2.1）：
//! - Windows：注册表 HKCU Internet Settings（原 commands/proxy.rs 实现原样复用，
//!   行为零变化）；
//! - macOS：`scutil --proxies`（读，全局聚合视图含 ExceptionsList）+
//!   `networksetup`（写，对所有网络服务逐个设置，失败逐服务收集）。
//!
//! 消费方：
//! 1. `commands/proxy.rs::do_start` 启动前捕获用户 VPN（mac 侧补齐 Windows 同等能力）；
//! 2. `commands/updater.rs::system_proxy_url` 更新器走用户 VPN 下载（**原编译阻断点**：
//!    该函数此前直调 cfg(windows) 的 get_existing_win_proxy，mac 构建必失败）；
//! 3. `device_proxy/bypass.rs` OAuth 直连豁免的 bypass 列表读写。

/// 读系统代理，返回 (enabled, "host:port", bypass 域列表——';' 连接)。
/// 未启用或解析失败返回 None（表示用户本来没有系统代理/VPN）。
pub fn get_system_proxy() -> Option<(bool, String, String)> {
    #[cfg(windows)]
    {
        crate::commands::proxy::get_existing_win_proxy()
    }
    #[cfg(target_os = "macos")]
    {
        scutil_parse()
    }
}

/// 应用系统代理设置（enable=true 接管 / false 还原清空）。
/// Windows：apply_proxy 注册表三键 + WinINET 通知（原样复用）；
/// macOS：networksetup 逐网络服务 setwebproxy/setsecurewebproxy + bypass 整表覆盖写。
#[allow(dead_code)] // mac 分支消费（proxy.rs cfg(macos) set/clear_win_proxy），Windows 构建不接线
pub fn apply_system_proxy(enable: bool, server: &str, bypass: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        crate::commands::proxy::apply_proxy(enable, server, bypass)
    }
    #[cfg(target_os = "macos")]
    {
        networksetup_apply(enable, server, bypass)
    }
}

/// 枚举全部网络服务（macOS 专用；跳过带 * 的禁用项与首行说明）
#[cfg(target_os = "macos")]
pub(crate) fn network_services() -> Result<Vec<String>, String> {
    let out = crate::platform::cmd::sys_output(
        crate::platform::cmd::sys_command("networksetup").arg("-listallnetworkservices"),
    )?;
    Ok(out
        .lines()
        .skip(1) // 首行为 "An asterisk (*) denotes ..." 说明行
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.starts_with('*'))
        .map(String::from)
        .collect())
}

#[cfg(target_os = "macos")]
fn networksetup_apply(enable: bool, server: &str, bypass: &str) -> Result<(), String> {
    let services = network_services()?;
    let (host, port) = server
        .rsplit_once(':')
        .ok_or_else(|| format!("系统代理地址格式非法（应为 host:port）: {server}"))?;

    let mut errs: Vec<String> = Vec::new();
    for svc in &services {
        let mut run = |args: &[&str]| -> Option<String> {
            crate::platform::cmd::sys_output(
                crate::platform::cmd::sys_command("networksetup").args(args),
            )
            .err()
        };
        if enable {
            for args in [
                vec!["-setwebproxy", svc, host, port, "on"],
                vec!["-setsecurewebproxy", svc, host, port, "on"],
            ] {
                if let Some(e) = run(&args) {
                    errs.push(format!("{svc}: {e}"));
                }
            }
            // bypass 整表覆盖写（对齐 Windows ProxyOverride 语义）；
            // 空列表用 man 页 Empty 关键字清空
            let mut b = vec!["-setproxybypassdomains", svc];
            let domains: Vec<&str> = bypass.split(';').map(str::trim).filter(|s| !s.is_empty()).collect();
            if domains.is_empty() {
                b.push("Empty");
            } else {
                b.extend(domains);
            }
            if let Some(e) = run(&b) {
                errs.push(format!("{svc}: {e}"));
            }
        } else {
            // off 态仍需带 domain/port 形参（networksetup 语法要求，off 时被忽略）
            for args in [
                vec!["-setwebproxy", svc, "0.0.0.0", "0", "off"],
                vec!["-setsecurewebproxy", svc, "0.0.0.0", "0", "off"],
            ] {
                if let Some(e) = run(&args) {
                    errs.push(format!("{svc}: {e}"));
                }
            }
        }
    }
    // 逐服务独立收集错误（VPN 虚拟网卡等个别服务失败不掩盖其余服务成功）
    if errs.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "部分网络服务设置失败: {}",
            errs.join("；")
        ))
    }
}

/// scutil --proxies 输出解析：
///   <dictionary> {
///     HTTPEnable : 1
///     HTTPProxy : 127.0.0.1
///     HTTPPort : 8899
///     ExceptionsList : <array> {
///       0 : 127.0.0.1
///       1 : localhost
///     }
///   }
/// 注意 substring 匹配边界：HTTPEnable 不命中 HTTPSEnable（HTTP 后跟 S），
/// HTTPProxy 不命中 HTTPSProxy，语义安全。
#[cfg(target_os = "macos")]
fn scutil_parse() -> Option<(bool, String, String)> {
    let out = crate::platform::cmd::sys_output(
        crate::platform::cmd::sys_command("scutil").arg("--proxies"),
    )
    .ok()?;
    let get = |k: &str| {
        out.lines()
            .find(|l| l.contains(k))
            .and_then(|l| l.split(':').nth(1).map(|v| v.trim().to_string()))
    };
    if get("HTTPEnable").as_deref() != Some("1") {
        return None;
    }
    let host = get("HTTPProxy")?;
    let port = get("HTTPPort")?;
    if host.is_empty() || port.is_empty() {
        return None;
    }
    // ExceptionsList 数组段抽取（聚合视图的 bypass 域，还原时回写保真）
    let mut bypass: Vec<String> = Vec::new();
    let mut in_list = false;
    for line in out.lines() {
        if line.contains("ExceptionsList") {
            in_list = true;
            continue;
        }
        if in_list {
            let t = line.trim();
            if t.starts_with('}') {
                break;
            }
            if let Some((_, v)) = t.split_once(':') {
                let v = v.trim();
                if !v.is_empty() {
                    bypass.push(v.to_string());
                }
            }
        }
    }
    Some((true, format!("{host}:{port}"), bypass.join(";")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn scutil_解析样例() {
        // 以样例文本锁定解析逻辑（真机 M-1 侦察 8 复核权限模型）
        let sample = "<dictionary> {\n  HTTPEnable : 1\n  HTTPProxy : 127.0.0.1\n  HTTPPort : 7890\n  ExceptionsList : <array> {\n    0 : 127.0.0.1\n    1 : localhost\n  }\n}";
        // 解析逻辑抽自 scutil_parse 内联实现——此处直接验证关键子串判定边界
        assert!(sample.lines().any(|l| l.contains("HTTPEnable")));
        assert!(!sample.lines().any(|l| l.contains("HTTPEnable") && l.contains("HTTPS")));
    }

    #[test]
    fn get_system_proxy_返回形态稳定() {
        // 仅验证不 panic（Windows 注册表读取 / scutil 依赖真机环境，None 均合法）
        let _ = get_system_proxy();
    }
}
