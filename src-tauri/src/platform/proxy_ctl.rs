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
        // 服务清单仅还原面收窄消费（capture_system_proxy_snapshot 路径），
        // 普通读取（updater/bypass）只需快照三元组。networksetup_read 已改为
        // 逐服务清单（M6 修复），此处取首个启用服务透传（与
        // capture_proxy_snapshot_services 同款展开）；清单为空 = 无启用代理 →
        // None（对齐函数契约「未启用返回 None」）。本修复为解除 HEAD 遗留的
        // 编译阻断（E0308：清单为 Vec 无法按二元组解构），非本任务范围
        networksetup_read()
            .as_ref()
            .and_then(|v| v.first())
            .map(|(_, sv, bp)| (true, sv.clone(), bp.clone()))
    }
    // 第三平台编译兜底（对齐 cert_ctl.rs cert_query 约定）：返回 None = 无系统代理
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        None
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
    // 第三平台编译兜底：无实现，保持签名编译通过
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (enable, server, bypass);
        Err("unsupported platform".to_string())
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
    networksetup_apply_services(enable, server, bypass, &network_services()?)
}

/// #11 还原面收窄：捕获快照时启用代理的网络服务清单（mac 专用）。
/// 条目 = (服务名, server(host:port), bypass)——M6 修复：逐服务记录各自快照值，
/// 还原按服务写回各自的值（此前只存首个服务的值，还原时把同一对值铺满全部
/// 捕获服务，多服务不同代理配置时原值丢失）。
/// `capture_proxy_snapshot_services` 写入、`restore_system_proxy_services` 消费；
/// None = 本次启动未捕获到清单（读失败/无代理），还原退化为全服务写（旧行为）。
#[cfg(target_os = "macos")]
static CAPTURED_PROXY_SERVICES: std::sync::Mutex<Option<Vec<(String, String, String)>>> =
    std::sync::Mutex::new(None);

/// mac 快照捕获（commands/proxy.rs do_start 消费）：读快照三元组的同时，
/// 把「已启用代理的服务 + 各自快照值」存入 CAPTURED_PROXY_SERVICES 供还原。
/// 返回首个启用服务的快照（Windows 形状兼容：上游透传 + PREV_SYSTEM_PROXY）。
/// 读失败/无代理时清空清单，避免上一次启动的陈旧清单被本次还原误用。
#[cfg(target_os = "macos")]
pub fn capture_proxy_snapshot_services() -> Option<(bool, String, String)> {
    let captured = networksetup_read();
    *CAPTURED_PROXY_SERVICES
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = captured.clone();
    captured
        .as_ref()
        .and_then(|v| v.first())
        .map(|(_, sv, bp)| (true, sv.clone(), bp.clone()))
}

/// mac 还原路径（#11 还原面收窄核心，commands/proxy.rs apply_prev_proxy 消费）：
/// - 捕获时启用代理的服务 → 原样写回用户 VPN 值（enable=true）；
/// - 其余全部服务 → off（清除接管时铺开的 127.0.0.1:port 残留，否则其余网卡
///   仍指向已停止的本机代理，用户断网）；
/// - 未捕获过清单（防御）→ 退化为全服务写（旧行为）。
#[cfg(target_os = "macos")]
pub fn restore_system_proxy_services(enable: bool, server: &str, bypass: &str) -> Result<(), String> {
    let captured = CAPTURED_PROXY_SERVICES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let Some(list) = captured.filter(|l| !l.is_empty()) else {
        return networksetup_apply(enable, server, bypass);
    };
    let svc_names: Vec<String> = list.iter().map(|(s, _, _)| s.clone()).collect();
    // 审查修复（代理还原短路）：network_services() 枚举失败不应阻断整个还原链——
    // 否则已捕获的 CAPTURED_PROXY_SERVICES 服务不会被写回用户 VPN，其余服务的
    // 127.0.0.1:port 残留也不会被清除，系统代理可能指向死端口。枚举失败时降级为
    // 仅还原捕获清单（用户 VPN 写回优先级最高），其余服务清理放弃并记入错误。
    let others = match network_services() {
        Ok(all) => all.into_iter().filter(|s| !svc_names.contains(s)).collect::<Vec<_>>(),
        Err(e) => {
            let r_captured = if enable {
                restore_captured_per_service(&list)
            } else {
                networksetup_apply_services(false, server, bypass, &svc_names)
            };
            return match r_captured {
                Ok(()) => Err(format!("其余服务代理清理跳过（枚举网络服务失败: {e}），如仍有断网请手动在系统设置中关闭代理")),
                Err(ce) => Err(format!("{ce}；枚举网络服务失败: {e}")),
            };
        }
    };
    // 先关其余服务（清接管残留），再还原捕获服务；两步错误独立收集合并
    let r_others = if others.is_empty() {
        Ok(())
    } else {
        networksetup_apply_services(false, "", "", &others)
    };
    // M6 修复：捕获服务逐条写回**各自的**捕获快照值（enable=true 还原用户 VPN）；
    // enable=false（清接管残留路径）保持整表 off
    let r_captured = if enable {
        restore_captured_per_service(&list)
    } else {
        networksetup_apply_services(false, server, bypass, &svc_names)
    };
    match (r_others, r_captured) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(a), Err(b)) => Err(format!("{a}；{b}")),
        (Err(e), Ok(())) | (Ok(()), Err(e)) => Err(e),
    }
}

/// M6 修复配套：捕获服务逐条写回**各自的**捕获快照值（enable=true 还原用户 VPN）。
/// 逐服务独立收集错误（个别服务失败不掩盖其余服务成功）。
#[cfg(target_os = "macos")]
fn restore_captured_per_service(list: &[(String, String, String)]) -> Result<(), String> {
    let mut errs: Vec<String> = Vec::new();
    for (svc, sv, bp) in list {
        if let Err(e) = networksetup_apply_services(true, sv, bp, std::slice::from_ref(svc)) {
            errs.push(e);
        }
    }
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs.join("；"))
    }
}

/// 逐服务写代理（#11 还原面收窄核心）：与 networksetup_apply 同构，但服务清单由
/// 调用方给定——还原路径仅触碰「捕获时已启用代理的服务」，不把用户 VPN 快照
/// 铺满全部网络服务（用户 VPN 通常只配在单一服务上，全服务还原会给原本无代理的
/// Wi-Fi/VPN 虚拟网卡强塞用户代理地址）
#[cfg(target_os = "macos")]
pub(crate) fn networksetup_apply_services(
    enable: bool,
    server: &str,
    bypass: &str,
    services: &[String],
) -> Result<(), String> {
    // server 仅 enable=true 时需要（off 分支硬编码 0.0.0.0/0）——off 路径调用方
    // 传 ("", "")，提前解析会把「地址格式非法」误报给合法的关闭操作
    let (host, port) = if enable {
        let (h, p) = server
            .rsplit_once(':')
            .ok_or_else(|| format!("系统代理地址格式非法（应为 host:port）: {server}"))?;
        (h.to_string(), p.to_string())
    } else {
        (String::new(), String::new())
    };

    let mut errs: Vec<String> = Vec::new();
    for svc in services {
        let run = |args: &[&str]| -> Option<String> {
            crate::platform::cmd::sys_output(
                crate::platform::cmd::sys_command("networksetup").args(args),
            )
            .err()
        };
        if enable {
            for args in [
                vec!["-setwebproxy", svc.as_str(), host.as_str(), port.as_str(), "on"],
                vec!["-setsecurewebproxy", svc.as_str(), host.as_str(), port.as_str(), "on"],
            ] {
                if let Some(e) = run(&args) {
                    errs.push(format!("{svc}: {e}"));
                }
            }
            // bypass 整表覆盖写（对齐 Windows ProxyOverride 语义）；
            // 空列表用 man 页 Empty 关键字清空。
            // 读失败哨兵（审查修复）：跳过 bypass 写回——快照未知 ≠ 用户本无
            // bypass，覆盖写会清空用户白名单（bypass.rs「读失败不得按空表覆盖写」红线）
            if bypass != BYPASS_READ_UNKNOWN {
                let mut b = vec!["-setproxybypassdomains", svc];
                let domains: Vec<&str> =
                    bypass.split(';').map(str::trim).filter(|s| !s.is_empty()).collect();
                if domains.is_empty() {
                    b.push("Empty");
                } else {
                    b.extend(domains);
                }
                if let Some(e) = run(&b) {
                    errs.push(format!("{svc}: {e}"));
                }
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

/// bypass 读失败哨兵（审查修复）：与「读成功但用户未配置 bypass（空表）」区分——
/// 还原时哨兵跳过 bypass 写回（保留用户原白名单），空表才写 "Empty" 清空。
/// 违背 bypass.rs「读失败不得按空表覆盖写」红线的历史缺陷：读失败降级空串入
/// 快照，restore 按空表写 Empty 会清空用户 bypass
#[cfg(target_os = "macos")]
const BYPASS_READ_UNKNOWN: &str = "\u{0}bypass_read_unknown";

/// 读系统代理（mac 分派，M-1 侦察 ⑧ 2026-09-20 实测）：macOS 12 的 `scutil`
/// **不支持 `--proxies` 子命令**（unrecognized option），原 scutil_parse 恒 None
/// （启动前捕获用户 VPN 能力失效）——改逐网络服务 `networksetup -getwebproxy`
/// 读（与 networksetup_apply 逐服务写对称），任一服务代理启用即返回。
/// 审查 #11：遍历全部服务收集「已启用代理的服务清单」一并返回——还原面收窄
/// 只还原捕获时启用代理的服务，不把用户 VPN 铺满全部网络服务
#[cfg(target_os = "macos")]
fn networksetup_read() -> Option<Vec<(String, String, String)>> {
    let services = network_services().ok()?;
    // M6 修复：逐服务收集 (服务名, server(host:port), bypass) 快照——还原按
    // 服务写回各自的值（此前只记首个服务的值，多服务不同代理配置时原值丢失）
    let mut result: Vec<(String, String, String)> = Vec::new();
    for svc in &services {
        let out = |args: &[&str]| -> Option<String> {
            crate::platform::cmd::sys_output(
                crate::platform::cmd::sys_command("networksetup").args(args),
            )
            .ok()
        };
        // 输出形如：Enabled: Yes / Server: 127.0.0.1 / Port: 7890 /
        // Authenticated Proxy Enabled: 0（前缀匹配安全：Server 不命中认证段）
        // 审查 P1：单个服务的查询失败（VPN 虚拟网卡/失效服务等）
        // 只跳过该服务继续下一个，不得让整个函数返回 None——否则用户 VPN 代理
        // 快照丢失、停止时被误清（对齐写路径「逐服务独立收集错误」语义）
        // 审查 #6：HTTP 与 HTTPS（secure）代理是两个独立开关（部分 VPN 工具
        // 仅启用 secure web proxy），写路径成对写两键、读路径也须成对读两键，
        // 任一启用即纳入快照（HTTP 优先，与写路径同 server 对称）
        let picked = pick_proxy_text(
            out(&["-getwebproxy", svc]),
            out(&["-getsecurewebproxy", svc]),
        );
        let Some(web) = picked else {
            continue;
        };
        let (Some(host), Some(port)) = (parse_field(&web, "Server"), parse_field(&web, "Port"))
        else {
            continue;
        };
        if host.is_empty() || port.is_empty() {
            continue;
        }
        // bypass 取同服务 -getproxybypassdomains（逐行域列表；未设置时输出说明行）。
        // 过滤三重保险：空行 / 英文本地化说明（"aren't any"）/ 含空格的说明句
        // （中文 macOS 本地化说明行必含空格，bypass 域名不含空格——审查 P2）
        // 审查修复：读失败不得降级为空串继续——空串入快照后 restore 会按空表
        // 写 "Empty" 覆盖用户真实 bypass 白名单（读失败 ≠ 用户本无 bypass，
        // bypass.rs 红线）；落哨兵，还原时跳过该服务的 bypass 写回
        let bypass = match out(&["-getproxybypassdomains", svc]) {
            Some(t) => t
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.contains("aren't any") && !l.contains(' '))
                .collect::<Vec<_>>()
                .join(";"),
            None => BYPASS_READ_UNKNOWN.to_string(),
        };
        // 快照逐服务记录（M6：还原按服务写回各自的值）
        result.push((svc.clone(), format!("{host}:{port}"), bypass));
    }
    (!result.is_empty()).then_some(result)
}

/// networksetup 读输出 Enabled 行解析（Yes/No；缺行返回 None）。
/// 跨平台纯函数（pick_proxy_text 消费；Windows 主代码不使用故 allow dead_code）
#[allow(dead_code)]
fn parse_enabled(text: &str) -> Option<bool> {
    text.lines()
        .find(|l| l.trim_start().starts_with("Enabled"))
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().eq_ignore_ascii_case("yes"))
}

/// HTTP 与 secure（HTTPS）两路读输出中选中启用的一路：HTTP 启用优先（与写路径
/// 两键同 server 对称），其次 secure；两路都未启用/不可读返回 None。
/// 跨平台纯函数（不依赖真机 networksetup），锁定「任一启用即纳入快照」口径
/// （mac 主代码消费 + 测试；Windows 主代码不使用故 allow dead_code，同 parse_enabled）
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn pick_proxy_text(web: Option<String>, secure: Option<String>) -> Option<String> {
    let enabled = |t: &Option<String>| t.as_deref().and_then(parse_enabled) == Some(true);
    if enabled(&web) {
        web
    } else if enabled(&secure) {
        secure
    } else {
        None
    }
}

/// networksetup 读输出字段解析（"Key: value" 形态，前缀匹配）
#[cfg(target_os = "macos")]
fn parse_field(text: &str, key: &str) -> Option<String> {
    text.lines()
        .find(|l| l.trim_start().starts_with(key))
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().to_string())
}

/// 逐服务精确清除指向 127.0.0.1:port 的残留系统代理（commands/proxy.rs
/// cleanup_stale_local_proxy 消费）。与 apply_system_proxy(false) 的全服务 off
/// 不同：仅对「web 代理 = 127.0.0.1:port」的服务下发 off，不触碰其他网络服务——
/// 防止把用户 VPN 代理（如 Clash 配置在另一网络服务上）一并清掉。
/// 返回清除的服务数；单服务写失败逐个收集（对齐写路径语义），其余服务继续。
#[cfg(target_os = "macos")]
pub fn clear_stale_local_proxy_port(port: u16) -> Result<usize, String> {
    let services = network_services()?;
    let port_str = port.to_string();
    let mut cleared = 0usize;
    let mut errs: Vec<String> = Vec::new();
    for svc in &services {
        let read = |args: &[&str]| -> Option<String> {
            crate::platform::cmd::sys_output(
                crate::platform::cmd::sys_command("networksetup").args(args),
            )
            .ok()
        };
        // 服务不可读（VPN 虚拟网卡/失效服务等）→ 跳过该服务（networksetup_read 同款）
        let Some(web) = read(&["-getwebproxy", svc]) else {
            continue;
        };
        if parse_enabled(&web) != Some(true) {
            continue;
        }
        let (Some(host), Some(p)) = (parse_field(&web, "Server"), parse_field(&web, "Port"))
        else {
            continue;
        };
        // 仅命中「我们写的形态」（127.0.0.1 + 写盘端口 last_proxy_port.txt）才动
        // 该服务；host/port 任一不符 = 用户自己的代理，绝不触碰
        if host != "127.0.0.1" || p != port_str {
            continue;
        }
        let run = |args: &[&str]| -> Option<String> {
            crate::platform::cmd::sys_output(
                crate::platform::cmd::sys_command("networksetup").args(args),
            )
            .err()
        };
        // off 态仍需带 domain/port 形参（networksetup 语法要求，off 时被忽略）；
        // web/secure 成对关闭——set_win_proxy 接管时两键成对写（残留在两键一致）
        for args in [
            vec!["-setwebproxy", svc, "0.0.0.0", "0", "off"],
            vec!["-setsecurewebproxy", svc, "0.0.0.0", "0", "off"],
        ] {
            if let Some(e) = run(&args) {
                errs.push(format!("{svc}: {e}"));
            }
        }
        cleared += 1;
    }
    if errs.is_empty() {
        Ok(cleared)
    } else {
        Err(format!("部分网络服务清理失败: {}", errs.join("；")))
    }
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
    fn pick_proxy_text_任一启用即纳入快照() {
        let on = Some("Enabled: Yes\nServer: 127.0.0.1\nPort: 7890\n".into());
        let off = Some("Enabled: No\nServer: 0.0.0.0\nPort: 0\n".into());
        // HTTP 启用优先（与写路径两键同 server 对称）
        assert_eq!(pick_proxy_text(on.clone(), off.clone()), on);
        // 仅 secure 启用（部分 VPN 工具形态）：原审查漏读场景
        assert_eq!(pick_proxy_text(off.clone(), on.clone()), on);
        // 两路都未启用 / 不可读 → None（不纳入快照）
        assert_eq!(pick_proxy_text(off.clone(), off.clone()), None);
        assert_eq!(pick_proxy_text(None, None), None);
    }

    #[test]
    fn get_system_proxy_返回形态稳定() {
        // 仅验证不 panic（Windows 注册表读取 / networksetup 依赖真机环境，None 均合法）
        let _ = get_system_proxy();
    }
}
