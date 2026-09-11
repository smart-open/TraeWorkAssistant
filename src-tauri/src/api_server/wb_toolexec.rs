//! 网关工具代执行（T5.5/F-64）
//!
//! 「上游不支持的工具调用在代理侧补齐」范式（muskke/trae-api-proxy 设计吸收，
//! learn-the-design 不抄码）。客户端（Codex /v1/responses）声明 `type:"web_search"`
//! 等上游不支持的工具时：
//! 1. 代理向 chat body 注入同义 function 工具（web_search / open_url）+ system 提示；
//! 2. 上游模型发起 function 调用 → 代理本地执行（DuckDuckGo HTML 搜索 / 页面抓取
//!    剥离，零新增依赖，ureq 已有）；
//! 3. 结果以 tool 消息回喂上游继续，直至最终回复（上限 MAX_ROUNDS 轮防积分失控）；
//! 4. 响应投影：历史调用轮以原生 `web_search_call` 输出项返回（Responses 协议），
//!    最终回复按既有 completion_to_responses 投影。
//!
//! 红线：仅代理注入的这两个工具会被代执行；客户端声明的真实 function 工具
//! 照常透传上游，由客户端自行执行。搜索请求单次单发、不重试。

use serde_json::{json, Value};

pub const TOOL_SEARCH: &str = "web_search";
pub const TOOL_OPEN_URL: &str = "open_url";
/// 代执行轮数上限（每轮 = 一次上游请求，积分成本有界）
pub const MAX_ROUNDS: usize = 3;
/// open_url 页面正文截断（字符）
const PAGE_TEXT_LIMIT: usize = 4000;
/// 搜索结果条数上限
const SEARCH_RESULTS_LIMIT: usize = 5;

/// Responses 请求是否声明了上游不支持的 web_search 类工具
pub fn responses_declares_web_search(body: &Value) -> bool {
    body.get("tools")
        .and_then(|t| t.as_array())
        .map_or(false, |arr| {
            arr.iter()
                .any(|t| matches!(t.get("type").and_then(|v| v.as_str()), Some("web_search") | Some("web_search_preview")))
        })
}

/// 向 chat body（responses_to_chat 产物）注入代理 function 工具 + system 提示
pub fn inject_proxy_tools(chat_body: &mut Value) {
    let Some(obj) = chat_body.as_object_mut() else { return };
    let tools = obj.entry("tools".to_string()).or_insert_with(|| json!([]));
    if let Some(arr) = tools.as_array_mut() {
        let has = |arr: &[Value], name: &str| -> bool {
            arr.iter()
                .any(|t| t.pointer("/function/name").and_then(|n| n.as_str()) == Some(name))
        };
        if !has(arr, TOOL_SEARCH) {
            arr.push(json!({
                "type": "function",
                "function": {
                    "name": TOOL_SEARCH,
                    "description": "Search the web for up-to-date information. Returns a numbered list of results (title, url, snippet).",
                    "parameters": {
                        "type": "object",
                        "properties": { "query": { "type": "string", "description": "Search query" } },
                        "required": ["query"],
                    },
                },
            }));
        }
        if !has(arr, TOOL_OPEN_URL) {
            arr.push(json!({
                "type": "function",
                "function": {
                    "name": TOOL_OPEN_URL,
                    "description": "Fetch a web page by URL and return its readable text content.",
                    "parameters": {
                        "type": "object",
                        "properties": { "url": { "type": "string", "description": "Absolute http(s) URL" } },
                        "required": ["url"],
                    },
                },
            }));
        }
    }
    // system 提示：插在首个非 system 消息之前（保留既有 instructions 语义）
    if let Some(msgs) = obj.get_mut("messages").and_then(|m| m.as_array_mut()) {
        let hint = json!({
            "role": "system",
            "content": "Web research tools (web_search, open_url) are executed by the serving proxy. Call web_search with a concise query when current information is needed, and open_url to read a specific page. Wait for tool results before answering.",
        });
        let insert_at = msgs
            .iter()
            .position(|m| m.get("role").and_then(|r| r.as_str()) != Some("system"))
            .unwrap_or(msgs.len());
        msgs.insert(insert_at, hint);
    }
}

/// 代执行记录（供 Responses web_search_call 输出项投影）
#[derive(Debug, Clone)]
pub struct SearchRecord {
    pub tool: String,
    pub query: String,
    /// 执行是否成功（保留字段：调试与后续 usage 归因扩展用）
    #[allow(dead_code)]
    pub ok: bool,
}

/// 从聚合 completion 提取代理工具调用：(call_id, name, arguments)
pub fn extract_proxy_calls(completion: &Value) -> Vec<(String, String, String)> {
    completion
        .pointer("/choices/0/message/tool_calls")
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|tc| {
                    let name = tc.pointer("/function/name").and_then(|n| n.as_str())?;
                    if name != TOOL_SEARCH && name != TOOL_OPEN_URL {
                        return None;
                    }
                    let id = tc.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
                    let args = tc
                        .pointer("/function/arguments")
                        .and_then(|a| a.as_str())
                        .unwrap_or("{}")
                        .to_string();
                    Some((id, name.to_string(), args))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 执行代理工具调用；失败以文本错误返回（回喂模型自行决策，不中断轮次）
pub fn execute(name: &str, arguments: &str) -> (String, bool) {
    let args: Value = serde_json::from_str(arguments).unwrap_or(json!({}));
    match name {
        TOOL_SEARCH => {
            let q = args.get("query").and_then(|v| v.as_str()).unwrap_or("").trim();
            if q.is_empty() {
                return ("error: empty query".into(), false);
            }
            let r = web_search(q);
            let ok = r.is_ok();
            (r.unwrap_or_else(|e| format!("error: {e}")), ok)
        }
        TOOL_OPEN_URL => {
            let u = args.get("url").and_then(|v| v.as_str()).unwrap_or("").trim();
            if u.is_empty() {
                return ("error: empty url".into(), false);
            }
            let r = open_url(u);
            let ok = r.is_ok();
            (r.unwrap_or_else(|e| format!("error: {e}")), ok)
        }
        _ => (format!("error: unknown proxy tool {name}"), false),
    }
}

/// 极简 percent-encode（查询串安全子集之外全编码）
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// DuckDuckGo HTML 搜索（lite 端点；单发不重试，失败明示）
fn web_search(query: &str) -> Result<String, String> {
    let url = format!("https://html.duckduckgo.com/html/?q={}", url_encode(query));
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(8))
        .timeout(std::time::Duration::from_secs(15))
        .build();
    let html = agent
        .get(&url)
        .set("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
        .set("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8")
        .call()
        .map_err(|e| format!("搜索请求失败: {e}"))?
        .into_string()
        .map_err(|e| format!("搜索响应读取失败: {e}"))?;
    let results = parse_ddg_results(&html);
    if results.is_empty() {
        return Err("搜索无结果（上游可能限流，可稍后重试）".into());
    }
    let mut out = String::new();
    for (i, (title, url, snippet)) in results.iter().take(SEARCH_RESULTS_LIMIT).enumerate() {
        out.push_str(&format!("{}. {}\n   {}\n   {}\n\n", i + 1, title, url, snippet));
    }
    Ok(out)
}

/// 从 DDG HTML 提取 (title, url, snippet)：仅做字符串定位，不引 HTML 解析器
fn parse_ddg_results(html: &str) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(pos) = html[from..].find("result__a") {
        let anchor_start = html[from + pos..].find("<a").map(|p| from + pos + p).unwrap_or(usize::MAX);
        if anchor_start == usize::MAX {
            break;
        }
        let seg = &html[anchor_start..];
        let Some(anchor_end) = seg.find("</a>") else { break };
        let anchor = &seg[..anchor_end];
        let title = strip_tags(anchor);
        // href 提取（uddg 参数内含真实 url，形如 //duckduckgo.com/l/?uddg=<enc>)
        let link = extract_attr(anchor, "href").unwrap_or_default();
        let real_url = decode_uddg(&link);
        // 紧随其后的 snippet
        let after = &html[anchor_start + anchor_end..];
        let snippet = after
            .find("result__snippet")
            .and_then(|p| {
                let s = &after[p..];
                let a = s.find("<a").map(|x| &s[x..])?;
                let e = a.find("</a>")?;
                Some(strip_tags(&a[..e]))
            })
            .unwrap_or_default();
        if !title.is_empty() {
            out.push((title, real_url, snippet));
        }
        from = anchor_start + anchor_end + 4;
        if out.len() >= SEARCH_RESULTS_LIMIT + 3 {
            break;
        }
    }
    out
}

/// uddg 参数解码：//duckduckgo.com/l/?uddg=<percent-encoded>&rut=...
fn decode_uddg(link: &str) -> String {
    let Some(p) = link.find("uddg=") else {
        return if link.starts_with("//") { format!("https:{link}") } else { link.to_string() };
    };
    let raw = &link[p + 5..];
    let end = raw.find('&').unwrap_or(raw.len());
    let enc = &raw[..end];
    percent_decode(enc)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() + 1 && i + 2 < bytes.len() + 1 {
            if let (Some(h), Some(l)) = (hex_val(bytes.get(i + 1).copied()), hex_val(bytes.get(i + 2).copied())) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn hex_val(b: Option<u8>) -> Option<u8> {
    match b? {
        c @ b'0'..=b'9' => Some(c - b'0'),
        c @ b'a'..=b'f' => Some(c - b'a' + 10),
        c @ b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn extract_attr(seg: &str, attr: &str) -> Option<String> {
    let key = format!("{attr}=\"");
    let p = seg.find(&key)?;
    let rest = &seg[p + key.len()..];
    let e = rest.find('"')?;
    Some(rest[..e].to_string())
}

/// 剥离 HTML 标签 + 实体最小解码（标题/摘要清洗）
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut depth = 0usize;
    for ch in html.chars() {
        match ch {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            c if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ")
        .trim()
        .to_string()
}

/// 页面抓取 → 可读文本（script/style 剔除 + 剥标签 + 截断）
fn open_url(url: &str) -> Result<String, String> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("仅支持 http/https URL".into());
    }
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(8))
        .timeout(std::time::Duration::from_secs(20))
        .build();
    let html = agent
        .get(url)
        .set("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
        .call()
        .map_err(|e| format!("页面请求失败: {e}"))?
        .into_string()
        .map_err(|e| format!("页面读取失败: {e}"))?;
    // script/style 块整体剔除
    let mut cleaned = html.to_lowercase();
    let _ = &mut cleaned;
    let body = remove_blocks(&html, "<script", "</script>");
    let body = remove_blocks(&body, "<style", "</style>");
    let mut text = strip_tags(&body);
    text = text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        return Err("页面无可读文本".into());
    }
    if text.chars().count() > PAGE_TEXT_LIMIT {
        text = text.chars().take(PAGE_TEXT_LIMIT).collect();
    }
    Ok(text)
}

/// 移除 <tag ...>...</tag> 块（大小写不敏感，块级剔除）
fn remove_blocks(html: &str, open: &str, close: &str) -> String {
    let lower = html.to_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut from = 0;
    loop {
        match lower[from..].find(open) {
            Some(p) => {
                let start = from + p;
                out.push_str(&html[from..start]);
                match lower[start..].find(close) {
                    Some(e) => from = start + e + close.len(),
                    None => break, // 未闭合：丢弃剩余
                }
            }
            None => {
                out.push_str(&html[from..]);
                break;
            }
        }
    }
    out
}

/// 代执行历史 → Responses web_search_call 输出项
pub fn search_call_items(records: &[SearchRecord], resp_id: &str) -> Vec<Value> {
    records
        .iter()
        .filter(|r| r.tool == TOOL_SEARCH)
        .enumerate()
        .map(|(i, r)| {
            json!({
                "id": format!("ws_{}_{}", resp_id, i),
                "type": "web_search_call",
                "status": "completed",
                "action": { "type": "search", "query": r.query },
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_web_search_tools() {
        let body = json!({"tools":[{"type":"web_search"},{"type":"function","name":"shell"}]});
        assert!(responses_declares_web_search(&body));
        let body2 = json!({"tools":[{"type":"web_search_preview"}]});
        assert!(responses_declares_web_search(&body2));
        let none = json!({"tools":[{"type":"function","name":"shell"}]});
        assert!(!responses_declares_web_search(&none));
        assert!(!responses_declares_web_search(&json!({})));
    }

    #[test]
    fn injects_tools_and_system_hint() {
        let mut body = json!({
            "messages": [
                {"role": "system", "content": "inst"},
                {"role": "user", "content": "hi"}
            ]
        });
        inject_proxy_tools(&mut body);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["function"]["name"], json!("web_search"));
        assert_eq!(tools[1]["function"]["name"], json!("open_url"));
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1]["role"], json!("system"), "提示插在 instructions 之后、首条用户消息之前");
        // 重复注入不重复加工具
        inject_proxy_tools(&mut body);
        assert_eq!(body["tools"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn extracts_only_proxy_tool_calls() {
        let c = json!({
            "choices": [{"message": {"tool_calls": [
                {"id": "c1", "function": {"name": "web_search", "arguments": "{\"query\":\"rust\"}"}},
                {"id": "c2", "function": {"name": "shell", "arguments": "{}"}},
                {"id": "c3", "function": {"name": "open_url", "arguments": "{\"url\":\"https://a\"}"}}
            ]}}]
        });
        let calls = extract_proxy_calls(&c);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "c1");
        assert_eq!(calls[0].1, "web_search");
        assert_eq!(calls[1].1, "open_url");
    }

    #[test]
    fn search_call_items_projection() {
        let records = vec![
            SearchRecord { tool: TOOL_SEARCH.into(), query: "rust 1.0".into(), ok: true },
            SearchRecord { tool: TOOL_OPEN_URL.into(), query: "https://a".into(), ok: true },
            SearchRecord { tool: TOOL_SEARCH.into(), query: "axum".into(), ok: false },
        ];
        let items = search_call_items(&records, "resp_1");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["type"], json!("web_search_call"));
        assert_eq!(items[0]["action"]["query"], json!("rust 1.0"));
        assert_eq!(items[1]["action"]["query"], json!("axum"));
    }

    #[test]
    fn url_encode_and_percent_decode_roundtrip() {
        let enc = url_encode("a b/c?d=中文");
        assert!(!enc.contains(' '));
        assert_eq!(percent_decode(&enc), "a b/c?d=中文");
        assert_eq!(percent_decode("a+b"), "a b");
    }

    #[test]
    fn strip_tags_and_blocks() {
        assert_eq!(strip_tags("<a href=\"x\" class=\"y\">标题</a>"), "标题");
        assert_eq!(strip_tags("a &amp; b"), "a & b");
        let html = "<html><script>bad()</script><style>.x{}</style><body>正文</body></html>";
        let b = remove_blocks(html, "<script", "</script>");
        let b = remove_blocks(&b, "<style", "</style>");
        assert_eq!(strip_tags(&b), "正文");
    }

    #[test]
    fn unknown_tool_errors_cleanly() {
        let (out, ok) = execute("shell", "{}");
        assert!(!ok);
        assert!(out.contains("unknown proxy tool"));
    }
}
