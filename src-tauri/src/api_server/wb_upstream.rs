//! WorkBuddy 上游请求层（T2.1/F-28 + T2.6 双源保活）
//!
//! headers 三铁律（§3.9 ①）：
//! 1. `Origin` + `Referer` 必带，按账号 region 切换（CN=codebuddy.cn 系 /
//!    Global=workbuddy.ai）；缺 Origin/Referer 被网关拒；
//! 2. 缺省字段显式 `X-No-*: 1` 占位（User-Id/Enterprise-Id/Department-Info）；
//! 3. **红线：chat 请求绝不携带 `X-Refresh-Token`**——该头只允许出现在
//!    refresh 端点（配 `X-Auth-Refresh-Source: workbuddy`），带此头触发安全拦截。
//!
//! 上游只回 SSE：非流式由 wb_sse::aggregate 本地聚合（payload 层强制 stream:true）。

use std::io::{BufRead, BufReader, Read};
use std::time::Duration;

use crate::fs_utils;

/// 对话上游 base（按区域）
pub const WB_CHAT_HOST_CN: &str = "https://copilot.tencent.com";
pub const WB_CHAT_HOST_GLOBAL: &str = "https://www.workbuddy.ai";
pub const WB_CHAT_PATH: &str = "/v2/chat/completions";
/// UA 伪装（F-28）
pub const WB_UA: &str = "CLI/2.63.2 CodeBuddy/2.63.2";
/// 刷新端点（红线：唯一允许携带 X-Refresh-Token 的地方）
pub const WB_REFRESH_URL: &str = "https://www.codebuddy.cn/v2/plugin/auth/token/refresh";
/// 首字超时（F-34）：上游建连后 10s 内未产出任何字节 → 故障转移
pub const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Default)]
pub struct WbCreds {
    /// 工具侧账号 id（wb-xxx，token store 键；预留日志/刷新定位用）
    #[allow(dead_code)]
    pub id: String,
    #[allow(dead_code)]
    pub name: String,
    pub uid: String,
    pub token: String,
    /// token domain 字段（决定区域与 X-Domain）
    pub domain: String,
    /// 企业 id（可空 → X-No-Enterprise-Id: 1）
    pub enterprise_id: String,
    /// Global 区：domain 含 `.workbuddy.ai`（F-36 域名路由 §5.2）
    pub global_region: bool,
}

impl WbCreds {
    pub fn chat_base(&self) -> &'static str {
        if self.global_region {
            WB_CHAT_HOST_GLOBAL
        } else {
            WB_CHAT_HOST_CN
        }
    }
}

/// WB SSE Agent：连接 10s / 写 30s / 空闲读 300s
/// （ureq 2.12 未启用 proxy-from-env，直连，不会回环到本网关端口）
pub fn wb_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_read(Duration::from_secs(300))
        .timeout_write(Duration::from_secs(30))
        .timeout_connect(Duration::from_secs(10))
        .max_idle_connections(20)
        .max_idle_connections_per_host(20)
        .build()
}

/// 构建对话请求头（三铁律落点；返回 (header, value) 列表便于断言与复用）
pub fn build_chat_headers(c: &WbCreds) -> Vec<(&'static str, String)> {
    let base = c.chat_base();
    let referer = format!("{}{}", base, WB_CHAT_PATH);
    let h: Vec<(&'static str, String)> = vec![
        ("content-type", "application/json".into()),
        ("accept", "text/event-stream".into()),
        ("user-agent", WB_UA.into()),
        // 铁律 1：Origin/Referer 必带
        ("origin", base.into()),
        ("referer", referer),
        ("X-Requested-With", "XMLHttpRequest".into()),
        ("Authorization", format!("Bearer {}", c.token)),
        // 铁律 2：缺省字段显式 X-No-* 占位
        (
            if c.uid.is_empty() { "X-No-User-Id" } else { "X-User-Id" },
            if c.uid.is_empty() { "1".into() } else { c.uid.clone() },
        ),
        (
            if c.enterprise_id.is_empty() { "X-No-Enterprise-Id" } else { "X-Enterprise-Id" },
            if c.enterprise_id.is_empty() { "1".into() } else { c.enterprise_id.clone() },
        ),
        (
            if c.domain.is_empty() { "X-No-Department-Info" } else { "X-Domain" },
            if c.domain.is_empty() { "1".into() } else { c.domain.clone() },
        ),
        ("X-Product", "SaaS".into()),
    ];
    // 铁律 3（红线）：chat 请求绝不携带 X-Refresh-Token——构造器根本不产出该头，
    // debug_assert 兜底防未来误加。
    debug_assert!(
        !h.iter().any(|(k, _)| k.eq_ignore_ascii_case("x-refresh-token")),
        "红线：chat 请求禁止携带 X-Refresh-Token"
    );
    h
}

/// 上游请求错误：(HTTP 状态 | 502 传输错误, body 摘要, Retry-After 秒)
pub type UpstreamErr = (u16, String, Option<u64>);

/// 发起 WB 对话请求，返回 SSE 流 reader
pub fn make_wb_request(c: &WbCreds, body: &[u8]) -> Result<Box<dyn Read + Send>, UpstreamErr> {
    let url = format!("{}{}", c.chat_base(), WB_CHAT_PATH);
    let mut req = wb_agent().post(&url);
    for (k, v) in build_chat_headers(c) {
        req = req.set(k, &v);
    }
    match req.send_bytes(body) {
        Ok(r) => Ok(Box::new(r.into_reader())),
        Err(ureq::Error::Status(code, resp)) => {
            let retry_after = resp
                .header("retry-after")
                .and_then(|v| v.trim().parse::<u64>().ok());
            let body_text = resp.into_string().unwrap_or_default();
            Err((code, body_text, retry_after))
        }
        Err(e) => {
            let s = format!("{}", e);
            let detail = if s.contains("dns") || s.contains("resolve") || s.contains("name resolution") {
                format!("DNS解析失败（{}），请检查网络: {}", c.chat_base(), e)
            } else if s.contains("timed out") || s.contains("timeout") {
                format!("连接超时（{} 10秒内未响应）: {}", c.chat_base(), e)
            } else if s.contains("tls") || s.contains("certificate") || s.contains("ssl") {
                format!("TLS证书验证失败: {}", e)
            } else {
                format!("传输错误: {}", e)
            };
            Err((502, detail, None))
        }
    }
}

/// 行迭代封装：首行以 FIRST_BYTE_TIMEOUT 超时等待（F-34 首字超时 → 故障转移），
/// 后续行无超时（正常流式出包不受限）。
///
/// 实现：转发线程 + channel。首字超时返回 Err；转发线程 detach（受 Agent
/// 300s 读超时兜底自然退出，不无限泄漏）。
pub fn lines_with_first_byte_timeout<R: Read + Send + 'static>(
    reader: R,
) -> Result<Box<dyn Iterator<Item = String> + Send>, ()> {
    let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<String>>();
    std::thread::spawn(move || {
        let br = BufReader::new(reader);
        for line in br.lines() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    // 首字（可为空行/注释行，均视为"已开始产出"）
    match rx.recv_timeout(FIRST_BYTE_TIMEOUT) {
        Ok(Ok(first)) => {
            let rest = rx.into_iter().filter_map(|r| r.ok());
            Ok(Box::new(std::iter::once(first).chain(rest))
                as Box<dyn Iterator<Item = String> + Send>)
        }
        _ => Err(()),
    }
}

// ==================== T2.6 双源 token 保活（网关 401 路径） ====================

/// token store 进程级写锁（读-改-写分段持锁，网络刷新段不持锁防长阻塞）：
/// 并发刷新不同账号时防止整份 store 互相覆盖丢更新（与 api_keys::KEYS_LOCK 同策略）
static TOKEN_STORE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 网关侧刷新（T2.6）：读 token store → POST refresh → 原子回写工具侧副本。
/// 与 commands::workbuddy::workbuddy_refresh_token 同一端点/红线；此处面向
/// API 网关 401 自动续期（无 Tauri State 依赖，仅 data_dir）。
/// 成功返回新 accessToken；失败返回 Err（调用方按 SwitchKey 换号）。
pub fn refresh_access_token(data_dir: &std::path::Path, account_id: &str) -> Result<String, String> {
    let store_path = data_dir.join("workbuddy_token_store.json");
    let refresh = {
        let _guard = TOKEN_STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let store: serde_json::Value = fs_utils::read_json(&store_path);
        let rec = store
            .get("tokens")
            .and_then(|t| t.get(account_id))
            .cloned()
            .unwrap_or_default();
        rec.get("refresh_token")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or("该账号无 refreshToken（不可刷新，需重新登录）")?
            .to_string()
    };

    // 红线：X-Refresh-Token 仅出现在 refresh 端点
    let resp = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .post(WB_REFRESH_URL)
        .set("Authorization", "Bearer")
        .set("User-Agent", "WorkBuddy")
        .set("X-Refresh-Token", &refresh)
        .set("X-Auth-Refresh-Source", "workbuddy")
        .set("Content-Type", "application/json")
        .send_string("{}");
    let body: serde_json::Value = match resp {
        Ok(r) => r.into_json().unwrap_or_default(),
        Err(ureq::Error::Status(code, _)) => {
            return Err(format!("刷新失败（HTTP {code}）：refresh token 可能已失效，需重新登录"))
        }
        Err(e) => return Err(format!("刷新请求失败: {e}")),
    };
    let dig_str = |v: &serde_json::Value, keys: &[&str]| -> Option<String> {
        keys.iter().find_map(|k| v.get(k).and_then(|x| x.as_str())).map(str::to_string)
    };
    let new_access = dig_str(&body, &["accessToken", "data", "accessToken"])
        .or_else(|| {
            body.get("data")
                .map(|d| dig_str(d, &["accessToken"]))
                .unwrap_or(None)
        })
        .ok_or("刷新响应中无 accessToken")?;
    let new_refresh = dig_str(&body, &["refreshToken"]).or_else(|| {
        body.get("data")
            .map(|d| dig_str(d, &["refreshToken"]))
            .unwrap_or(None)
    });
    let expires_in = body
        .pointer("/expiresIn")
        .or_else(|| body.pointer("/data/expiresIn"))
        .and_then(|v| v.as_i64());
    let refresh_expires_in = body
        .pointer("/refreshExpiresIn")
        .or_else(|| body.pointer("/data/refreshExpiresIn"))
        .and_then(|v| v.as_i64());
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    // 原子回写工具侧副本（F-10 双源谁新用谁：expiresAtMs 更晚者胜出）：
    // 锁内重读最新 store 再合并写回（网络段已释放锁），并发刷新不丢他账号更新
    {
        let _guard = TOKEN_STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut store: serde_json::Value = fs_utils::read_json(&store_path);
        if !store.is_object() {
            store = serde_json::json!({});
        }
        let obj = store.as_object_mut().ok_or("token store 结构异常")?;
        obj.entry("version".to_string()).or_insert(serde_json::json!(1));
        let tokens = obj.entry("tokens".to_string()).or_insert_with(|| serde_json::json!({}));
        if let Some(t) = tokens.as_object_mut() {
            let mut r = t.get(account_id).cloned().unwrap_or(serde_json::json!({}));
            if let Some(rm) = r.as_object_mut() {
                rm.insert("access_token".into(), serde_json::json!(new_access));
                if let Some(nr) = &new_refresh {
                    if !nr.is_empty() {
                        rm.insert("refresh_token".into(), serde_json::json!(nr));
                    }
                }
                if let Some(s) = expires_in {
                    rm.insert("expires_at_ms".into(), serde_json::json!(now_ms + s * 1000));
                }
                if let Some(s) = refresh_expires_in {
                    rm.insert("refresh_expires_at_ms".into(), serde_json::json!(now_ms + s * 1000));
                }
                rm.insert("updated_at".into(), serde_json::json!(fs_utils::now_iso()));
            }
            t.insert(account_id.to_string(), r);
        }
        fs_utils::write_json(&store_path, &store).map_err(|e| format!("回写 token store 失败: {e}"))?;
    }
    Ok(new_access)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds(global: bool, uid: &str, eid: &str, domain: &str) -> WbCreds {
        WbCreds {
            id: "wb-test".into(),
            uid: uid.into(),
            name: "测试".into(),
            token: "tk".into(),
            domain: domain.into(),
            enterprise_id: eid.into(),
            global_region: global,
        }
    }

    #[test]
    fn headers_origin_referer_follow_region() {
        let cn = build_chat_headers(&creds(false, "u", "", ""));
        let base = |h: &[(&str, String)], k: &str| {
            h.iter().find(|(hk, _)| hk.eq_ignore_ascii_case(k)).map(|(_, v)| v.clone()).unwrap()
        };
        assert_eq!(base(&cn, "origin"), "https://copilot.tencent.com");
        assert!(base(&cn, "referer").starts_with("https://copilot.tencent.com/v2/chat/completions"));
        let g = build_chat_headers(&creds(true, "u", "", ""));
        assert_eq!(base(&g, "origin"), "https://www.workbuddy.ai");
    }

    #[test]
    fn headers_no_placeholders_when_missing() {
        let h = build_chat_headers(&creds(false, "", "", ""));
        let get = |k: &str| {
            h.iter().find(|(hk, _)| hk.eq_ignore_ascii_case(k)).map(|(_, v)| v.clone())
        };
        // 铁律 2：缺省字段显式 X-No-* 占位
        assert_eq!(get("X-No-User-Id").as_deref(), Some("1"));
        assert!(get("X-User-Id").is_none());
        assert_eq!(get("X-No-Enterprise-Id").as_deref(), Some("1"));
        assert_eq!(get("X-No-Department-Info").as_deref(), Some("1"));
        // 完整字段时走正头
        let h = build_chat_headers(&creds(false, "u1", "e1", "d1"));
        let get = |k: &str| {
            h.iter().find(|(hk, _)| hk.eq_ignore_ascii_case(k)).map(|(_, v)| v.clone())
        };
        assert_eq!(get("X-User-Id").as_deref(), Some("u1"));
        assert_eq!(get("X-Enterprise-Id").as_deref(), Some("e1"));
        assert_eq!(get("X-Domain").as_deref(), Some("d1"));
        assert_eq!(get("X-Product").as_deref(), Some("SaaS"));
        // UA 伪装
        assert_eq!(get("user-agent").as_deref(), Some(WB_UA));
    }

    #[test]
    fn redline_no_refresh_token_header_in_chat() {
        // 铁律 3：chat 头里绝不出现 X-Refresh-Token
        let c = creds(true, "u", "e", "d");
        let h = build_chat_headers(&c);
        assert!(!h.iter().any(|(k, _)| k.eq_ignore_ascii_case("x-refresh-token")));
        assert!(h.iter().any(|(k, v)| k.eq_ignore_ascii_case("authorization") && v.starts_with("Bearer ")));
    }

    #[test]
    fn refresh_requires_stored_refresh_token() {
        // 空 store → 无 refreshToken → 明确报错（不静默）
        let dir = std::env::temp_dir().join(format!("wb_up_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let r = refresh_access_token(&dir, "wb-none");
        assert!(r.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
