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
    relay_lines(reader, tx);
    // 首字（可为空行/注释行，均视为"已开始产出"）
    match rx.recv_timeout(FIRST_BYTE_TIMEOUT) {
        Ok(Ok(first)) => Ok(chain_rest(first, rx)),
        _ => Err(()),
    }
}

/// reader → channel 行转发线程（首字超时/竞速共用）
fn relay_lines<R: Read + Send + 'static>(
    reader: R,
    tx: std::sync::mpsc::Sender<std::io::Result<String>>,
) {
    std::thread::spawn(move || {
        let br = BufReader::new(reader);
        for line in br.lines() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
}

/// 首行 + 剩余行组装为行迭代器
fn chain_rest(
    first: String,
    rx: std::sync::mpsc::Receiver<std::io::Result<String>>,
) -> Box<dyn Iterator<Item = String> + Send> {
    Box::new(
        std::iter::once(first).chain(rx.into_iter().filter_map(|r| r.ok())),
    ) as Box<dyn Iterator<Item = String> + Send>
}

// ==================== F-76③ 慢请求竞速对冲 ====================

/// 首字竞速结果（F-76③）。泛型 `T` 为对冲侧上下文（wb_route 传入计数租约
/// HedgeLease）：未触发/双败路径在本函数内 Drop（计数自动释放，无泄漏），
/// Ok 路径随结果移交调用方，由调用方在竞速结束后释放（settle_hedge）
pub struct RaceOutcome<T> {
    /// 竞速胜者的行迭代器（含首行）
    pub lines: Box<dyn Iterator<Item = String> + Send>,
    /// 对冲是否接管（true = lines 来自对冲请求；false = 主请求胜出或未触发）
    pub takeover: bool,
    /// 对冲侧上下文：触发过对冲时 Some，未触发为 None
    pub hedge: Option<T>,
}

/// 慢请求竞速对冲（F-76③，P2C 调度理念的请求级延伸）：
///
/// 主请求首字节超过 `hedge_delay_ms` 未到 → 调 `spawn_backup` 取第二账号发起对冲
/// 请求，先出首字者胜、另一侧 reader 被 Drop（连接关闭，被取消一侧不计 usage）。
/// 主请求仍受原 FIRST_BYTE_TIMEOUT 约束（从发起计），对冲请求自带完整首字窗口。
///
/// `hedge_delay_ms` 收敛到 [1s, 8s]：保证首字超时（10s）前留出竞速窗口，
/// 过小会对亚秒抖动误触发，过大则永远轮不到对冲。
///
/// 计数配对保证（F-77）：`spawn_backup` 返回的 `T`（计数租约）在所有退出路径
/// 恰好释放一次——未触发/无对冲账号（闭包返回 None，租约由闭包自释放）、
/// 双败 Err（本函数 Drop）、竞速胜出（移交调用方）
pub fn lines_with_first_byte_hedged<T>(
    primary: Box<dyn Read + Send>,
    hedge_delay_ms: u64,
    mut spawn_backup: impl FnMut() -> Option<(Box<dyn Read + Send>, T)>,
) -> Result<RaceOutcome<T>, ()> {
    use std::sync::mpsc::{Receiver, TryRecvError};
    use std::time::Instant;

    const POLL: Duration = Duration::from_millis(20);
    let hedge_delay = Duration::from_millis(hedge_delay_ms.clamp(1000, 8000));
    let started = Instant::now();
    let (tx1, rx1) = std::sync::mpsc::channel::<std::io::Result<String>>();
    relay_lines(primary, tx1);

    // 阶段一：对冲窗口内主请求先出首字 → 未触发对冲，与原首字超时语义一致
    match rx1.recv_timeout(hedge_delay) {
        Ok(Ok(first)) => {
            return Ok(RaceOutcome {
                lines: chain_rest(first, rx1),
                takeover: false,
                hedge: None,
            })
        }
        // 主请求窗口内即失败（EOF/IO 错误）或窗口耗尽 → 进入对冲路径
        Ok(Err(_)) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
    }

    let Some((backup, hedge)) = spawn_backup() else {
        // 无可用对冲账号（含对冲建连失败，租约已由闭包 Drop 释放）：
        // 退回纯首字超时（剩余窗口）
        let deadline = started + FIRST_BYTE_TIMEOUT;
        return match rx1.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Ok(first)) => Ok(RaceOutcome {
                lines: chain_rest(first, rx1),
                takeover: false,
                hedge: None,
            }),
            _ => Err(()),
        };
    };
    let (tx2, rx2) = std::sync::mpsc::channel::<std::io::Result<String>>();
    relay_lines(backup, tx2);

    // 阶段二：竞速。主请求维持原 10s 上限（started+FIRST_BYTE_TIMEOUT），
    // 对冲请求自带完整 10s 首字窗口；20ms 轮询合并两个 std channel（无 select
    // 原语，轮询开销可忽略）。任一侧 EOF/出错标记 dead；双 dead → Err
    //（hedge 租约随局部变量 Drop 释放——修复双败路径计数泄漏）。
    let primary_deadline = started + FIRST_BYTE_TIMEOUT;
    let hedge_deadline = Instant::now() + FIRST_BYTE_TIMEOUT;
    let mut primary_dead = false;
    let mut hedge_dead = false;
    loop {
        let now = Instant::now();
        if !primary_dead {
            if now >= primary_deadline {
                primary_dead = true; // 超原首字上限：不再考虑主请求
            } else {
                match rx1.try_recv() {
                    Ok(Ok(line)) => {
                        return Ok(RaceOutcome {
                            lines: chain_rest(line, rx1),
                            takeover: false,
                            hedge: Some(hedge),
                        })
                    }
                    Ok(Err(_)) | Err(TryRecvError::Disconnected) => primary_dead = true,
                    Err(TryRecvError::Empty) => {}
                }
            }
        }
        if !hedge_dead {
            if now >= hedge_deadline {
                hedge_dead = true;
            } else {
                match rx2.try_recv() {
                    Ok(Ok(line)) => {
                        return Ok(RaceOutcome {
                            lines: chain_rest(line, rx2),
                            takeover: true,
                            hedge: Some(hedge),
                        })
                    }
                    Ok(Err(_)) | Err(TryRecvError::Disconnected) => hedge_dead = true,
                    Err(TryRecvError::Empty) => {}
                }
            }
        }
        if primary_dead && hedge_dead {
            return Err(());
        }
        // 仅剩一侧：退化为阻塞等待该侧剩余窗口
        if primary_dead {
            return wait_single(rx2, hedge_deadline).map(|lines| RaceOutcome {
                lines,
                takeover: true,
                hedge: Some(hedge),
            });
        }
        if hedge_dead {
            return wait_single(rx1, primary_deadline).map(|lines| RaceOutcome {
                lines,
                takeover: false,
                hedge: Some(hedge),
            });
        }
        std::thread::sleep(POLL);
    }

    fn wait_single(
        rx: Receiver<std::io::Result<String>>,
        deadline: Instant,
    ) -> Result<Box<dyn Iterator<Item = String> + Send>, ()> {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Ok(first)) => Ok(chain_rest(first, rx)),
            _ => Err(()),
        }
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
    // SQLite 化（P4）：死引用修复——原读写 data_dir **根**路径的 workbuddy_token_store.json
    // （正牌在 data/ 子目录，此分叉使网关 401 刷新永远读写错位文件），现统一走 store wb_tokens 表
    let store_db = crate::store::db(data_dir);
    let refresh = {
        let _guard = TOKEN_STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let store: serde_json::Value = crate::store::docs::wb_token_store_load(&store_db);
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
        let mut r = {
            let store: serde_json::Value = crate::store::docs::wb_token_store_load(&store_db);
            store
                .get("tokens")
                .and_then(|t| t.get(account_id))
                .cloned()
                .unwrap_or(serde_json::json!({}))
        };
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
        crate::store::docs::wb_token_store_upsert(&store_db, account_id, &r)
            .map_err(|e| format!("回写 token store 失败: {e}"))?;
    }
    Ok(new_access)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

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

    // ==================== F-76③ 竞速对冲 ====================

    /// 首字节延迟后吐出固定文本的 reader（模拟慢上游）
    struct DelayedRead {
        delay: Option<Duration>,
        data: std::io::Cursor<Vec<u8>>,
    }
    impl DelayedRead {
        fn new(delay_ms: u64, text: &str) -> Self {
            Self {
                delay: if delay_ms == 0 { None } else { Some(Duration::from_millis(delay_ms)) },
                data: std::io::Cursor::new(text.as_bytes().to_vec()),
            }
        }
    }
    impl Read for DelayedRead {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if let Some(d) = self.delay.take() {
                std::thread::sleep(d);
            }
            self.data.read(buf)
        }
    }

    fn collect(lines: Box<dyn Iterator<Item = String> + Send>) -> Vec<String> {
        lines.collect()
    }

    /// Drop 副作用探针：验证对冲上下文 T 的 RAII 配对契约
    struct DropProbe(Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    #[test]
    fn hedge_not_fired_when_primary_fast() {
        // 主请求亚秒出首字 → 未触发对冲（hedge None），内容完整
        let out: RaceOutcome<&str> = lines_with_first_byte_hedged(
            Box::new(DelayedRead::new(0, "data: a\ndata: b\n")),
            2000,
            || None,
        )
        .unwrap();
        assert!(out.hedge.is_none());
        assert!(!out.takeover);
        assert_eq!(collect(out.lines), vec!["data: a".to_string(), "data: b".to_string()]);
    }

    #[test]
    fn hedge_takeover_when_primary_slow() {
        // 主请求 3s 无首字（>1s 对冲窗口）→ 备份立即出首字 → 对冲接管
        let out: RaceOutcome<&str> = lines_with_first_byte_hedged(
            Box::new(DelayedRead::new(3000, "data: slow\n")),
            1000,
            || Some((Box::new(DelayedRead::new(0, "data: fast\n")), "hedge-uid")),
        )
        .unwrap();
        assert_eq!(out.hedge, Some("hedge-uid"));
        assert!(out.takeover);
        assert_eq!(collect(out.lines), vec!["data: fast".to_string()]);
    }

    #[test]
    fn hedge_lost_when_primary_recovers() {
        // 主请求慢但先于备份出首字（备份 3s、主 1.5s，均在窗口内）→ 主胜出
        let out: RaceOutcome<&str> = lines_with_first_byte_hedged(
            Box::new(DelayedRead::new(1500, "data: primary\n")),
            1000,
            || Some((Box::new(DelayedRead::new(3000, "data: backup\n")), "hedge-uid")),
        )
        .unwrap();
        assert_eq!(out.hedge, Some("hedge-uid"));
        assert!(!out.takeover);
        assert_eq!(collect(out.lines), vec!["data: primary".to_string()]);
    }

    #[test]
    fn hedge_fallback_without_backup() {
        // 无可用对冲账号 → 主请求退回纯首字超时窗口（1.2s < 10s 仍成功）
        let out: RaceOutcome<&str> = lines_with_first_byte_hedged(
            Box::new(DelayedRead::new(1200, "data: solo\n")),
            1000,
            || None,
        )
        .unwrap();
        assert!(out.hedge.is_none());
        assert!(!out.takeover);
        assert_eq!(collect(out.lines), vec!["data: solo".to_string()]);
    }

    #[test]
    fn hedge_err_when_both_eof() {
        // 主/对冲均立即 EOF → 双败 Err（泛型版语义回归）
        let out: Result<RaceOutcome<&str>, ()> = lines_with_first_byte_hedged(
            Box::new(std::io::empty()),
            500,
            || Some((Box::new(std::io::empty()), "hedge-uid")),
        );
        assert!(out.is_err());
    }

    #[test]
    fn hedge_ctx_dropped_on_double_failure() {
        // P0 泄漏修复契约：双败路径对冲上下文必须在函数内 Drop（计数释放恰好一次）
        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let d2 = drops.clone();
        let out: Result<RaceOutcome<DropProbe>, ()> = lines_with_first_byte_hedged(
            Box::new(std::io::empty()),
            500,
            move || Some((Box::new(std::io::empty()), DropProbe(d2.clone()))),
        );
        assert!(out.is_err());
        assert_eq!(
            drops.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "双败路径对冲上下文必须恰好 Drop 一次"
        );
    }

    #[test]
    fn hedge_ctx_transferred_on_win() {
        // P0 泄漏修复契约：竞速胜出时上下文随 RaceOutcome 移交调用方，
        // 调用方 drop 后释放恰好一次（wb_route 在 settle_hedge 落定）
        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let d2 = drops.clone();
        let out = lines_with_first_byte_hedged(
            Box::new(DelayedRead::new(3000, "data: slow\n")),
            1000,
            move || Some((Box::new(DelayedRead::new(0, "data: fast\n")), DropProbe(d2.clone()))),
        )
        .unwrap();
        assert_eq!(drops.load(std::sync::atomic::Ordering::Relaxed), 0, "胜出前上下文仍在途");
        drop(out);
        assert_eq!(drops.load(std::sync::atomic::Ordering::Relaxed), 1, "调用方落定后释放恰好一次");
    }
}
