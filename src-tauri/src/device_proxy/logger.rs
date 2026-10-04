//! 代理日志（原 device_proxy.py log/ProxyRequestLogger/_mask_*/extract_sse_summary/_ws_hexdump 的 Rust 版）。
//!
//! 两条日志流：
//! - [`ProxyLog`]：操作日志 → logs/proxy.log + 前端 `proxy-log` 事件（含 `account-captured` 派生），
//!   对应 Python 版 stdout 逐行被桌面端消费的通路；
//! - [`RequestLogger`]：抓包日志 → logs/proxy_req_YYYY-MM-DD.log（同日 100MB 滚动 .N 序号），
//!   凭证头/体级凭证键值一律脱敏后落盘（脱敏红线，见 Python 版审查修复 P0）。

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

/// 凭证头脱敏清单（键比较不区分大小写）：命中即掩码，禁止明文落盘。
/// x-cloudide-token / x-icube-token 与 handler::auth_header_value 嗅探的 JWT 承载头
/// 保持同源（审查修复：此前两头来承载 token 却未命中清单，明文落盘违反脱敏红线）
const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "proxy-authorization",
    "x-cloudide-token",
    "x-icube-token",
    "x-auth-token",
    "x-session-token",
];

/// 体级凭证键值脱敏：值整体替换为 "***"（fail-closed：编译期正则，无运行期失效路径）。
/// 键集合覆盖 handler 采信的凭证键（含裸 "token"——handler::try_capture_doubao_creds
/// 采信 inner.get("token") 为 access_token；client_secret 补漏——\bsecret\b 因下划线
/// 属 word 字符永远匹配不到它）
fn body_mask_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r#"(?i)((?:"(?:access_token|refresh_token|id_token|session_token|sessionid|sid_guard|ttwid|jwt|api_key|apikey|secret|client_secret|token|authorization|password|pass_token)"|\b(?:access_token|refresh_token|id_token|session_token|sessionid|sid_guard|ttwid|jwt|api_key|apikey|client_secret|token|authorization|password|pass_token)\b)\s*[=:]\s*)("[^"]*"|'[^']*'|[^,;&\s}]+)"#,
        )
        .expect("body mask regex")
    })
}

/// 凭证头掩码：len>16 取前8+…+后4，否则 "***"
pub(crate) fn mask_header_value(name: &str, value: &str) -> String {
    if SENSITIVE_HEADERS.iter().any(|h| h.eq_ignore_ascii_case(name)) {
        let chars: Vec<char> = value.chars().collect();
        return if chars.len() > 16 {
            let head: String = chars[..8].iter().collect();
            let tail: String = chars[chars.len() - 4..].iter().collect();
            format!("{head}…{tail}")
        } else {
            "***".to_string()
        };
    }
    value.to_string()
}

/// 体级凭证键值脱敏（对齐 Python _mask_body）
pub(crate) fn mask_body(text: &str) -> String {
    body_mask_regex().replace_all(text, r#"$1"***""#).to_string()
}

/// 按 Content-Encoding 解压响应体用于日志展示（gzip/deflate；br/zstd 与 Python 版同样跳过）
pub(crate) fn decompress_body(body: &[u8], resp_headers: &[(String, String)]) -> Vec<u8> {
    let encoding = resp_headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-encoding"))
        .map(|(_, v)| v.trim().to_ascii_lowercase());
    let Some(encoding) = encoding else {
        return body.to_vec();
    };
    let mut out = Vec::new();
    match encoding.as_str() {
        "gzip" => {
            if flate2::read::MultiGzDecoder::new(body).read_to_end(&mut out).is_err() {
                return body.to_vec();
            }
        }
        "deflate" => {
            // 先按 zlib-wrapped 试，失败再按 raw deflate（对齐 Python 双尝试）
            if flate2::read::ZlibDecoder::new(body).read_to_end(&mut out).is_err() {
                out.clear();
                if flate2::read::DeflateDecoder::new(body).read_to_end(&mut out).is_err() {
                    return body.to_vec();
                }
            }
        }
        _ => return body.to_vec(),
    }
    out
}

/// SSE 流式响应摘要（模型/token 用量等）。仅对 Content-Type: text/event-stream 生效。
pub(crate) fn extract_sse_summary(resp_headers: &[(String, String)], body: &[u8]) -> Option<Vec<(String, String)>> {
    let is_sse = resp_headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("content-type") && v.to_ascii_lowercase().contains("event-stream")
    });
    if !is_sse || body.is_empty() {
        return None;
    }
    let text = String::from_utf8_lossy(body);
    let mut summary: Vec<(String, String)> = Vec::new();
    let mut push = |k: &str, v: String| {
        if let Some(slot) = summary.iter_mut().find(|(ek, _)| ek == k) {
            slot.1 = v;
        } else {
            summary.push((k.to_string(), v));
        }
    };
    let mut current_event = String::new();
    let mut output_count = 0usize;
    for line in text.split('\n') {
        let line = line.trim();
        if let Some(ev) = line.strip_prefix("event:") {
            current_event = ev.trim().to_string();
        } else if let Some(data_str) = line.strip_prefix("data:") {
            let data_str = data_str.trim();
            if data_str.is_empty() {
                continue;
            }
            let Ok(data) = serde_json::from_str::<serde_json::Value>(data_str) else {
                continue;
            };
            match current_event.as_str() {
                "metadata" => {
                    if let Some(m) = data.get("model").or_else(|| data.get("model_name")).and_then(|v| v.as_str()) {
                        push("model", m.to_string());
                    }
                    if let Some(sid) = data.get("session_id").and_then(|v| v.as_str()) {
                        let truncated: String = sid.chars().take(16).collect();
                        push("session_id", format!("{truncated}..."));
                    }
                }
                "output" => output_count += 1,
                "token_usage" => {
                    if let Some(pt) = data.get("prompt_tokens").and_then(serde_json::Value::as_i64) {
                        push("prompt_tokens", pt.to_string());
                    }
                    if let Some(ct) = data.get("completion_tokens").and_then(serde_json::Value::as_i64) {
                        push("completion_tokens", ct.to_string());
                    }
                    if let Some(tt) = data.get("total_tokens").and_then(serde_json::Value::as_i64) {
                        push("total_tokens", tt.to_string());
                    }
                }
                "done" => {
                    if let Some(fr) = data.get("finish_reason").and_then(|v| v.as_str()) {
                        push("finish_reason", fr.to_string());
                    }
                }
                _ => {}
            }
        }
    }
    if output_count > 0 {
        push("output_chunks", output_count.to_string());
    }
    (!summary.is_empty()).then_some(summary)
}

// ---------------- 操作日志（proxy.log + 前端事件桥接） ----------------

/// 操作日志：落盘 logs/proxy.log 并向前端 emit `proxy-log`；
/// 行内含 user=<纯数字> 时派生 `account-captured` 事件并累加捕获计数
///（对齐 Python 版 stdout 行被桌面端 extract_uid 消费的语义）。
/// 超过 10MB 滚动为 proxy.log.1（审查修复：原 append-only 无上限，长跑高频 WS 帧日志可无限增长）
#[derive(Clone)]
pub struct ProxyLog {
    /// (带时间戳行, 原始行) 投递给专用落盘线程。写文件+滚动+前端 emit 全部移出
    /// tokio worker——此前同步写盘（Mutex + flush + Tauri IPC emit）跑在异步上下文，
    /// 桌面客户端启动风暴（百级连接、每请求 3~4 行日志）会阻塞 worker，导致 TLS
    /// accept / 上游转发排队超时（实测 forward 30s 超时 + 客户端握手中止恶性循环；
    /// Python 版 thread-per-connection 无此问题）
    tx: std::sync::mpsc::Sender<(String, String)>,
}

/// proxy.log 单文件滚动上限
const PROXY_LOG_MAX: u64 = 10 * 1024 * 1024;

fn open_append(path: &std::path::Path) -> Option<std::fs::File> {
    OpenOptions::new().create(true).append(true).open(path).ok()
}

impl ProxyLog {
    pub fn new(log_path: PathBuf, app: Option<tauri::AppHandle>, captured: Arc<AtomicI64>) -> Self {
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let (tx, rx) = std::sync::mpsc::channel::<(String, String)>();
        std::thread::Builder::new()
            .name("proxy-log-drainer".into())
            .spawn(move || {
                use tauri::Emitter;
                let mut path = log_path;
                let mut file = open_append(&path).or_else(|| {
                    path = std::env::temp_dir().join("aiwork_proxy.log");
                    open_append(&path)
                });
                while let Ok((stamped, raw)) = rx.recv() {
                    if file.is_none() {
                        file = open_append(&path);
                    }
                    if let Some(f) = file.as_mut() {
                        // 滚动：超限先关句柄（Windows rename 需独占）→ 换名 .1 → 重开
                        if f.metadata().map(|m| m.len()).unwrap_or(0) >= PROXY_LOG_MAX {
                            let _ = file.take(); // 关闭旧句柄（drop 副作用，非赋值）
                            let rotated = path.with_extension("log.1");
                            let _ = std::fs::remove_file(&rotated);
                            let _ = std::fs::rename(&path, &rotated);
                            file = open_append(&path);
                        }
                    }
                    if let Some(f) = file.as_mut() {
                        let _ = writeln!(f, "{stamped}");
                        let _ = f.flush();
                    }
                    if let Some(app) = &app {
                        let _ = app.emit("proxy-log", &raw);
                        if let Some(uid) = extract_uid(&raw) {
                            captured.fetch_add(1, Ordering::Relaxed);
                            // 结果型事件（前端弹 toast + 刷新账号池）：emit 失败落日志（issue #44 遗留项）
                            crate::events::emit_logged(app, "account-captured", serde_json::Value::String(uid.clone()), None);
                        }
                    }
                }
            })
            .expect("spawn proxy log drainer");
        Self { tx }
    }

    /// 记录一行操作日志（自动加时间戳前缀；非阻塞投递给落盘线程，顺序有保证）
    pub fn log(&self, line: &str) {
        let stamped = format!("[{}] {}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), line);
        let _ = self.tx.send((stamped, line.to_string()));
    }
}

/// 从日志行提取 user=<uid>（对齐 commands/proxy.rs 原实现，供 account-captured 事件派生）
fn extract_uid(line: &str) -> Option<String> {
    for key in ["user=", "user_id="] {
        if let Some(idx) = line.find(key) {
            let rest = &line[idx + key.len()..];
            let end = rest
                .find(|c: char| !(c.is_ascii_digit() || c == '_'))
                .unwrap_or(rest.len());
            let uid = &rest[..end];
            if !uid.is_empty() && uid.chars().all(|c| c.is_ascii_digit()) {
                return Some(uid.to_string());
            }
        }
    }
    None
}

// ---------------- 抓包日志（滚动文件） ----------------

/// 抓包日志：按日命名 proxy_req_%Y-%m-%d.log，同日单文件超 100MB 换 .N 序号继续追加。
struct RollingState {
    file: Option<std::fs::File>,
    day: String,
    seq: u32,
    size: u64,
}

pub struct RequestLogger {
    /// 格式化后的整块日志投递给专用落盘线程（滚动+写盘+flush 移出 tokio worker，
    /// 理由同 ProxyLog 异步化）
    tx: std::sync::mpsc::Sender<String>,
}

impl RequestLogger {
    pub fn new(dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        std::thread::Builder::new()
            .name("proxy-reqlog-drainer".into())
            .spawn(move || {
                let mut state = RollingState {
                    file: None,
                    day: String::new(),
                    seq: 0,
                    size: 0,
                };
                let max_size = 100 * 1024 * 1024;
                while let Ok(data) = rx.recv() {
                    if let Ok(f) = Self::ensure_file(&mut state, &dir, max_size) {
                        let _ = f.write_all(data.as_bytes());
                        let _ = f.flush();
                        state.size += data.len() as u64;
                    }
                }
            })
            .expect("spawn proxy req-log drainer");
        Self { tx }
    }

    /// 确保当日文件可写（跨日/超限换文件），返回已定位的写入口
    fn ensure_file<'a>(
        state: &'a mut RollingState,
        dir: &std::path::Path,
        max_size: u64,
    ) -> std::io::Result<&'a mut std::fs::File> {
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        if state.file.is_some() && state.day == today && state.size < max_size {
            return Ok(state.file.as_mut().expect("checked"));
        }
        state.file = None;
        if state.day != today {
            state.day = today;
            state.seq = 0;
        }
        let base = dir.join(format!("proxy_req_{}.log", state.day));
        loop {
            let path = if state.seq == 0 { base.clone() } else { base.with_extension(format!("log.{}", state.seq)) };
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if size < max_size {
                let f = OpenOptions::new().create(true).append(true).open(&path)?;
                state.file = Some(f);
                state.size = size;
                break;
            }
            state.seq += 1;
        }
        Ok(state.file.as_mut().expect("just set"))
    }

    fn write_block(&self, data: &str) {
        // 非阻塞投递（丢失容忍：日志非关键数据；线程退出即停）
        let _ = self.tx.send(data.to_string());
    }

    /// 记录一次完整请求/响应（脱敏红线：凭证头与体级凭证键值掩码后落盘）
    #[allow(clippy::too_many_arguments)]
    pub fn log_request(
        &self,
        method: &str,
        host: &str,
        path: &str,
        req_headers: &[(String, String)],
        req_body: &[u8],
        resp_status: u16,
        resp_reason: &str,
        resp_headers: &[(String, String)],
        resp_body: &[u8],
    ) {
        let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let mut out = String::new();
        out.push_str(&format!("\n{}\n", "=".repeat(80)));
        out.push_str(&format!("[{ts}] {method} {host}{path}\n"));
        out.push_str("--- Request Headers ---\n");
        for (k, v) in req_headers {
            out.push_str(&format!("  {k}: {}\n", mask_header_value(k, v)));
        }
        if !req_body.is_empty() {
            let preview = String::from_utf8_lossy(&req_body[..req_body.len().min(4096)]);
            out.push_str(&format!("--- Request Body ({} bytes) ---\n", req_body.len()));
            out.push_str(&mask_body(&preview));
            out.push('\n');
        }
        out.push_str(&format!("--- Response: {resp_status} {resp_reason} ---\n"));
        for (k, v) in resp_headers {
            out.push_str(&format!("  {k}: {}\n", mask_header_value(k, v)));
        }
        if !resp_body.is_empty() {
            let decompressed = decompress_body(resp_body, resp_headers);
            // 预览上限 256KB：model/list / region endpoints 等配置类 JSON 解压后 ~64-128KB，
            // 旧 8KB 截断导致完整结构（模型目录/加密 payload）无法离线分析
            let preview_max: usize = 256 * 1024;
            let preview = String::from_utf8_lossy(&decompressed[..decompressed.len().min(preview_max)]);
            out.push_str(&format!(
                "--- Response Body ({} bytes, decompressed {} bytes) ---\n",
                resp_body.len(),
                decompressed.len()
            ));
            out.push_str(&mask_body(&preview));
            out.push('\n');
        }
        if let Some(summary) = extract_sse_summary(resp_headers, resp_body) {
            out.push_str("--- SSE Summary ---\n");
            for (k, v) in summary {
                out.push_str(&format!("  {k}: {v}\n"));
            }
        }
        out.push('\n');
        self.write_block(&out);
    }

    /// 记录 WebSocket 升级握手（升级请求同样携带 Authorization 等凭证头，掩码后落盘）
    pub fn log_websocket(&self, host: &str, path: &str, req_headers: &[(String, String)]) {
        let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let mut out = String::new();
        out.push_str(&format!("\n{}\n", "=".repeat(80)));
        out.push_str(&format!("[{ts}] [WebSocket Upgrade] {host}{path}\n"));
        out.push_str("--- Request Headers ---\n");
        for (k, v) in req_headers {
            out.push_str(&format!("  {k}: {}\n", mask_header_value(k, v)));
        }
        out.push_str("--- WebSocket tunnel established: 双向帧载荷将在隧道中按 SEQ 记录 ---\n");
        out.push('\n');
        self.write_block(&out);
    }
}

// ---------------- WS 帧可视化辅助 ----------------

/// 二进制数据 → 可读 hex+ascii 文本（截断 400 字节，对齐 Python _ws_hexdump）
pub(crate) fn ws_hexdump(data: &[u8], max_bytes: usize) -> String {
    let mut lines = Vec::new();
    let n = data.len().min(max_bytes);
    for (offset, chunk) in data[..n].chunks(16).enumerate() {
        let hexs: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
        let hex_padded: String = hexs.join(" ");
        let asc: String = chunk
            .iter()
            .map(|&b| if (0x20..=0x7e).contains(&b) { b as char } else { '.' })
            .collect();
        lines.push(format!("    {:04x}: {:<48}  {asc}", offset * 16, hex_padded));
    }
    if data.len() > n {
        lines.push(format!("    ... (截断，完整 {} bytes)", data.len()));
    }
    lines.join("\n")
}

/// 抽取连续可打印 ASCII 串（>=min_len），用于快速定位字面量字段（对齐 _ws_extract_strings）
pub(crate) fn ws_extract_strings(data: &[u8], min_len: usize, max_strings: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    for &b in data {
        if (0x20..=0x7e).contains(&b) {
            buf.push(b);
        } else {
            if buf.len() >= min_len {
                out.push(String::from_utf8_lossy(&buf).to_string());
            }
            buf.clear();
        }
    }
    if buf.len() >= min_len {
        out.push(String::from_utf8_lossy(&buf).to_string());
    }
    out.truncate(max_strings);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_credentials_in_bodies() {
        let masked = mask_body(r#"{"access_token":"abc.def.ghi","refresh_token": "xyz123"}"#);
        assert!(masked.contains(r#""access_token":"***""#));
        assert!(masked.contains(r#""refresh_token": "***""#));
        assert!(!masked.contains("abc.def"));
        // 审查修复回归：裸 token 键（handler 采信为 access_token）与 client_secret 必须掩码
        let masked2 = mask_body(r#"{"token":"jwt-plain","client_secret":"cs-secret","note":"keep"}"#);
        assert!(masked2.contains(r#""token":"***""#), "裸 token 键: {masked2}");
        assert!(masked2.contains(r#""client_secret":"***""#), "client_secret: {masked2}");
        assert!(masked2.contains("keep"));
        assert!(!masked2.contains("jwt-plain"));
        assert!(!masked2.contains("cs-secret"));
        let masked3 = mask_body("token=abc.def&x=1");
        assert!(masked3.contains(r#"token="***""#), "urlencoded: {masked3}");
    }

    #[test]
    fn masks_sensitive_headers() {
        let long = mask_header_value("Authorization", "Cloud-IDE-JWT eyJhbGciOiJSUzI1NiJ9.sig");
        assert!(long.starts_with("Cloud-ID"));
        assert!(long.ends_with(".sig"));
        assert!(long.contains('…'));
        assert_eq!(mask_header_value("Cookie", "a=b"), "***");
        assert_eq!(mask_header_value("Content-Type", "application/json"), "application/json");
        // 审查修复回归：JWT 承载头（与 handler::auth_header_value 嗅探集合同源）必须掩码
        let ide = mask_header_value("x-cloudide-token", "eyJhbGciOiJIUzI1NiJ9.payload.sig");
        assert!(!ide.contains("payload"), "x-cloudide-token 掩码: {ide}");
        // 长值（>16）走「前8…后4」掩码（与 Authorization 用例同款设计）：
        // 断言中段载荷被隐藏，而非整值消失（尾 4 字符本就保留）
        let icube = mask_header_value("X-Icube-Token", "eyJhbGciOiJIUzI1NiJ9.p.s");
        assert!(!icube.contains("OiJIUzI1NiJ9"), "x-icube-token 掩码: {icube}");
        assert_ne!(icube, "eyJhbGciOiJIUzI1NiJ9.p.s", "x-icube-token 必须掩码");
    }

    #[test]
    fn gzip_roundtrip_decompress() {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(b"hello world").unwrap();
        let gz = enc.finish().unwrap();
        let headers = vec![("Content-Encoding".to_string(), "gzip".to_string())];
        assert_eq!(decompress_body(&gz, &headers), b"hello world");
    }

    #[test]
    fn sse_summary_extraction() {
        let headers = vec![("Content-Type".to_string(), "text/event-stream".to_string())];
        let body = concat!(
            "event: metadata\n",
            "data: {\"model\":\"glm-5.3\",\"session_id\":\"abcdefghijklmnopqrst\"}\n",
            "event: output\n",
            "data: {}\n",
            "event: output\n",
            "data: {}\n",
            "event: token_usage\n",
            "data: {\"prompt_tokens\":10,\"completion_tokens\":20,\"total_tokens\":30}\n",
            "event: done\n",
            "data: {\"finish_reason\":\"stop\"}\n",
        );
        let s = extract_sse_summary(&headers, body.as_bytes()).expect("summary");
        let get = |k: &str| s.iter().find(|(ek, _)| ek == k).map(|(_, v)| v.clone());
        assert_eq!(get("model").as_deref(), Some("glm-5.3"));
        assert_eq!(get("session_id").as_deref(), Some("abcdefghijklmnop..."));
        assert_eq!(get("output_chunks").as_deref(), Some("2"));
        assert_eq!(get("total_tokens").as_deref(), Some("30"));
        assert_eq!(get("finish_reason").as_deref(), Some("stop"));
    }

    #[test]
    fn uid_extraction_from_log_lines() {
        // 真捕获行（handler: [JWT 自动更新/追加] user=…）触发派生事件
        assert_eq!(extract_uid("  [JWT 自动更新] user=4487568582777872 exp=..."), Some("4487568582777872".into()));
        // 签到改写行用 uid= 记法（审查修复：user= 会每次误触发 account-captured）
        assert_eq!(extract_uid("  [签到改写] uid=4487568582777872 -> x-device-id=1"), None);
        assert_eq!(extract_uid("no user here"), None);
        assert_eq!(extract_uid("user=abc"), None);
    }

    #[test]
    fn hexdump_format() {
        let s = ws_hexdump(b"AB", 400);
        assert!(s.contains("41 42"));
        assert!(s.contains("AB"));
    }
}
