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
        networksetup_read()
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
        let run = |args: &[&str]| -> Option<String> {
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

/// 读系统代理（mac 分派，M-1 侦察 ⑧ 2026-09-20 实测）：macOS 12 的 `scutil`
/// **不支持 `--proxies` 子命令**（unrecognized option），原 scutil_parse 恒 None
/// （启动前捕获用户 VPN 能力失效）——改逐网络服务 `networksetup -getwebproxy`
/// 读（与 networksetup_apply 逐服务写对称），任一服务 HTTP 代理启用即返回。
#[cfg(target_os = "macos")]
fn networksetup_read() -> Option<(bool, String, String)> {
    let services = network_services().ok()?;
    for svc in &services {
        let out = |args: &[&str]| -> Option<String> {
            crate::platform::cmd::sys_output(
                crate::platform::cmd::sys_command("networksetup").args(args),
            )
            .ok()
        };
        // 输出形如：Enabled: Yes / Server: 127.0.0.1 / Port: 7890 /
        // Authenticated Proxy Enabled: 0（前缀匹配安全：Server 不命中认证段）
        let web = out(&["-getwebproxy", svc])?;
        if parse_enabled(&web) != Some(true) {
            continue;
        }
        let (Some(host), Some(port)) = (parse_field(&web, "Server"), parse_field(&web, "Port"))
        else {
            continue;
        };
        if host.is_empty() || port.is_empty() {
            continue;
        }
        // bypass 取同服务 -getproxybypassdomains（逐行域列表；未设置时输出说明行）
        let bypass = out(&["-getproxybypassdomains", svc])
            .map(|t| {
                t.lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty() && !l.contains("aren't any"))
                    .collect::<Vec<_>>()
                    .join(";")
            })
            .unwrap_or_default();
        return Some((true, format!("{host}:{port}"), bypass));
    }
    None
}

/// networksetup 读输出 Enabled 行解析（Yes/No；缺行返回 None）
#[cfg(target_os = "macos")]
fn parse_enabled(text: &str) -> Option<bool> {
    text.lines()
        .find(|l| l.trim_start().starts_with("Enabled"))
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().eq_ignore_ascii_case("yes"))
}

/// networksetup 读输出字段解析（"Key: value" 形态，前缀匹配）
#[cfg(target_os = "macos")]
fn parse_field(text: &str, key: &str) -> Option<String> {
    text.lines()
        .find(|l| l.trim_start().starts_with(key))
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn networksetup_读输出解析() {
        // M-1 侦察 ⑧：以 networksetup -getwebproxy 标准输出样例锁定解析
        let sample = "Enabled: Yes\nServer: 127.0.0.1\nPort: 7890\nAuthenticated Proxy Enabled: 0\n";
        assert_eq!(parse_enabled(&sample), Some(true));
        assert_eq!(parse_field(&sample, "Server").as_deref(), Some("127.0.0.1"));
        assert_eq!(parse_field(&sample, "Port").as_deref(), Some("7890"));
        // 前缀边界：Port 不命中 Authenticated 段（starts_with 从行首判定）
        let off = "Enabled: No\nServer: 0.0.0.0\nPort: 0\nAuthenticated Proxy Enabled: 0\n";
        assert_eq!(parse_enabled(&off), Some(false));
        let auth = "Enabled: Yes\nServer: 1.2.3.4\nPort: 8888\nAuthenticated Proxy Enabled: Yes\n";
        assert_eq!(parse_field(&auth, "Server").as_deref(), Some("1.2.3.4"));
    }

    #[test]
    fn get_system_proxy_返回形态稳定() {
        // 仅验证不 panic（Windows 注册表读取 / networksetup 依赖真机环境，None 均合法）
        let _ = get_system_proxy();
    }
}
