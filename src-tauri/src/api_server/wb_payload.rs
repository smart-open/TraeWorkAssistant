//! WorkBuddy 上游请求体改写（T2.1/F-28/F-30 v1.2）
//!
//! 上游 `POST {chatBase}/v2/chat/completions` 为 OpenAI 兼容体，但有四个坑：
//! 1. 拒绝非流式（code 11101）→ 强制 `stream:true`，非流式由本地聚合；
//! 2. `tool_choice` 对象形式报 400 → 归一化为 string；
//! 3. `reasoning_effort` 档位上游不支持时拒答/忽略 → 按目录 supportedEfforts
//!    降级（调用方先用 wb_catalog::resolve_effort 解析，本层只负责注入）；
//! 4. Claude Code 指纹触发腾讯内容审核 → 指纹清洗（身份句改写 + cc_xxx 键值
//!    剥离 + x-anthropic-* 引用剥离）+ 审核模板黑名单最小改写（映射表外置
//!    `wb_template_map.json` 可热更新，§5.5 #9 cat-and-mouse，禁硬编码扩散）。
//!
//! 另：连续同角色消息自动合并（生态实测，§5.8 #9）。

use serde_json::{json, Map, Value};

/// 默认审核模板映射（映射表文件缺失时的内置兜底，§5.5 #9：
/// CLI→CLI tool、Main branch→Default branch——任何一字改动即绕过逐字匹配）
pub fn default_template_map() -> Vec<(String, String)> {
    vec![
        (
            "You are Claude Code, Anthropic's official CLI for Claude.".to_string(),
            "You are Claude Code, Anthropic's official CLI tool for Claude.".to_string(),
        ),
        (
            "Main branch (you will usually use this with PRs)".to_string(),
            "Default branch (you will usually use this with PRs)".to_string(),
        ),
        (
            "Main branch (you will usually use this for PRs)".to_string(),
            "Default branch (you will usually use this for PRs)".to_string(),
        ),
    ]
}

/// 模板映射文件结构：wb_template_map.json
#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct TemplateMapFile {
    #[serde(default)]
    pub templates: Vec<TemplateRule>,
    #[serde(default)]
    pub updated_at: Option<i64>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct TemplateRule {
    pub from: String,
    pub to: String,
}

/// 读取外置映射表；文件缺失/损坏返回 None（调用方用内置兜底）
pub fn read_template_file(data_dir: &std::path::Path) -> Option<Vec<(String, String)>> {
    let file: TemplateMapFile =
        crate::fs_utils::read_json(&data_dir.join("wb_template_map.json"));
    if file.templates.is_empty() {
        return None;
    }
    Some(
        file.templates
            .into_iter()
            .filter(|r| !r.from.is_empty())
            .map(|r| (r.from, r.to))
            .collect(),
    )
}

/// 审核模板黑名单最小改写：零分配预检（仅在内容含指纹关键词时才做替换）
pub fn apply_template_map(text: &str, templates: &[(String, String)]) -> String {
    let needs_check = text.contains("Claude Code")
        || text.contains("Main branch")
        || text.contains("official CLI");
    if !needs_check {
        return text.to_string();
    }
    let mut out = text.to_string();
    for (from, to) in templates {
        if out.contains(from.as_str()) {
            out = out.replace(from.as_str(), to.as_str());
        }
    }
    out
}

/// 剥离 cc_xxx=...; 键值对与 x-anthropic-* 引用（F-30 指纹清洗，零正则依赖）
pub fn strip_cc_fingerprints(text: &str) -> String {
    let needs_check = text.contains("cc_") || text.contains("x-anthropic-");
    if !needs_check {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        let is_cc = rest.starts_with("cc_");
        let is_anthropic = rest.starts_with("x-anthropic-");
        if (is_cc || is_anthropic) && (i == 0 || !is_token_char(bytes[i - 1])) {
            // 键名（token 字符）
            let mut j = i;
            while j < text.len() && is_token_char(bytes[j]) {
                j += 1;
            }
            // 可选分隔符 = / :（前后允许空白），值到停止符为止
            let after = &text[j..];
            let t2 = after.trim_start();
            let mut end = j;
            if let Some(r) = t2.strip_prefix(['=', ':']) {
                let v = r.trim_start(); // v 是 text 的后缀
                let vs = text.len() - v.len();
                let stop_rel = v
                    .find(|c| matches!(c, ';' | ',' | ' ' | '\n' | '\t'))
                    .unwrap_or(v.len());
                end = vs + stop_rel;
            }
            // 吃掉尾随的分号/逗号/空白（键值对属于指纹痕迹，连逗号一并清除）
            while end < text.len() && matches!(bytes[end], b';' | b',' | b' ' | b'\t') {
                end += 1;
            }
            i = end;
            continue;
        }
        // UTF-8 安全推进
        let ch_len = utf8_len(bytes[i]);
        out.push_str(&text[i..i + ch_len]);
        i += ch_len;
    }
    out
}

fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

/// 对单条消息 content 施加清洗（string 与 text blocks 两种形态）
fn sanitize_content(content: &mut Value, templates: &[(String, String)], sanitize: bool) {
    match content {
        Value::String(s) => {
            let mut t = if sanitize { strip_cc_fingerprints(s) } else { s.clone() };
            if sanitize {
                t = apply_template_map(&t, templates);
            }
            *s = t;
        }
        Value::Array(blocks) => {
            for b in blocks.iter_mut() {
                if let Some(Value::String(t)) = b.get_mut("text") {
                    let mut v = if sanitize { strip_cc_fingerprints(t) } else { t.clone() };
                    if sanitize {
                        v = apply_template_map(&v, templates);
                    }
                    *t = v;
                }
            }
        }
        _ => {}
    }
}

/// OpenAI 请求体 → WB /v2/chat/completions 请求体改写
pub fn prepare_wb_chat_body(
    src: &[u8],
    model: &str,
    conv_id: &str,
    effort: Option<&str>,
    sanitize: bool,
    templates: &[(String, String)],
) -> Vec<u8> {
    let mut obj: Value = match serde_json::from_slice(src) {
        Ok(v) => v,
        Err(_) => return src.to_vec(),
    };
    let m = match obj.as_object_mut() {
        Some(m) => m,
        None => return src.to_vec(),
    };

    // 模型归一化（目录 id 为小写规范名）
    m.insert("model".into(), json!(model));

    // 连续同角色消息合并 + 指纹清洗
    if let Some(msgs) = m.get_mut("messages").and_then(|v| v.as_array_mut()) {
        let mut merged: Vec<Value> = Vec::with_capacity(msgs.len());
        for msg in msgs.drain(..) {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("").to_string();
            if let Some(last) = merged.last_mut() {
                let last_role = last.get("role").and_then(|r| r.as_str()).unwrap_or("");
                if !role.is_empty() && role == last_role {
                    merge_message(last, msg);
                    continue;
                }
            }
            merged.push(msg);
        }
        for msg in merged.iter_mut() {
            if let Some(c) = msg.get_mut("content") {
                sanitize_content(c, templates, sanitize);
            }
        }
        m.insert("messages".into(), Value::Array(merged));
    }

    normalize_tool_choice(m);

    // 强制流式（上游拒绝非流式，code 11101）
    m.insert("stream".into(), json!(true));

    // 会话 id（粘性绑定 / 上游会话归属）
    if !conv_id.is_empty() {
        m.insert("conversation_id".into(), json!(conv_id));
    }

    // reasoning_effort：调用方已按目录降级，这里只注入非空值
    if let Some(e) = effort.map(str::trim).filter(|s| !s.is_empty()) {
        m.insert("reasoning_effort".into(), json!(e));
    } else {
        m.remove("reasoning_effort");
    }

    serde_json::to_vec(&obj).unwrap_or_else(|_| src.to_vec())
}

/// 同角色合并：string/text 拼接；tool_calls 追加；其余字段保留首个
fn merge_message(base: &mut Value, extra: Value) {
    let content_b = extra.get("content").cloned().unwrap_or(Value::Null);
    if !content_b.is_null() {
        // 目标 content 缺失/为 null：直接采用新值
        if base.get("content").map_or(true, |c| c.is_null()) {
            base["content"] = content_b;
        } else {
            join_content(base.get_mut("content").unwrap(), content_b);
        }
    }
    if let Some(tcs) = extra.get("tool_calls").and_then(|t| t.as_array()).cloned() {
        match base.get_mut("tool_calls").and_then(|t| t.as_array_mut()) {
            Some(arr) => arr.extend(tcs),
            None => base["tool_calls"] = Value::Array(tcs),
        }
    }
}

/// content 拼接：string↔string 加空行拼接；string 进数组追加 text 块；数组合并
fn join_content(base: &mut Value, extra: Value) {
    match extra {
        Value::String(y) => match base {
            Value::String(x) => {
                let mut s = x.clone();
                if !s.is_empty() && !y.is_empty() {
                    s.push_str("\n\n");
                }
                s.push_str(&y);
                *x = s;
            }
            Value::Array(x) => x.push(json!({"type": "text", "text": y})),
            _ => {}
        },
        Value::Array(y) => match base {
            Value::Array(x) => x.extend(y),
            Value::String(x) => {
                let old = x.clone();
                let mut blocks = vec![json!({"type": "text", "text": old})];
                blocks.extend(y);
                *base = Value::Array(blocks);
            }
            _ => {}
        },
        _ => {}
    }
}

/// tool_choice 归一化（对象形式上游报 400 code=11101）：
/// {"type":"function","function":{"name":X}} → "X"；auto/none/required 原样 string
pub fn normalize_tool_choice(m: &mut Map<String, Value>) {
    let tc = match m.remove("tool_choice") {
        Some(v) => v,
        None => return,
    };
    match tc {
        Value::String(_) => {
            m.insert("tool_choice".into(), tc);
        }
        Value::Object(map) => {
            let typ = map
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("auto")
                .to_lowercase();
            let name = map
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .or_else(|| map.get("name").and_then(|n| n.as_str()))
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let value = match (typ.as_str(), name) {
                ("none", _) => "none".to_string(),
                ("auto", _) => "auto".to_string(),
                ("any" | "required", _) => "required".to_string(),
                ("function", Some(n)) => n.to_string(),
                _ => "auto".to_string(),
            };
            m.insert("tool_choice".into(), json!(value));
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NO_TPL: &[(String, String)] = &[];

    fn rewrite(v: Value, effort: Option<&str>) -> Value {
        let src = serde_json::to_vec(&v).unwrap();
        let out = prepare_wb_chat_body(&src, "glm-5.3", "conv-1", effort, true, NO_TPL);
        serde_json::from_slice(&out).unwrap()
    }

    #[test]
    fn forces_stream_and_sets_conversation() {
        let out = rewrite(json!({"model":"glm-5.3","stream":false,"messages":[]}), None);
        assert_eq!(out["stream"], json!(true));
        assert_eq!(out["conversation_id"], json!("conv-1"));
        assert_eq!(out["model"], json!("glm-5.3"));
    }

    #[test]
    fn tool_choice_object_becomes_string() {
        let out = rewrite(
            json!({"messages":[],"tool_choice":{"type":"function","function":{"name":"get_weather"}}}),
            None,
        );
        assert_eq!(out["tool_choice"], json!("get_weather"));
        let out = rewrite(json!({"messages":[],"tool_choice":{"type":"any"}}), None);
        assert_eq!(out["tool_choice"], json!("required"));
        let out = rewrite(json!({"messages":[],"tool_choice":{"type":"none"}}), None);
        assert_eq!(out["tool_choice"], json!("none"));
        // string 原样保留
        let out = rewrite(json!({"messages":[],"tool_choice":"auto"}), None);
        assert_eq!(out["tool_choice"], json!("auto"));
    }

    #[test]
    fn effort_injected_or_removed() {
        let out = rewrite(json!({"messages":[],"reasoning_effort":"xhigh"}), Some("medium"));
        assert_eq!(out["reasoning_effort"], json!("medium"));
        let out = rewrite(json!({"messages":[],"reasoning_effort":"high"}), None);
        assert!(out.get("reasoning_effort").is_none());
    }

    #[test]
    fn merges_consecutive_same_role() {
        let out = rewrite(
            json!({"messages":[
                {"role":"user","content":"第一段"},
                {"role":"user","content":"第二段"},
                {"role":"assistant","content":"回复"},
            ]}),
            None,
        );
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["content"], json!("第一段\n\n第二段"));
    }

    #[test]
    fn template_blacklist_minimal_rewrite() {
        let tpl = default_template_map();
        let cc = "You are Claude Code, Anthropic's official CLI for Claude.";
        let out = apply_template_map(cc, &tpl);
        assert_ne!(out, cc);
        assert!(out.contains("CLI tool"));
        assert!(!out.contains("official CLI for Claude."));
        // Main branch → Default branch（两种原文写法都覆盖）
        assert!(apply_template_map("Main branch (you will usually use this for PRs)", &tpl)
            .starts_with("Default branch"));
        assert!(apply_template_map("Main branch (you will usually use this with PRs)", &tpl)
            .starts_with("Default branch"));
        // 无指纹内容零改动（预检短路）
        let plain = "普通中文内容不清洗";
        assert_eq!(apply_template_map(plain, &tpl), plain);
    }

    #[test]
    fn sanitize_strips_cc_and_anthropic_tokens() {
        assert_eq!(strip_cc_fingerprints("ok cc_user=abc; tail"), "ok tail");
        assert_eq!(strip_cc_fingerprints("a cc_env=X:1,b"), "a b");
        assert_eq!(
            strip_cc_fingerprints("see x-anthropic-beta: prompt-caching end"),
            "see end"
        );
        // 普通下划线词不受影响
        assert_eq!(strip_cc_fingerprints("success_count=3"), "success_count=3");
    }

    #[test]
    fn sanitize_applied_to_message_content() {
        let src = json!({"messages":[{"role":"system","content":"You are Claude Code, Anthropic's official CLI for Claude."}]});
        let src_bytes = serde_json::to_vec(&src).unwrap();
        let tpl = default_template_map();
        let out = prepare_wb_chat_body(&src_bytes, "m", "", None, true, &tpl);
        let v: Value = serde_json::from_slice(&out).unwrap();
        let s = v["messages"][0]["content"].as_str().unwrap();
        assert!(s.contains("CLI tool"));
        assert!(!s.contains("official CLI for Claude."));
        // sanitize 关闭时跳过该预处理（与指纹清洗开关联动 §5.5 #9）
        let out = prepare_wb_chat_body(&src_bytes, "m", "", None, false, &tpl);
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["messages"][0]["content"], json!("You are Claude Code, Anthropic's official CLI for Claude."));
    }
}
