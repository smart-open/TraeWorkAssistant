//! IP 允许列表（Phase 3 T12a）：应用层访问控制，覆盖 /v1/* 网关、/api/* 管理面
//! 与静态资源（/health、/healthz 探活放行）。
//!
//! - 配置存 kv `ip_allowlist`（enabled / trust_proxy / cidrs），进程内 LazyLock 缓存，
//!   保存（ip_allowlist_set）后立即热生效；
//! - 回环地址（127.0.0.0/8、::1）始终放行，防止误配置自锁；
//! - trust_proxy = true 时信任反代头取客户端 IP（X-Real-IP → X-Forwarded-For 首项），
//!   否则一律取 TCP 对端地址（防伪造头绕过）；取不到来源 IP 时 fail-closed 拒绝；
//! - CIDR 匹配手写 std::net（IPv4 u32 掩码 / IPv6 u128 掩码），零新依赖；
//!   IPv4-mapped IPv6（::ffff:a.b.c.d）归一为 IPv4 参与匹配。

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, LazyLock, RwLock};

use aiwork_core::state::AppState;
use axum::extract::{ConnectInfo, Request};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::Response;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::json_response;

/// kv 键
const KV_KEY: &str = "ip_allowlist";

/// IP 允许列表配置（kv 持久化；字段与前端 IpAllowlistConfig 对齐）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IpAllowlistConfig {
    /// 总开关：关闭时所有来源放行
    #[serde(default)]
    pub enabled: bool,
    /// 反代信任：开启后取 X-Real-IP / X-Forwarded-For 首项作为客户端 IP；
    /// 直连部署必须关闭，否则客户端可伪造头绕过
    #[serde(default)]
    pub trust_proxy: bool,
    /// 允许的 CIDR/IP 条目（如 "192.168.1.0/24"、"10.0.0.5"、"2001:db8::/32"）
    #[serde(default)]
    pub cidrs: Vec<String>,
}

/// 已解析的允许网段（掩码形式，匹配时按位与）
#[derive(Debug, Clone, Copy)]
enum AllowedNet {
    /// (网络地址, 掩码)
    V4(u32, u32),
    V6(u128, u128),
}

/// 进程内配置缓存（热生效）：请求路径只读 Arc 快照，保存时整体替换
struct AllowCache {
    enabled: bool,
    trust_proxy: bool,
    nets: Vec<AllowedNet>,
}

static CACHE: LazyLock<RwLock<Arc<AllowCache>>> = LazyLock::new(|| {
    RwLock::new(Arc::new(AllowCache {
        enabled: false,
        trust_proxy: false,
        nets: Vec::new(),
    }))
});

/// 从 kv 重载缓存（启动时与保存后各调用一次）
pub fn reload(state: &AppState) {
    let cfg: IpAllowlistConfig = aiwork_core::store::db(&state.data_dir).kv_get(KV_KEY);
    let (nets, skipped) = parse_all(&cfg.cidrs);
    *CACHE.write().expect("ip_allowlist 缓存锁中毒") = Arc::new(AllowCache {
        enabled: cfg.enabled,
        trust_proxy: cfg.trust_proxy,
        nets,
    });
    if cfg.enabled {
        let mut msg = format!(
            "IP 允许列表已启用: {} 条网段（trust_proxy={}）",
            cfg.cidrs.len(),
            cfg.trust_proxy
        );
        if skipped > 0 {
            msg.push_str(&format!("；{skipped} 条无效条目已忽略"));
        }
        println!("{msg}");
        aiwork_core::fs_utils::app_log(&state.data_dir, &msg);
    }
}

/// 解析 CIDR 列表 →（有效网段, 无效条目数）
fn parse_all(cidrs: &[String]) -> (Vec<AllowedNet>, usize) {
    let mut nets = Vec::with_capacity(cidrs.len());
    let mut skipped = 0usize;
    for s in cidrs {
        match parse_net(s) {
            Some(n) => nets.push(n),
            None => skipped += 1,
        }
    }
    (nets, skipped)
}

/// 读取配置（kv 直读，随存随取）
pub fn config_get(state: &AppState) -> IpAllowlistConfig {
    aiwork_core::store::db(&state.data_dir).kv_get(KV_KEY)
}

/// 保存配置：归一化（trim/去空/去重）+ 逐条校验（存在无效条目则整体拒绝），
/// 落库后重载缓存立即生效。返回保存后的生效值（前端表单以返回值为准）。
pub fn config_set(state: &AppState, cfg: IpAllowlistConfig) -> Result<IpAllowlistConfig, String> {
    // 归一化：trim + 去空行 + 去重（保持首次出现顺序）
    let mut seen = std::collections::HashSet::new();
    let cidrs: Vec<String> = cfg
        .cidrs
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .filter(|s| seen.insert(s.clone()))
        .collect();
    // 校验：任一条目无法解析 → 整体拒绝（明确错误供前端提示）
    let bad: Vec<&str> = cidrs
        .iter()
        .map(|s| s.as_str())
        .filter(|s| parse_net(s).is_none())
        .collect();
    if !bad.is_empty() {
        return Err(format!("无效的 CIDR/IP 条目: {}", bad.join(", ")));
    }
    let cfg = IpAllowlistConfig {
        enabled: cfg.enabled,
        trust_proxy: cfg.trust_proxy,
        cidrs,
    };
    aiwork_core::store::db(&state.data_dir)
        .kv_set(KV_KEY, &cfg)
        .map_err(|e| format!("保存失败: {e}"))?;
    reload(state);
    let msg = format!(
        "IP 允许列表已更新: enabled={} trust_proxy={} {} 条网段",
        cfg.enabled,
        cfg.trust_proxy,
        cfg.cidrs.len()
    );
    println!("{msg}");
    aiwork_core::fs_utils::app_log(&state.data_dir, &msg);
    Ok(cfg)
}

/// IP 允许列表中间件：挂载于合并后路由最外层（见 main.rs）。
/// `/health`、`/healthz` 放行；未启用或列表为空放行；回环地址始终放行。
pub(crate) async fn ip_middleware(req: Request, next: Next) -> Response {
    match req.uri().path() {
        "/health" | "/healthz" => return next.run(req).await,
        _ => {}
    }
    let cache = CACHE.read().expect("ip_allowlist 缓存锁中毒").clone();
    if !cache.enabled || cache.nets.is_empty() {
        return next.run(req).await;
    }
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip());
    let ip = client_ip(req.headers(), peer, cache.trust_proxy);
    let allowed = match ip {
        // 回环始终放行（本机运维通道，防自锁）
        Some(ip) if ip.is_loopback() => true,
        Some(ip) => ip_allowed(ip, &cache.nets),
        // 取不到来源 IP：fail-closed
        None => false,
    };
    if allowed {
        next.run(req).await
    } else {
        json_response(
            axum::http::StatusCode::FORBIDDEN,
            json!({"ok": false, "error": "来源 IP 不在允许列表内"}),
        )
    }
}

/// 提取客户端 IP：trust_proxy 时反代头优先（X-Real-IP → X-Forwarded-For 首项），
/// 否则仅信任 TCP 对端地址。
fn client_ip(headers: &HeaderMap, peer: Option<IpAddr>, trust_proxy: bool) -> Option<IpAddr> {
    if trust_proxy {
        if let Some(ip) = headers
            .get("x-real-ip")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse().ok())
        {
            return Some(ip);
        }
        if let Some(ip) = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split(',').next())
            .and_then(|s| s.trim().parse().ok())
        {
            return Some(ip);
        }
    }
    peer
}

/// 解析单条 CIDR/IP（裸 IP 视为 /32 或 /128）；失败返回 None。
/// IPv4-mapped IPv6 归一为 IPv4：前缀 ≤32 按 v4 位宽，96..=128 按减 96 折算。
fn parse_net(s: &str) -> Option<AllowedNet> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (addr_part, prefix) = match s.split_once('/') {
        Some((a, p)) => (a, Some(p.trim().parse::<u8>().ok()?)),
        None => (s, None),
    };
    let addr: IpAddr = addr_part.trim().parse().ok()?;
    match addr {
        IpAddr::V4(a) => {
            let bits = prefix.unwrap_or(32);
            if bits > 32 {
                return None;
            }
            let (net, mask) = mask_v4(u32::from(a), bits);
            Some(AllowedNet::V4(net, mask))
        }
        IpAddr::V6(a) => {
            if let Some(v4) = a.to_ipv4_mapped() {
                let bits = match prefix {
                    None => 32,
                    Some(b) if b <= 32 => b,
                    Some(b) if (96..=128).contains(&b) => b - 96,
                    Some(_) => return None,
                };
                let (net, mask) = mask_v4(u32::from(v4), bits);
                return Some(AllowedNet::V4(net, mask));
            }
            let bits = prefix.unwrap_or(128);
            if bits > 128 {
                return None;
            }
            let (net, mask) = mask_v6(u128::from_be_bytes(a.octets()), bits);
            Some(AllowedNet::V6(net, mask))
        }
    }
}

/// IPv4 网段归一：(网络地址, 掩码)
fn mask_v4(addr: u32, bits: u8) -> (u32, u32) {
    let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
    (addr & mask, mask)
}

/// IPv6 网段归一：(网络地址, 掩码)
fn mask_v6(addr: u128, bits: u8) -> (u128, u128) {
    let mask = if bits == 0 { 0 } else { u128::MAX << (128 - bits) };
    (addr & mask, mask)
}

/// 目标 IP 是否命中任一网段（v4/v6 维度对齐，mapped 已归一）
fn ip_allowed(ip: IpAddr, nets: &[AllowedNet]) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    };
    nets.iter().any(|net| match (net, ip) {
        (AllowedNet::V4(net, mask), IpAddr::V4(a)) => u32::from(a) & mask == *net,
        (AllowedNet::V6(net, mask), IpAddr::V6(a)) => u128::from_be_bytes(a.octets()) & mask == *net,
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    /// CIDR 解析与匹配（IPv4）：网段 / 裸 IP / 全零网段
    #[test]
    fn cidr_parse_and_match_v4() {
        let nets = vec![parse_net("192.168.1.0/24").unwrap()];
        assert!(ip_allowed("192.168.1.55".parse().unwrap(), &nets));
        assert!(!ip_allowed("192.168.2.55".parse().unwrap(), &nets));
        // 裸 IP = /32
        let one = vec![parse_net("10.0.0.5").unwrap()];
        assert!(ip_allowed("10.0.0.5".parse().unwrap(), &one));
        assert!(!ip_allowed("10.0.0.6".parse().unwrap(), &one));
        // /0 放行一切
        let all = vec![parse_net("0.0.0.0/0").unwrap()];
        assert!(ip_allowed("203.0.113.9".parse().unwrap(), &all));
    }

    /// CIDR 解析与匹配（IPv6 / mapped 归一）
    #[test]
    fn cidr_parse_and_match_v6() {
        let nets = vec![parse_net("2001:db8::/32").unwrap()];
        assert!(ip_allowed("2001:db8::1".parse().unwrap(), &nets));
        assert!(!ip_allowed("2001:db9::1".parse().unwrap(), &nets));
        // IPv4-mapped IPv6 目标归一为 v4 匹配
        let mapped: IpAddr = "::ffff:192.168.1.9".parse().unwrap();
        let v4nets = vec![parse_net("192.168.1.0/24").unwrap()];
        assert!(ip_allowed(mapped, &v4nets));
        // mapped 写法的网段（/120 ↔ /24）
        let mapped_net = vec![parse_net("::ffff:192.168.1.0/120").unwrap()];
        assert!(ip_allowed("192.168.1.77".parse().unwrap(), &mapped_net));
    }

    /// 无效条目拒绝；空白/超长前缀均不接受
    #[test]
    fn cidr_parse_rejects_invalid() {
        assert!(parse_net("abc").is_none());
        assert!(parse_net("10.0.0.0/33").is_none());
        assert!(parse_net("10.0.0.5/").is_none());
        assert!(parse_net("").is_none());
    }

    /// 客户端 IP 提取：trust_proxy 关闭时忽略伪造头；开启时反代头优先，XFF 取首项
    #[test]
    fn client_ip_priority() {
        let mut hm = HeaderMap::new();
        hm.insert("x-real-ip", HeaderValue::from_static("1.2.3.4"));
        let peer = Some("10.0.0.1".parse::<IpAddr>().unwrap());
        assert_eq!(client_ip(&hm, peer, false), Some("10.0.0.1".parse().unwrap()));
        assert_eq!(client_ip(&hm, peer, true), Some("1.2.3.4".parse().unwrap()));
        let mut xff = HeaderMap::new();
        xff.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.7, 10.0.0.2"),
        );
        assert_eq!(client_ip(&xff, None, true), Some("203.0.113.7".parse().unwrap()));
    }

    /// config_set：归一化（trim/去重）落盘 + 无效条目整体拒绝
    #[test]
    fn config_set_validates_and_persists() {
        let dir = std::env::temp_dir().join(format!("aiwork_ip_allow_{}", std::process::id()));
        let state = AppState {
            data_dir: dir,
            jwt_refresh_lock: Arc::new(std::sync::Mutex::new(())),
        };
        let saved = config_set(
            &state,
            IpAllowlistConfig {
                enabled: true,
                trust_proxy: false,
                cidrs: vec!["192.168.1.0/24".into(), " 10.0.0.5 ".into(), "192.168.1.0/24".into()],
            },
        )
        .unwrap();
        assert_eq!(saved.cidrs, vec!["192.168.1.0/24", "10.0.0.5"]);
        assert!(config_set(
            &state,
            IpAllowlistConfig { enabled: false, trust_proxy: false, cidrs: vec!["bad".into()] }
        )
        .is_err());
    }
}
