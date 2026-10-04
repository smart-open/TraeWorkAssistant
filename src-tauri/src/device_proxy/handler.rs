//! MITM 解密请求处理（device_proxy.py `tunnel_https` / `forward_upstream` 迁移，P4-5）。
//!
//! 流程（对齐 Python 版）：
//! 1. 读原始请求（head + Content-Length/Chunked body）
//! 2. JWT 自动捕获（authorization / x-cloudide-token / x-icube-token，不限 host）→ 写回 accounts
//! 3. 签到接口头改写（注入按账号派生的 x-device-id / x-market-user-id / vscode-sessionid）
//! 4. WebSocket 升级 → 交由 [`ws`] 模块接管
//! 5. 其余转发上游：非流式整体缓冲 + 凭据抓取（refresh_token / 豆包会话）；
//!    流式（SSE）逐块 chunked 转发 + 全量摘要落日志

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use http_body_util::{BodyExt, Full, Limited};
use hyper::Request;
use hyper_util::client::legacy::Client;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;

use crate::device_proxy::logger::{ProxyLog, RequestLogger};
use crate::device_proxy::upstream::UpstreamConnector;

/// 建连/首读超时（对齐 Python `_CONN_TIMEOUT`）
const CONN_TIMEOUT: Duration = Duration::from_secs(300);
/// 流式请求上游超时（对齐 Python forward_upstream is_stream 分支）
const STREAM_TIMEOUT: Duration = Duration::from_secs(300);
/// 非流式请求上游超时
const PLAIN_TIMEOUT: Duration = Duration::from_secs(30);
/// 请求头缓冲上限（对齐 Python recv_until max_size=10MB）
const MAX_HEAD: usize = 10 * 1024 * 1024;
/// 请求体上限（Python 无显式上限，此处防御性收紧；正常 API 请求远小于该值）
const MAX_BODY: usize = 64 * 1024 * 1024;
/// 非流式响应体缓冲上限（Python 无显式上限，此处防御性；流式路径逐块转发不整体缓冲）
pub(crate) const MAX_RESP_BODY: usize = 256 * 1024 * 1024;
/// 流式响应日志缓冲上限（对齐 Python MAX_LOG_BODY）
const MAX_LOG_BODY: usize = 10 * 1024 * 1024;

/// 签到领取接口（请求头改写命中路径）
pub const SIGNIN_PATH: &str = "/trae/api/v2/ug/checkin_credits/claim";

/// 已知 TRAE 接口（path 子串匹配 → 日志用途标签），对齐 Python `KNOWN_TRAE_PATHS`
const KNOWN_TRAE_PATHS: &[(&str, &str)] = &[
    ("/trae/api/v2/ug/checkin_credits/claim", "签到领取"),
    ("/trae/api/v2/ug/checkin_credits/status", "签到状态"),
    ("/cloudide/api", "CloudIDE网关"),
    ("/oauth/", "OAuth"),
    ("ExchangeToken", "换Token"),
    ("ide_user_ent_usage", "积分用量"),
    ("/api/agent/v3/llm_utils_chat", "大模型对话"),
    ("/api/ide/v1/get_detail_param", "模型列表"),
    ("/api/remote/v1/plugins", "插件接口"),
    ("/api/remote/v1/skills", "技能列表"),
];

/// 请求侧跳过头（对齐 Python HOP_BY_HOP）
pub(crate) const HOP_BY_HOP_REQ: &[&str] = &[
    "proxy-connection",
    "connection",
    "keep-alive",
    "proxy-authorization",
    "host",
    "content-length",
];

/// 响应侧跳过头（对齐 Python send_response/_stream_response 过滤集）
const HOP_BY_HOP_RESP: &[&str] = &["transfer-encoding", "connection", "keep-alive", "content-length"];

/// accounts 写回互斥：防止并发连接同时读改写 JSON（Python 用 RLock，等价保护）
static ACCOUNTS_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
/// 豆包凭证进程内去重缓存 + 落盘锁（对齐 Python _doubao_captured_cache/_capture_lock）
static DOUBAO_LOCK: OnceLock<Mutex<Option<serde_json::Value>>> = OnceLock::new();

pub(crate) fn accounts_lock() -> &'static Mutex<()> {
    ACCOUNTS_LOCK.get_or_init(|| Mutex::new(()))
}

fn doubao_cache() -> &'static Mutex<Option<serde_json::Value>> {
    DOUBAO_LOCK.get_or_init(|| Mutex::new(None))
}

/// 代理共享上下文：路径/开关/日志句柄，由 mod.rs 主循环构造并贯穿各模块
pub struct ProxyCtx {
    pub log: ProxyLog,
    pub req_logger: Arc<RequestLogger>,
    /// 监听域名列表（PROXY_DOMAINS，小写）
    pub targets: Vec<String>,
    /// 自动捕获 JWT 写回 accounts（AUTO_CAPTURE_JWT，默认开）
    pub auto_capture_jwt: bool,
    /// 数据根目录（SQLite 化 P3：accounts/cooldowns/凭证快照均经 store 读写）
    pub data_dir: PathBuf,
    /// 上游代理（用户 VPN）：配置后所有转发流量（MITM/WS/隧道）一律先经上游，
    /// 失败回退直连 —— 镜像客户端无本代理时的正常出口路径（见 upstream.rs 模块注释）
    pub upstream: Option<crate::device_proxy::upstream::UpstreamProxy>,
    /// 直连白名单域（p3-2e，后缀匹配）：命中域跳过上游 VPN 直接连目标
    ///（qoder 国内域经 VPN 实测全挂、直连恢复），供 WS 路径与隧道判定
    pub direct_domains: Arc<Vec<String>>,
    /// 自适应证书锁定降级：host → (握手成功数, 握手被中止数, 已判定锁定)。
    /// 失败率判定（非连续计数）：投机性预连接风暴（客户端并发开连接再裁撤，
    /// 握手 RST）会造成「连续 3 次失败」的假阳性，把正常域误降级为直通（实测
    /// www/accounts.doubao.com 被误降级、Cookie 捕获停摆）；样本 ≥5 且失败率
    /// >75% 才判锁定，混布域（webview 成功 + cronet 失败）永不误判。重启复位。
    pub pin_state: Mutex<HashMap<String, (u32, u32, bool)>>,
}

/// 判定证书锁定的最小失败样本数
pub const PIN_MIN_FAILS: u32 = 5;
/// 判定证书锁定的最大成功/失败比（成功数低于失败数的 25% 才判锁定）
pub const PIN_MAX_OK_RATIO: f64 = 0.25;

impl ProxyCtx {
    /// 域名后缀匹配（对齐 Python host_in_targets）
    pub fn host_in_targets(&self, host: &str) -> bool {
        let h = host.to_ascii_lowercase();
        self.targets
            .iter()
            .any(|d| h == *d || h.ends_with(&format!(".{d}")))
    }

    /// 握手成功：成功计数 +1（混布域——webview 成功/cronet 失败——永不误判）
    pub fn note_handshake_ok(&self, host: &str) {
        let mut m = self.pin_state.lock().unwrap_or_else(|e| e.into_inner());
        let e = m.entry(host.to_string()).or_insert((0, 0, false));
        e.0 = e.0.saturating_add(1);
    }

    /// 握手被客户端中止：失败计数 +1，返回 true 表示刚判定为锁定
    pub fn note_handshake_fail(&self, host: &str) -> bool {
        let mut m = self.pin_state.lock().unwrap_or_else(|e| e.into_inner());
        let e = m.entry(host.to_string()).or_insert((0, 0, false));
        e.1 = e.1.saturating_add(1);
        if !e.2 && e.1 >= PIN_MIN_FAILS && (e.0 as f64) < (e.1 as f64) * PIN_MAX_OK_RATIO {
            e.2 = true;
            return true;
        }
        false
    }

    /// 该域是否已判定证书锁定（CONNECT 转透明直通；判定后本会话保持）
    pub fn is_pinned(&self, host: &str) -> bool {
        self.pin_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(host)
            .map(|e| e.2)
            .unwrap_or(false)
    }

    /// path 子串匹配已知接口（对齐 Python classify_path）
    pub fn classify_path(&self, path: &str) -> Option<&'static str> {
        KNOWN_TRAE_PATHS
            .iter()
            .find(|(sub, _)| path.contains(sub))
            .map(|(_, name)| *name)
    }
}

// ---------------- 原始请求读取（客户端侧） ----------------

/// 解密后的原始 HTTP/1.1 请求
pub struct RawRequest {
    pub method: String,
    pub path: String,
    /// 原始大小写头对（可重复；Set-Cookie 等多值头不折叠）
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl RawRequest {
    /// 大小写不敏感取头（同名字段取最后一次出现，对齐 Python dict 语义）
    pub fn hget(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .rev()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// 替换头（移除同名大小写变体后追加）
    pub fn hset(&mut self, name: &str, value: String) {
        self.headers.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
        self.headers.push((name.to_string(), value));
    }

    /// WebSocket 升级检测（对齐 Python is_websocket_upgrade）
    pub fn is_websocket_upgrade(&self) -> bool {
        self.hget("upgrade").map(|v| v.eq_ignore_ascii_case("websocket")).unwrap_or(false)
    }
}

/// 读一条原始请求：head 直到 \r\n\r\n + body（Content-Length / Chunked）。
/// 返回 Ok(None) 表示客户端已关闭（EOF）。头超限视为连接异常（记日志后断开）。
pub async fn read_raw_request<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<RawRequest>, String> {
    read_raw_request_buf(r, BytesMut::new()).await
}

/// 带初始缓冲的变体：连接分发阶段已读取的字节注入后继续解析
///（mod.rs 明文路径在判路由时已消耗部分流字节，由此续接；缓冲可能含 body 前缀）
pub async fn read_raw_request_buf<R: AsyncRead + Unpin>(
    r: &mut R,
    mut buf: BytesMut,
) -> Result<Option<RawRequest>, String> {
    let head_end = loop {
        if let Some(pos) = find_head_end(&buf) {
            break pos;
        }
        if buf.len() > MAX_HEAD {
            return Err(format!("请求头缓冲区超限 ({})", buf.len()));
        }
        let mut chunk = [0u8; 8192];
        let n = r
            .read(&mut chunk)
            .await
            .map_err(|e| format!("读取请求头失败: {e}"))?;
        if n == 0 {
            return Ok(None); // EOF
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = buf.split_to(head_end + 4).freeze();
    let rest = buf;

    let head_str = String::from_utf8_lossy(&head);
    let mut lines = head_str.split("\r\n");
    let first = lines.next().unwrap_or("");
    let mut parts = first.split(' ');
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();
    let _version = parts.next().unwrap_or("HTTP/1.1");

    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let mut req = RawRequest { method, path, headers, body: Bytes::new() };

    let lookup = |name: &str| {
        req.headers
            .iter()
            .rev()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };

    // Content-Length 优先；Chunked 降级解析（Python 版不处理 Chunked 请求体，此处增强）
    if let Some(te) = lookup("transfer-encoding") {
        if te.to_ascii_lowercase().contains("chunked") {
            req.body = read_chunked_body(r, rest).await?;
            return Ok(Some(req));
        }
    }
    let cl: usize = lookup("content-length")
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    if cl > MAX_BODY {
        return Err(format!("请求体超限 ({cl} > {MAX_BODY})"));
    }
    let mut body = rest;
    while body.len() < cl {
        let mut chunk = [0u8; 8192];
        let n = r.read(&mut chunk).await.map_err(|e| format!("读取请求体失败: {e}"))?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(cl);
    req.body = body.freeze();
    Ok(Some(req))
}

/// 定位 \r\n\r\n（返回 head 部分结束下标，即最后一个 \r 的下标）
pub(crate) fn find_head_end(buf: &[u8]) -> Option<usize> {
    if buf.len() < 4 {
        return None;
    }
    (0..=buf.len() - 4).find(|&i| &buf[i..i + 4] == b"\r\n\r\n")
}

/// Chunked 请求体解码（head 之后的剩余字节为初始输入）
async fn read_chunked_body<R: AsyncRead + Unpin>(r: &mut R, mut buf: BytesMut) -> Result<Bytes, String> {
    let mut out = BytesMut::new();
    loop {
        // 读取一行 chunk size
        let line = loop {
            if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                break buf.split_to(pos + 1);
            }
            if buf.len() > 1024 * 1024 {
                return Err("chunk size 行超限".to_string());
            }
            let mut chunk = [0u8; 4096];
            let n = r.read(&mut chunk).await.map_err(|e| format!("读取 chunk 失败: {e}"))?;
            if n == 0 {
                return Err("chunked body 意外 EOF".to_string());
            }
            buf.extend_from_slice(&chunk[..n]);
        };
        let size_str = String::from_utf8_lossy(&line);
        let size = usize::from_str_radix(size_str.trim().split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| format!("非法 chunk size: {size_str}"))?;
        if size == 0 {
            // 终结块后的 trailer 不再消费（极少见，Python 版同样不处理 chunked 请求）
            return Ok(out.freeze());
        }
        if out.len() + size > MAX_BODY {
            return Err("chunked 请求体超限".to_string());
        }
        while buf.len() < size + 2 {
            let mut chunk = [0u8; 8192];
            let n = r.read(&mut chunk).await.map_err(|e| format!("读取 chunk 数据失败: {e}"))?;
            if n == 0 {
                return Err("chunked body 意外 EOF".to_string());
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let mut data = buf.split_to(size + 2);
        data.truncate(size);
        out.extend_from_slice(&data);
    }
}

// ---------------- 响应发送（客户端侧） ----------------

/// 逐帧读取上游响应体（Limited 上限 + 每帧空闲超时）。
/// 对齐 Python socket timeout 的「逐读」语义（差异修复：原 collect() 仅在建立请求段
/// 有超时，读体阶段无逐读超时——僵死上游 trickling body 可无限拖住连接与并发槽）。
pub(crate) async fn collect_body_with_idle_timeout(
    body: Limited<hyper::body::Incoming>,
    idle: Duration,
) -> Result<Bytes, String> {
    let mut body = body;
    let mut buf = BytesMut::new();
    loop {
        match timeout(idle, body.frame()).await {
            Err(_) => return Err(format!("读上游响应体空闲超时 ({}s)", idle.as_secs())),
            Ok(None) => return Ok(buf.freeze()),
            Ok(Some(Err(e))) => return Err(format!("读上游响应失败: {e}")),
            Ok(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    buf.extend_from_slice(data);
                }
            }
        }
    }
}

/// 缓冲式响应回写（对齐 Python send_response）：保留重复头（Set-Cookie 多值），
/// 重写 Content-Length 并强制 keep-alive。
pub(crate) async fn send_response<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    status: u16,
    reason: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
    for (k, v) in headers {
        if HOP_BY_HOP_RESP.iter().any(|h| k.eq_ignore_ascii_case(h)) {
            continue;
        }
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    head.push_str("Connection: keep-alive\r\n\r\n");
    w.write_all(head.as_bytes()).await?;
    w.write_all(body).await?;
    w.flush().await
}

/// HTTP 状态码标准短语（hyper 不透出上游 reason，取标准值即可，客户端仅展示用）
pub(crate) fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "",
    }
}

// ---------------- JWT 校验 / 提取 ----------------

/// 校验 Cloud-IDE-JWT：3 段、header alg=RS256、payload data.id 存在。
/// 通过则返回规范化 `Cloud-IDE-JWT <jwt>`（对齐 Python _valid_cloud_ide_jwt）。
pub fn valid_cloud_ide_jwt(tok: &str) -> Option<String> {
    use base64::Engine;
    let parts: Vec<&str> = tok.trim().split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let decode = |s: &str| -> Option<serde_json::Value> {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(s.trim_end_matches('='))
            .ok()?;
        serde_json::from_slice(&bytes).ok()
    };
    let header = decode(parts[0])?;
    let payload = decode(parts[1])?;
    let alg_ok = header.get("alg").and_then(|v| v.as_str()) == Some("RS256");
    let has_data_id = payload
        .get("data")
        .and_then(|d| d.get("id"))
        .map(|v| !v.is_null())
        .unwrap_or(false);
    if alg_ok && has_data_id {
        Some(format!("Cloud-IDE-JWT {}", tok.trim()))
    } else {
        None
    }
}

/// 从鉴权头提取 user_id（data.id → auth_id → sub；对齐 Python extract_user_id）
pub fn extract_user_id(auth_header: &str) -> Option<String> {
    crate::jwt::parse(auth_header).user_id
}

/// JWT exp 时间戳（秒）；解析失败 None
fn jwt_exp_ts(jwt_full: &str) -> Option<i64> {
    crate::jwt::parse(jwt_full).exp_timestamp
}

// ---------------- accounts / cooldowns 写回 ----------------

/// 按 user_id 更新/追加账号 JWT（对齐 Python update_account_jwt 语义：
/// exp 防降级；找不到则追加 auto_<uid8>；更新成功自动解除冷却）
pub fn update_account_jwt(ctx: &ProxyCtx, user_id: &str, jwt_full: &str) -> &'static str {
    update_account_jwt_and_refresh(ctx, user_id, jwt_full, None)
}

/// JWT 与（可选）refresh_token 一次持锁原子更新 + 单次落盘
///（审查修复「两段锁」：原 update_account_jwt + update_account_refresh_token
/// 分两次持锁写盘，间隙会落盘「JWT 已更新 / refresh_token 未更新」的中间态，
/// 并发读（签到/另一次捕获）会看到半更新数据）。
/// 语义对齐 Python 两函数合用：JWT exp 防降级（skipped 不阻塞 refresh 更新）、
/// refresh 同值 unchanged、追加账号时两者一并写入。
fn update_account_jwt_and_refresh(
    ctx: &ProxyCtx,
    user_id: &str,
    jwt_full: &str,
    refresh_token: Option<&str>,
) -> &'static str {
    let _g = accounts_lock().lock().unwrap_or_else(|e| e.into_inner());
    // SQLite 化（P3）：raw 读 accounts 表（保留扩展字段；沿用原 JSON 处理逻辑）
    let mut cfg: serde_json::Value =
        crate::store::docs::accounts_load_raw(&crate::store::db(&ctx.data_dir));
    if !cfg.get("accounts").map(|a| a.is_array()).unwrap_or(false) {
        cfg["accounts"] = serde_json::Value::Array(vec![]);
    }
    let accounts = cfg
        .get_mut("accounts")
        .and_then(|a| a.as_array_mut())
        .expect("accounts array");
    let new_exp = jwt_exp_ts(jwt_full);
    let exp_str = |ts: Option<i64>| {
        ts.and_then(|t| chrono::DateTime::from_timestamp(t, 0))
            .map(|dt| dt.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "?".to_string())
    };
    let new_exp_str = exp_str(new_exp);
    let uid_of = |v: &serde_json::Value| -> String {
        match v {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            _ => String::new(),
        }
    };

    let idx = accounts.iter().position(|a| {
        a.get("UserID")
            .or_else(|| a.get("user_id"))
            .map(|v| uid_of(v) == user_id)
            .unwrap_or(false)
    });

    if let Some(i) = idx {
        let (old_jwt, name) = {
            let acc = &accounts[i];
            (
                acc.get("jwt").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                acc.get("name").and_then(|v| v.as_str()).unwrap_or("?").to_string(),
            )
        };
        let mut changed = false;
        let mut jwt_status = "unchanged";
        if old_jwt != jwt_full {
            // 防降级：新 token 过期时间不晚于旧的 → 跳过 JWT（uid= 记法避免误触发捕获事件）
            if let (Some(old_ts), Some(new_ts)) = (jwt_exp_ts(&old_jwt), new_exp) {
                if new_ts <= old_ts {
                    ctx.log.log(&format!(
                        "  [JWT 跳过(更旧)] uid={user_id} 账号={name} 旧 exp={} 新 exp={new_exp_str}",
                        exp_str(Some(old_ts))
                    ));
                    jwt_status = "skipped";
                }
            }
            if jwt_status != "skipped" {
                accounts[i]["jwt"] = serde_json::Value::String(jwt_full.to_string());
                accounts[i]["updated_at"] = serde_json::Value::String(crate::fs_utils::now_iso());
                ctx.log.log(&format!("  [JWT 自动更新] user={user_id} 账号={name} exp={new_exp_str}"));
                changed = true;
                jwt_status = "updated";
            }
        }
        if let Some(rt) = refresh_token {
            if accounts[i].get("refresh_token").and_then(|v| v.as_str()) != Some(rt) {
                accounts[i]["refresh_token"] = serde_json::Value::String(rt.to_string());
                accounts[i]["refresh_token_updated_at"] =
                    serde_json::Value::String(crate::fs_utils::now_iso());
                ctx.log.log(&format!("  [refresh_token 更新] uid={user_id} 账号={name}"));
                changed = true;
            }
        }
        if changed {
            if write_accounts(ctx, &cfg).is_err() {
                ctx.log.log("  [accounts] 写入失败");
            }
            if jwt_status == "updated" {
                clear_cooldown(ctx, user_id);
            }
        }
        return jwt_status;
    }
    // 新账号追加（refresh_token 一并写入，省去追加后二次写盘；对齐 Python 两步净效果）
    let short: String = user_id.chars().take(8).collect();
    let mut new_acc = serde_json::json!({
        "name": format!("auto_{short}"),
        "UserID": user_id,
        "jwt": jwt_full,
        "added_at": crate::fs_utils::now_iso(),
    });
    if let Some(rt) = refresh_token {
        new_acc["refresh_token"] = serde_json::Value::String(rt.to_string());
        new_acc["refresh_token_updated_at"] = serde_json::Value::String(crate::fs_utils::now_iso());
    }
    accounts.push(new_acc);
    ctx.log
        .log(&format!("  [JWT 自动追加新账号] user={user_id} -> name=auto_{short} exp={new_exp_str}"));
    if write_accounts(ctx, &cfg).is_err() {
        ctx.log.log("  [accounts] 写入失败");
    }
    "appended"
}

fn write_accounts(ctx: &ProxyCtx, cfg: &serde_json::Value) -> Result<(), String> {
    // SQLite 化（P3）：raw 写 accounts 表（保留扩展字段如 refresh_token_updated_at）
    crate::store::docs::accounts_save_raw(&crate::store::db(&ctx.data_dir), cfg)
}

/// 新 JWT 捕获成功 → 自动解除该账号冷却（对齐 Python clear_cooldown 意图；
/// Python 版实为 NameError 空转，此处为真实修复）
fn clear_cooldown(ctx: &ProxyCtx, user_id: &str) {
    // SQLite 化（P3）：冷却状态经 store 读写
    let mut cd = crate::store::docs::account_cooldowns_load(&crate::store::db(&ctx.data_dir));
    if cd.cooldowns.remove(user_id).is_some() {
        cd.updated_at = Some(crate::fs_utils::now_iso());
        if crate::store::docs::account_cooldowns_save(&crate::store::db(&ctx.data_dir), &cd).is_ok() {
            ctx.log.log(&format!(
                "  [冷却解除] uid={user_id}（新 JWT 捕获成功，自动解除登录失效标记）"
            ));
        }
    }
}

// ---------------- ExchangeToken refresh_token 抓取 ----------------

/// 从 ExchangeToken/OAuth 响应体提取 refresh_token 并写回（对齐 Python
/// try_capture_refresh_token_from_response）
fn try_capture_refresh_token(ctx: &ProxyCtx, path: &str, resp_body: &[u8]) {
    if resp_body.is_empty() || !resp_body.windows(13).any(|w| w == b"refresh_token") {
        return;
    }
    if !(path.contains("ExchangeToken") || path.to_ascii_lowercase().contains("oauth")) {
        return;
    }
    let Ok(data) = serde_json::from_slice::<serde_json::Value>(resp_body) else {
        return;
    };
    let inner = data.get("data").filter(|v| v.is_object()).unwrap_or(&data);
    let Some(rt) = inner.get("refresh_token").and_then(|v| v.as_str()) else {
        return;
    };
    let at = inner
        .get("access_token")
        .and_then(|v| v.as_str())
        .or_else(|| inner.get("token").and_then(|v| v.as_str()))
        .unwrap_or("");
    if at.is_empty() {
        ctx.log.log("  [refresh_token] 响应中含 refresh_token 但无法提取 user_id，跳过");
        return;
    }
    // 已带前缀的 access_token 直接采信（对齐 Python），否则校验
    let validated = if at.starts_with("Cloud-IDE-JWT") {
        at.to_string()
    } else {
        match valid_cloud_ide_jwt(at) {
            Some(v) => v,
            None => {
                ctx.log.log("  [refresh_token] 响应中含 refresh_token 但无法提取 user_id，跳过");
                return;
            }
        }
    };
    let Some(uid) = extract_user_id(&validated) else {
        ctx.log.log("  [refresh_token] 响应中含 refresh_token 但无法提取 user_id，跳过");
        return;
    };
    // 一次持锁同时更新 JWT 与 refresh_token（审查修复：两段锁间隙会落盘半更新中间态）
    update_account_jwt_and_refresh(ctx, &uid, &validated, Some(rt));
}

// ---------------- 豆包会话凭证抓取 ----------------

/// Cookie 请求头宽松解析（对齐 Python _parse_cookie_header）
fn parse_cookie_header(cookie: &str) -> Vec<(String, String)> {
    cookie
        .split(';')
        .filter_map(|part| {
            let (k, v) = part.split_once('=')?;
            Some((k.trim().to_string(), v.trim().trim_matches('"').to_string()))
        })
        .collect()
}

/// 从 multi_sids cookie 解析当前登录 uid（对齐 Python _doubao_uid_from_multi_sids）
fn doubao_uid_from_multi_sids(cookie: &str, session_id: &str) -> String {
    // OnceLock 单次编译（审查修复：原每次调用重编译正则）
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"multi_sids=([^;\s]+)").expect("multi_sids regex"));
    let Some(m) = re.captures(cookie) else {
        return String::new();
    };
    let raw = urlencoding::decode(m.get(1).map(|g| g.as_str()).unwrap_or(""))
        .unwrap_or_default()
        .to_string();
    for pair in raw.split(['|', ';']) {
        if let Some((uid, sid)) = pair.split_once(':') {
            let uid = uid.trim();
            if uid.chars().all(|c| c.is_ascii_digit()) && !uid.is_empty() && sid.trim() == session_id {
                return uid.to_string();
            }
        }
    }
    String::new()
}

/// doubao.com 域请求 Cookie 中提取会话凭证，变化时写抓包文件（对齐 Python
/// try_capture_doubao_credentials）
fn try_capture_doubao_credentials(ctx: &ProxyCtx, host: &str, req_headers: &[(String, String)], resp_headers: &[(String, String)]) {
    const CRED_COOKIES: &[&str] = &["sessionid", "sid_guard", "ttwid"];
    let host_l = host.to_ascii_lowercase();
    if !host_l.ends_with(".doubao.com") {
        return;
    }
    let cookie = req_headers
        .iter()
        .rev()
        .find(|(k, _)| k.eq_ignore_ascii_case("cookie"))
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let mut jar = parse_cookie_header(&cookie);
    // 兜底：响应 Set-Cookie 中补齐请求缺失的凭证 cookie
    for (k, v) in resp_headers {
        if !k.eq_ignore_ascii_case("set-cookie") {
            continue;
        }
        let first = v.split(';').next().unwrap_or("");
        if let Some((ck, cv)) = first.split_once('=') {
            let ck = ck.trim();
            if CRED_COOKIES.contains(&ck) && !jar.iter().any(|(k, _)| k == ck) {
                jar.push((ck.to_string(), cv.trim().to_string()));
            }
        }
    }
    let get = |name: &str| jar.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()).unwrap_or_default();
    let session_id = get("sessionid");
    if session_id.is_empty() {
        return;
    }
    let sid_guard = get("sid_guard");
    let ttwid = get("ttwid");
    let uid = doubao_uid_from_multi_sids(&cookie, &session_id);
    let captured = serde_json::json!({
        "session_id": session_id,
        "sid_guard": sid_guard,
        "ttwid": ttwid,
        "uid": uid,
        "host": host_l,
        "captured_at": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
    });
    let mut cache = doubao_cache().lock().unwrap_or_else(|e| e.into_inner());
    let unchanged = cache
        .as_ref()
        .map(|c| {
            c.get("session_id") == captured.get("session_id")
                && c.get("sid_guard") == captured.get("sid_guard")
                && c.get("ttwid") == captured.get("ttwid")
                && c.get("uid") == captured.get("uid")
        })
        .unwrap_or(false);
    if unchanged {
        return;
    }
    *cache = Some(captured.clone());
    // SQLite 化（P3）：凭证快照 → kv `doubao_captured_credentials`
    let _ = crate::store::db(&ctx.data_dir).kv_set("doubao_captured_credentials", &captured);
    ctx.log.log(&format!(
        "  [doubao] 抓到会话凭证: sessionid={} 字符{}{}{}",
        session_id.len(),
        if sid_guard.is_empty() { String::new() } else { format!("，sid_guard={} 字符", sid_guard.len()) },
        if ttwid.is_empty() { String::new() } else { format!("，ttwid={} 字符", ttwid.len()) },
        if uid.is_empty() { "（uid 未识别）".to_string() } else { format!("，uid={uid}") },
    ));
}

// ---------------- 上游转发 ----------------

/// 提取 hyper 错误的完整 source 链（诊断修复）：legacy Client 的 Display 只有
/// "client error (Connect)" 一层，根因（DNS 失败/连接超时/拒绝）藏在 source 链里，
/// 不展开时 proxy.log 无法定位白屏类问题（实测 47~97s 停滞只见 Connect 字样）
pub(crate) fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut msgs = vec![e.to_string()];
    let mut src = e.source();
    while let Some(s) = src {
        let m = s.to_string();
        if msgs.last() != Some(&m) {
            msgs.push(m);
        }
        src = s.source();
    }
    msgs.join(" <- ")
}

/// 一次转发结果：false 表示上游要求关闭连接或出错（对齐 Python keep-alive 语义）
async fn forward_upstream<S: AsyncRead + AsyncWrite + Unpin>(
    io: &mut tokio_rustls::server::TlsStream<S>,
    client: &Client<UpstreamConnector, Full<Bytes>>,
    ctx: &Arc<ProxyCtx>,
    host: &str,
    port: u16,
    req: &RawRequest,
) -> bool {
    let method = req.method.clone();
    let is_stream = req.path.contains("llm_utils_chat")
        || req
            .hget("accept")
            .map(|v| v.contains("event-stream"))
            .unwrap_or(false);
    ctx.log.log(&format!(
        "  [forward] -> {method} https://{host}:{port}{} ({} bytes body)",
        req.path,
        req.body.len()
    ));

    // 组装上游请求：过滤跳过头 + accept-encoding 降级（br/zstd 无解压支持，对齐 Python）
    let uri = format!("https://{host}:{port}{}", req.path);
    let mut builder = Request::builder()
        .method(method.as_str())
        .uri(&uri)
        .header("host", format!("{host}:{port}"));
    let mut header_pairs: Vec<(String, String)> = Vec::new();
    for (k, v) in &req.headers {
        if HOP_BY_HOP_REQ.iter().any(|h| k.eq_ignore_ascii_case(h)) {
            continue;
        }
        let v = if k.eq_ignore_ascii_case("accept-encoding") {
            "gzip, deflate".to_string()
        } else {
            v.clone()
        };
        header_pairs.push((k.clone(), v));
    }
    for (k, v) in &header_pairs {
        if let (Ok(name), Ok(val)) = (k.parse::<hyper::header::HeaderName>(), v.parse::<hyper::header::HeaderValue>()) {
            builder = builder.header(name, val);
        }
    }
    // Python：GET 请求不带 body
    let body_bytes = if method.eq_ignore_ascii_case("GET") { Bytes::new() } else { req.body.clone() };
    let request = builder
        .body(Full::new(body_bytes))
        .expect("upstream request build");

    let timeout_dur = if is_stream { STREAM_TIMEOUT } else { PLAIN_TIMEOUT };
    let resp = match timeout(timeout_dur, client.request(request)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            ctx.log
                .log(&format!("  [forward] 错误: {} (host={host}, path={})", error_chain(&e), req.path));
            let _ = send_response(io, 502, "Bad Gateway", &[], b"Bad Gateway").await;
            return false;
        }
        Err(_) => {
            ctx.log
                .log(&format!("  [forward] 超时 (timeout={}s): {host}{}", timeout_dur.as_secs(), req.path));
            let _ = send_response(io, 504, "Gateway Timeout", &[], b"Gateway Timeout").await;
            return false;
        }
    };

    let status = resp.status().as_u16();
    let resp_pairs: Vec<(String, String)> = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).to_string()))
        .collect();
    let reason = reason_phrase(status);

    if is_stream {
        return stream_response(io, ctx, host, req, resp).await;
    }

    // 非流式：整体读取，Limited 超限即断 + 逐帧空闲超时（差异修复：对齐 Python
    // socket timeout 30s 的逐读语义，防 trickling body 拖住连接与并发槽）
    match collect_body_with_idle_timeout(Limited::new(resp.into_body(), MAX_RESP_BODY), PLAIN_TIMEOUT).await {
        Ok(resp_body) => {
            ctx.log
                .log(&format!("  [forward] <- {status} {reason} ({}) bytes from {host}{}", resp_body.len(), req.path));
            if send_response(io, status, reason, &resp_pairs, &resp_body).await.is_err() {
                return false;
            }
            // 凭据抓取：refresh_token / 豆包 cookie 命中时涉及 SQLite 全量账号读写 +
            // vault 加密写盘，移出 tokio worker 防风暴期阻塞 accept/上游转发
            {
                let ctx2 = Arc::clone(ctx);
                let path2 = req.path.clone();
                let body2 = resp_body.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    try_capture_refresh_token(&ctx2, &path2, &body2);
                });
            }
            {
                let ctx2 = Arc::clone(ctx);
                let host2 = host.to_string();
                let req_h = req.headers.clone();
                let resp_h = resp_pairs.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    try_capture_doubao_credentials(&ctx2, &host2, &req_h, &resp_h);
                });
            }
            ctx.req_logger.log_request(
                &method,
                host,
                &req.path,
                &req.headers,
                &req.body,
                status,
                reason,
                &resp_pairs,
                &resp_body,
            );
            // 上游要求关闭连接 → 退出 keep-alive 循环
            !resp_pairs
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("connection") && v.eq_ignore_ascii_case("close"))
        }
        Err(e) => {
            ctx.log
                .log(&format!("  [forward] 读上游响应失败: {e} (host={host}, path={})", req.path));
            let _ = send_response(io, 502, "Bad Gateway", &[], b"Bad Gateway").await;
            false
        }
    }
}

/// 流式（SSE）响应转发：逐块 chunked 推给客户端，同步累积日志缓冲（对齐 Python _stream_response）
async fn stream_response<S: AsyncRead + AsyncWrite + Unpin>(
    io: &mut tokio_rustls::server::TlsStream<S>,
    ctx: &ProxyCtx,
    host: &str,
    req: &RawRequest,
    resp: hyper::Response<hyper::body::Incoming>,
) -> bool {
    let status = resp.status().as_u16();
    let reason = reason_phrase(status);
    let resp_pairs: Vec<(String, String)> = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).to_string()))
        .collect();

    let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
    for (k, v) in &resp_pairs {
        if HOP_BY_HOP_RESP.iter().any(|h| k.eq_ignore_ascii_case(h)) {
            continue;
        }
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("Transfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n");
    if io.write_all(head.as_bytes()).await.is_err() {
        return false;
    }
    let _ = io.flush().await;
    ctx.log
        .log(&format!("  [forward] <- {status} {reason} (streaming) from {host}{}", req.path));

    let mut total = 0usize;
    let mut logged: Vec<u8> = Vec::new();
    let mut interrupted = false;
    let mut body = resp.into_body();
    loop {
        // 逐帧空闲超时（对齐 Python 流式分支 300s socket timeout 的逐读语义；
        // 差异修复：原裸 frame() 无超时，上游僵死时隧道永久挂起）
        match timeout(STREAM_TIMEOUT, body.frame()).await {
            Err(_) => {
                ctx.log.log(&format!(
                    "  [forward] 流式空闲超时 ({}s)，中断 (已转发 {total} bytes)",
                    STREAM_TIMEOUT.as_secs()
                ));
                interrupted = true;
                break;
            }
            Ok(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    let chunk: &[u8] = data;
                    let mut pkt = format!("{:x}\r\n", chunk.len()).into_bytes();
                    pkt.extend_from_slice(chunk);
                    pkt.extend_from_slice(b"\r\n");
                    if io.write_all(&pkt).await.is_err() || io.flush().await.is_err() {
                        interrupted = true;
                        break;
                    }
                    total += chunk.len();
                    if logged.len() < MAX_LOG_BODY {
                        logged.extend_from_slice(chunk);
                    }
                }
            }
            Ok(Some(Err(e))) => {
                ctx.log
                    .log(&format!("  [forward] 流式转发中断: {e} (已转发 {total} bytes)"));
                interrupted = true;
                break;
            }
            Ok(None) => break,
        }
    }
    if !interrupted {
        let _ = io.write_all(b"0\r\n\r\n").await;
        let _ = io.flush().await;
    }
    ctx.log.log(&format!(
        "  [forward] 流式转发{}，共 {total} bytes",
        if interrupted { "中断" } else { "完成" }
    ));
    ctx.req_logger.log_request(
        &req.method,
        host,
        &req.path,
        &req.headers,
        &req.body,
        status,
        reason,
        &resp_pairs,
        &logged,
    );
    !interrupted
}

// ---------------- MITM 连接主循环 ----------------

/// 处理一条已解密的 MITM TLS 连接（对齐 Python tunnel_https keep-alive 循环）
pub async fn serve_mitm<S: AsyncRead + AsyncWrite + Unpin>(
    mut io: tokio_rustls::server::TlsStream<S>,
    host: String,
    port: u16,
    ctx: Arc<ProxyCtx>,
    // 进程级共享上游 Client（mod.rs 创建）：跨连接复用 hyper 连接池（TCP/TLS
    // keep-alive），避免每连接/每请求重新建连——桌面客户端高频请求下显著提速
    client: Client<UpstreamConnector, Full<Bytes>>,
) {
    ctx.log
        .log(&format!("  [MITM] 进入 HTTPS 解密隧道: {host}:{port}"));

    loop {
        // 首读给 300s 超时（对齐握手期 _CONN_TIMEOUT；keep-alive 空闲由客户端断开驱动）
        let req = match timeout(CONN_TIMEOUT, read_raw_request(&mut io)).await {
            Err(_) => {
                ctx.log.log(&format!("  [MITM] 读取 TLS 请求超时: {host}:{port}"));
                break;
            }
            Ok(Ok(None)) => {
                ctx.log.log(&format!("  [MITM] 客户端关闭连接: {host}:{port}"));
                break;
            }
            Ok(Err(e)) => {
                // 客户端正常断开 keep-alive 的常见形式（高频，非错误）：直接 FIN
                // 不发 TLS close_notify（rustls 报 unexpected-eof）、RST(10054)、
                // 握手中止(10053)——Chromium/cronet 裁撤连接池为此常态，对齐
                // Python 版与 mitmproxy：静默结束连接，不记错误日志防刷屏
                //（实测 06:17 会话 250 条 MITM 连接中 ~180 条以此方式收尾）
                let client_closed = e.contains("unexpected-eof")
                    || e.contains("close_notify")
                    || e.contains("10053")
                    || e.contains("10054")
                    || e.to_lowercase().contains("connection reset");
                if !client_closed {
                    ctx.log.log(&format!("  [MITM] 读取 TLS 请求错误: {e}"));
                }
                break;
            }
            Ok(Ok(Some(r))) => r,
        };

        let is_target = ctx.host_in_targets(&host);
        let tag = if is_target { " [TRAE]" } else { "" };
        let ep_tag = ctx
            .classify_path(&req.path)
            .map(|n| format!("  <{n}>"))
            .unwrap_or_default();
        ctx.log
            .log(&format!("  {} {host}{}{tag}{ep_tag}", req.method, req.path));

        // 鉴权头嗅探（诊断，每 host 一次）+ JWT 自动捕获（不限 host，对齐 Python）
        if ctx.auto_capture_jwt {
            if let Some(auth) = auth_header_value(&req) {
                let raw = auth.strip_prefix("Cloud-IDE-JWT ").unwrap_or(auth);
                static AUTH_HINT: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
                let is_valid = valid_cloud_ide_jwt(raw).is_some();
                let seen = AUTH_HINT.get_or_init(|| Mutex::new(std::collections::HashSet::new()));
                let inserted = seen
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(format!("auth:{host}"));
                if inserted {
                    let marker = if is_valid { "Cloud-IDE-JWT ✅可捕获" } else { "其他/不可识别" };
                    let prefix: String = auth.chars().take(18).collect();
                    ctx.log.log(&format!(
                        "  [JWT-DEBUG] 在 {host} 发现鉴权头 (类型: {marker}: {prefix}…)"
                    ));
                }
                if let (true, Some(valid)) = (is_valid, valid_cloud_ide_jwt(raw)) {
                    if let Some(uid) = extract_user_id(&valid) {
                        // 重 DB 写盘（SQLite 全量账号读写）移出 async worker
                        let ctx2 = Arc::clone(&ctx);
                        let _ = tokio::task::spawn_blocking(move || {
                            update_account_jwt(&ctx2, &uid, &valid);
                        });
                    }
                }
            }
        }

        // 设备头改写（仅签到接口）：改写后走专用转发并继续 keep-alive 循环
        if req.path.contains(SIGNIN_PATH) {
            let uid = auth_header_value(&req).and_then(extract_user_id);
            match uid {
                Some(uid) => {
                    let dev = crate::commands::accounts::derive_device(&uid);
                    let mut rewritten = req;
                    rewritten.hset("x-device-id", dev.device_id.clone());
                    // 派生值缺失时不注入空头（审查修复：空值头非法且可能被上游 400）
                    if let Some(mid) = dev.market_user_id.clone() {
                        rewritten.hset("x-market-user-id", mid);
                    }
                    if let Some(sid) = dev.session_id.clone() {
                        rewritten.hset("vscode-sessionid", sid);
                    }
                    ctx.log.log(&format!(
                        "  [签到改写] uid={uid} -> x-device-id={} x-market-user-id={} vscode-sessionid={}",
                        dev.device_id,
                        dev.market_user_id.as_deref().unwrap_or("无"),
                        dev.session_id.as_deref().unwrap_or("无"),
                    ));
                    let keep = forward_upstream(&mut io, &client, &ctx, &host, port, &rewritten).await;
                    if !keep {
                        break;
                    }
                    continue;
                }
                None => ctx.log.log("  [签到] 未解析到 user id，未改写"),
            }
        }

        // WebSocket 升级 → 专用通道接管连接
        if req.is_websocket_upgrade() {
            ctx.log.log(&format!(
                "  [WebSocket] 检测到升级请求: {} {host}:{port}{} (Connection={})",
                req.method,
                req.path,
                req.hget("connection").unwrap_or("?")
            ));
            crate::device_proxy::ws::forward_websocket(&mut io, &ctx, &host, port, &req).await;
            break; // WS 连接已接管，退出 keep-alive 循环
        }

        let keep = forward_upstream(&mut io, &client, &ctx, &host, port, &req).await;
        if !keep {
            break;
        }
    }
    let _ = io.shutdown().await;
}

/// 取鉴权头值（authorization / x-cloudide-token / x-icube-token，对齐 Python 顺序）
fn auth_header_value(req: &RawRequest) -> Option<&str> {
    for name in ["authorization", "x-cloudide-token", "x-icube-token"] {
        if let Some(v) = req.hget(name) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jwt_validation_and_uid_extraction() {
        // 构造 header(alg=RS256) + payload(data.id=123) 的 JWT
        use base64::Engine;
        let b64 = |v: &serde_json::Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap())
        };
        let header = b64(&serde_json::json!({"alg": "RS256", "typ": "JWT"}));
        let payload = b64(&serde_json::json!({"data": {"id": 4487568582777872i64}, "exp": 1893456000}));
        let tok = format!("{header}.{payload}.c2ln");
        let valid = valid_cloud_ide_jwt(&tok);
        assert!(valid.is_some());
        assert_eq!(extract_user_id(&valid.unwrap()).unwrap(), "4487568582777872");
        // 非 RS256 → 拒绝
        let header2 = b64(&serde_json::json!({"alg": "HS256"}));
        assert!(valid_cloud_ide_jwt(&format!("{header2}.{payload}.sig")).is_none());
        // 前缀剥离 + Bearer
        assert_eq!(extract_user_id("Cloud-IDE-JWT abc.def.ghi"), None); // 非法 token
    }

    #[test]
    fn parse_cookie_and_multi_sids() {
        let jar = parse_cookie_header("a=1; sessionid=\"abc\"; b=2");
        assert_eq!(jar.iter().find(|(k, _)| k == "sessionid").map(|(_, v)| v.as_str()), Some("abc"));
        let uid = doubao_uid_from_multi_sids(
            "multi_sids=111%3AsidA%7C222%3AsidB; sessionid=sidB",
            "sidB",
        );
        assert_eq!(uid, "222");
        assert_eq!(doubao_uid_from_multi_sids("multi_sids=111:sidA", "nope"), "");
    }

    #[test]
    fn raw_request_header_ops() {
        let mut req = RawRequest {
            method: "GET".into(),
            path: "/".into(),
            headers: vec![
                ("Host".into(), "a.com".into()),
                ("X-Token".into(), "old".into()),
            ],
            body: Bytes::new(),
        };
        assert_eq!(req.hget("x-token"), Some("old"));
        req.hset("X-TOKEN", "new".into());
        assert_eq!(req.hget("x-token"), Some("new"));
        assert_eq!(req.headers.len(), 2); // 旧值被移除
        assert!(!req.is_websocket_upgrade());
        req.hset("Upgrade", "WebSocket".into());
        assert!(req.is_websocket_upgrade());
    }

    #[test]
    fn find_head_end_positions() {
        assert_eq!(find_head_end(b"GET / HTTP/1.1\r\n\r\n"), Some(14));
        assert_eq!(find_head_end(b"partial"), None);
        assert_eq!(find_head_end(b"ab\r\n\r\n"), Some(2));
    }

    #[test]
    fn reason_phrases() {
        assert_eq!(reason_phrase(200), "OK");
        assert_eq!(reason_phrase(502), "Bad Gateway");
        assert_eq!(reason_phrase(599), "");
    }

    /// 构造测试用 ProxyCtx（临时目录；不 emit 前端事件）
    fn test_ctx(tag: &str) -> ProxyCtx {
        let dir = std::env::temp_dir().join(format!("aiwork_handler_test_{tag}_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let captured = Arc::new(std::sync::atomic::AtomicI64::new(0));
        ProxyCtx {
            log: crate::device_proxy::logger::ProxyLog::new(dir.join("proxy.log"), None, captured),
            req_logger: Arc::new(crate::device_proxy::logger::RequestLogger::new(dir.clone())),
            targets: vec![],
            auto_capture_jwt: true,
            data_dir: dir.clone(),
            upstream: None,
            direct_domains: Arc::new(Vec::new()),
            pin_state: Mutex::new(HashMap::new()),
        }
    }

    /// 构造带 exp 的 Cloud-IDE-JWT（新 exp 晚于旧 exp 用于防降级测试）
    fn make_jwt(exp: i64) -> String {
        use base64::Engine;
        let b64 = |v: &serde_json::Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap())
        };
        let header = b64(&serde_json::json!({"alg": "RS256", "typ": "JWT"}));
        let payload = b64(&serde_json::json!({"data": {"id": "u-test"}, "exp": exp}));
        format!("Cloud-IDE-JWT {header}.{payload}.sig")
    }

    /// 审查修复回归（两段锁合并）：JWT + refresh_token 必须一次写盘同时生效，
    /// 不允许出现「JWT 已更新 / refresh_token 未更新」的中间态落盘
    #[test]
    fn jwt_and_refresh_update_atomically() {
        let ctx = test_ctx("atomic");
        let old_jwt = make_jwt(1893456000);
        let new_jwt = make_jwt(1893456000 + 3600);
        crate::store::docs::accounts_save_raw(
            &crate::store::db(&ctx.data_dir),
            &serde_json::json!({"accounts": [{"name": "n", "UserID": "u-test", "jwt": old_jwt}]}),
        )
        .unwrap();
        let status = update_account_jwt_and_refresh(&ctx, "u-test", &new_jwt, Some("rt-new"));
        assert_eq!(status, "updated");
        let cfg: serde_json::Value =
            crate::store::docs::accounts_load_raw(&crate::store::db(&ctx.data_dir));
        let acc = &cfg["accounts"][0];
        assert_eq!(acc["jwt"].as_str(), Some(new_jwt.as_str()), "JWT 应已更新");
        assert_eq!(acc["refresh_token"].as_str(), Some("rt-new"), "refresh_token 应同次写盘更新");
        assert!(acc.get("refresh_token_updated_at").and_then(|v| v.as_str()).is_some());
    }

    /// 审查修复回归（uid 切片）：非 ASCII uid 追加账号时按字符截断不 panic
    #[test]
    fn append_account_with_multibyte_uid() {
        let ctx = test_ctx("multibyte");
        let jwt = make_jwt(1893456000);
        let uid = "日本語ユーザー001";
        let status = update_account_jwt(&ctx, uid, &jwt);
        assert_eq!(status, "appended");
        let cfg: serde_json::Value =
            crate::store::docs::accounts_load_raw(&crate::store::db(&ctx.data_dir));
        let acc = &cfg["accounts"][0];
        assert_eq!(acc["UserID"].as_str(), Some(uid));
        assert_eq!(acc["name"].as_str(), Some("auto_日本語ユーザー0")); // chars().take(8)
    }
}
