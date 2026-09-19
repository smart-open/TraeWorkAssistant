//! WorkBuddy 公共请求层（原 src-python/wb_common.py 的 Rust 移植）。
//! 依据 docs/workbuddy-product-design.md §5.1~§5.4：
//! - 凭证双源化（F-10）：工具侧 token_store 与桌面 auth 文件「谁新用谁」（expiresAtMs 晚者胜出）
//! - 统一请求头（§5.3）+ 宽容解析 dig（复用 fs_utils::dig 信封下钻语义，对齐 wb.dig）
//! - 红线：chat 请求绝不携带 X-Refresh-Token；仅 refresh 端点携带
//! - 网络：ureq 默认直连（不读系统/环境代理），对齐 python OPENER 绕代理约定
//!
//! P1 先行落地（wb_credits 依赖）；P2 wb_checkin / trae_checkin 直接复用本层。

use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use crate::fs_utils;
use crate::state::AppState;

// ── 路径 ────────────────────────────────────────────────────────────────────
// （SQLite 化 P3：pool/token store/checkin results 均改走 store，文件路径函数已删除）

/// 桌面 auth 文件读取路径：settings.wb_auth_file_path 人工指定优先（与
/// commands/workbuddy/common.rs auth_file_path_of 同语义），否则默认布局。
fn auth_file_path(state: &AppState) -> PathBuf {
    if let Some(p) = state.settings().wb_auth_file_path.as_deref() {
        let p = p.trim();
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    // F-75 跨平台收口：Windows=%LOCALAPPDATA%，mac=~/Library/Application Support
    crate::platform::local_data_root_lossy()
        .join("CodeBuddyExtension")
        .join("Data")
        .join("Public")
        .join("auth")
        .join("workbuddy-desktop.info")
}

// ── 凭证结构（对齐 python creds_of 返回 dict）───────────────────────────────

#[derive(Serialize, Clone, Default, Debug)]
pub struct Creds {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub expires_at_ms: Option<i64>,
    #[serde(default)]
    pub refresh_expires_at_ms: Option<i64>,
    #[serde(default)]
    pub uid: String,
    #[serde(default)]
    pub domain: String,
    #[serde(default)]
    pub nickname: String,
    #[serde(default)]
    pub edition: String,
}

fn s_of(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or("").to_string()
}

/// 宽容整数：数字或数字字符串（对齐 python int(v) 兜底）
fn i_of(v: Option<&Value>) -> Option<i64> {
    let v = v?;
    v.as_i64().or_else(|| v.as_str()?.trim().parse::<i64>().ok())
}

/// 从 auth 文件 / token store 记录中提取凭证字段（兼容多种嵌套形态，F-04）。
pub fn creds_of(source: &Value) -> Creds {
    let auth = source.get("auth").filter(|v| v.is_object()).unwrap_or(source);
    let account = source
        .get("account")
        .filter(|v| v.is_object())
        .unwrap_or(source);
    Creds {
        access_token: s_of(fs_utils::dig(auth, &["accessToken", "access_token", "token"])),
        refresh_token: s_of(fs_utils::dig(auth, &["refreshToken", "refresh_token"])),
        expires_at_ms: i_of(fs_utils::dig(
            auth,
            &[
                "expiresAtMs",
                "expires_at_ms",
                "expiresAt",
                "expires_in_ms",
                "accessTokenExpiresAtMs",
            ],
        )),
        refresh_expires_at_ms: i_of(fs_utils::dig(
            auth,
            &["refreshExpiresAtMs", "refresh_expires_at_ms", "refreshExpiresAt"],
        )),
        uid: s_of(fs_utils::dig(account, &["uid", "userId", "user_id", "id"])),
        domain: s_of(fs_utils::dig(source, &["domain"])),
        nickname: s_of(fs_utils::dig(account, &["nickname", "name", "displayName"])),
        edition: s_of(fs_utils::dig(account, &["editionType", "edition_type", "edition"])),
    }
}

fn read_auth_file(state: &AppState) -> Creds {
    creds_of(&fs_utils::read_json::<Value>(&auth_file_path(state)))
}

/// 生效凭证 = token store 与 auth 文件中 expiresAtMs 更晚者（F-10 谁新用谁）。
/// auth 文件仅当其 uid 与账号匹配时参与双源比较（桌面当前登录态）。
/// 读 token store（SQLite 化 P3：wb_tokens 表；结构 {version, tokens:{id:rec}}）。
/// 全部 token store 读点统一入口。
pub fn load_token_store(state: &AppState) -> Value {
    crate::store::docs::wb_token_store_load(&crate::store::db(&state.data_dir))
}

pub fn effective_creds(state: &AppState, acct_id: &str, acct_uid: &str) -> Creds {
    let store: Value = load_token_store(state);
    let store_creds = store
        .get("tokens")
        .and_then(|t| t.get(acct_id))
        .map(creds_of)
        .unwrap_or_default();
    let mut file_creds = read_auth_file(state);
    if !acct_uid.is_empty() && !file_creds.uid.is_empty() && file_creds.uid != acct_uid {
        file_creds = Creds::default();
    }
    let a = store_creds.expires_at_ms;
    let b = file_creds.expires_at_ms;
    // python: file 有 token 且 (store 无到期时间 或 file 到期 >= store 到期)
    let file_newer = a.is_none() || b.is_some_and(|bv| a.map_or(true, |av| bv >= av));
    if !file_creds.access_token.is_empty() && file_newer {
        return file_creds;
    }
    store_creds
}

/// 写工具侧凭证副本（F-10 谁新用谁）。version≠1 拒绝写入（版本闸门）；
/// 非空字段合并 + updated_at（与 commands/workbuddy/common.rs upsert_token_store 同语义）。
pub fn save_token_store(state: &AppState, id: &str, creds: &Creds) -> Result<(), String> {
    let mut store: Value = load_token_store(state);
    if !store.is_object() {
        store = serde_json::json!({});
    }
    let obj = store.as_object_mut().unwrap();
    match obj.get("version") {
        Some(v) if v.as_i64() != Some(1) => {
            return Err("token_store 版本不识别，拒绝写入".to_string());
        }
        _ => {}
    }
    obj.insert("version".into(), serde_json::json!(1));
    let tokens = obj.entry("tokens").or_insert_with(|| serde_json::json!({}));
    if let Some(t) = tokens.as_object_mut() {
        let mut rec = t.get(id).cloned().unwrap_or(serde_json::json!({}));
        if let Some(rm) = rec.as_object_mut() {
            let val = serde_json::to_value(creds).map_err(|e| e.to_string())?;
            for (k, v) in val.as_object().into_iter().flatten() {
                if !v.is_null() {
                    rm.insert(k.clone(), v.clone());
                }
            }
            rm.insert("updated_at".into(), serde_json::json!(fs_utils::now_iso()));
        }
        t.insert(id.to_string(), rec);
    }
    crate::store::docs::wb_token_store_save(&crate::store::db(&state.data_dir), &store)
}

// ── 统一请求头（§5.3）───────────────────────────────────────────────────────

/// Bearer + X-User-Id（缺省 X-No-* 占位）；web_platform=true 附加
/// X-Client-Platform: web（积分三件套必需）。
pub fn build_auth_headers(creds: &Creds, web_platform: bool) -> Vec<(String, String)> {
    let mut h = vec![
        (
            "Authorization".to_string(),
            format!("Bearer {}", creds.access_token),
        ),
        ("User-Agent".to_string(), "WorkBuddy".to_string()),
        ("Content-Type".to_string(), "application/json".to_string()),
    ];
    if creds.uid.is_empty() {
        h.push(("X-No-User-Id".to_string(), "1".to_string()));
    } else {
        h.push(("X-User-Id".to_string(), creds.uid.clone()));
    }
    if web_platform {
        h.push(("X-Client-Platform".to_string(), "web".to_string()));
    }
    h
}

/// POST JSON → (http_status, parsed, raw_text)；status=0 表示网络不可达
///（对齐 python post_json 三元组；HTTPError 同样返回状态码与响应体原文）。
pub fn post_json_raw(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(String, String)],
    body: &Value,
) -> (u16, Option<Value>, String) {
    let mut req = agent.post(url);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    match req.send_string(&body.to_string()) {
        Ok(resp) => {
            let raw = resp.into_string().unwrap_or_default();
            let parsed = serde_json::from_str(&raw).ok();
            (200, parsed, raw)
        }
        Err(ureq::Error::Status(code, resp)) => {
            let raw = resp.into_string().unwrap_or_default();
            let parsed = serde_json::from_str(&raw).ok();
            (code, parsed, raw)
        }
        Err(e) => (0, None, e.to_string()),
    }
}

/// POST JSON → (http_status, parsed)；status=0 表示网络不可达
///（对齐 python post_json 三元组的可消费部分；HTTPError 同样返回状态码）。
pub fn post_json(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(String, String)],
    body: &Value,
) -> (u16, Option<Value>) {
    let (status, parsed, _) = post_json_raw(agent, url, headers, body);
    (status, parsed)
}

/// GET 请求 → (http_status, parsed, raw_text)；status=0 网络不可达
///（对齐 python get_json，F-17 成长中心等 GET 端点；HTTPError 同样返回状态码）。
pub fn get_json(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(String, String)],
) -> (u16, Option<Value>, String) {
    let mut req = agent.get(url);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    match req.call() {
        Ok(resp) => {
            let raw = resp.into_string().unwrap_or_default();
            let parsed = serde_json::from_str(&raw).ok();
            (200, parsed, raw)
        }
        Err(ureq::Error::Status(code, resp)) => {
            let raw = resp.into_string().unwrap_or_default();
            let parsed = serde_json::from_str(&raw).ok();
            (code, parsed, raw)
        }
        Err(e) => (0, None, e.to_string()),
    }
}

// ── 区域路由（T4.5/F-36，§5.2）──────────────────────────────────────────────
// CN：billing/积分 + 活动接口走 www.codebuddy.cn；Global（domain 含 .workbuddy.ai）：
// 全走 www.workbuddy.ai。plugin 网关（token refresh）固定 codebuddy.cn 不随区域。

pub const BILLING_BASE_CN: &str = "https://www.codebuddy.cn";
pub const BILLING_BASE_GLOBAL: &str = "https://www.workbuddy.ai";
pub const REFRESH_URL: &str = "https://www.codebuddy.cn/v2/plugin/auth/token/refresh";

pub fn is_global_region(domain: &str) -> bool {
    domain.contains(".workbuddy.ai")
}

pub fn region_billing_base(domain: &str) -> &'static str {
    if is_global_region(domain) {
        BILLING_BASE_GLOBAL
    } else {
        BILLING_BASE_CN
    }
}

/// 域名双探测（§2.2 接口稳定性）：主域名在前、备用域名在后。
pub fn billing_bases(domain: &str) -> [&'static str; 2] {
    let main = region_billing_base(domain);
    let alt = if main == BILLING_BASE_CN {
        BILLING_BASE_GLOBAL
    } else {
        BILLING_BASE_CN
    };
    [main, alt]
}

// ── token 刷新（F-09）──────────────────────────────────────────────────────

/// 调 plugin refresh 端点（X-Refresh-Token 仅允许出现在此端点）。
/// 成功返回新 Creds（expires 字段按 expiresIn/refreshExpiresIn 秒数回填）。
pub fn refresh_token_once(agent: &ureq::Agent, creds: &Creds) -> Option<Creds> {
    if creds.refresh_token.is_empty() {
        return None;
    }
    let mut h = build_auth_headers(creds, false);
    h.push(("X-Refresh-Token".to_string(), creds.refresh_token.clone()));
    h.push((
        "X-Auth-Refresh-Source".to_string(),
        "workbuddy".to_string(),
    ));
    let (status, body) = post_json(agent, REFRESH_URL, &h, &serde_json::json!({}));
    if status != 200 {
        return None;
    }
    let body = body?;
    let acc = s_of(fs_utils::dig(&body, &["accessToken"]));
    if acc.is_empty() {
        return None;
    }
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut out = creds.clone();
    out.access_token = acc;
    let ref_tok = s_of(fs_utils::dig(&body, &["refreshToken"]));
    if !ref_tok.is_empty() {
        out.refresh_token = ref_tok;
    }
    out.expires_at_ms = i_of(fs_utils::dig(&body, &["expiresIn"])).map(|e| now_ms + e * 1000);
    out.refresh_expires_at_ms = i_of(fs_utils::dig(&body, &["refreshExpiresIn"]))
        .map(|e| now_ms + e * 1000);
    Some(out)
}

// ── 本地 quota 端口发现兜底（T5.8/F-21）────────────────────────────────────
// 云端 billing 全链失败时的最后兜底：WorkBuddy/CodeBuddy 桌面端本地服务在
// 127.0.0.1 暴露 quota 查询端点。发现顺序：
// ① ~/.workbuddy/*.port 文件声明的端口；② 固定候选端口；③ 有界端口段。
// 红线：单次单发不重试、每端口 0.8s 超时、候选总数有界。

const QUOTA_PATH: &str = "/api/v1/quota";
const QUOTA_PORT_CANDIDATES: [u16; 4] = [18789, 11101, 8890, 8899];
const QUOTA_PORT_RANGE: std::ops::RangeInclusive<u16> = 18780..=18795;

/// 响应含 remaining/credits/quota/balance 任一键即认定 quota 端点（浅层宽容）。
fn quota_looks_valid(v: &Value, depth: usize) -> bool {
    if depth > 3 {
        return false;
    }
    if let Some(map) = v.as_object() {
        for (k, val) in map {
            let kl = k.to_ascii_lowercase();
            if kl == "remaining" || kl == "credits" || kl == "quota" || kl == "balance" {
                return true;
            }
            if quota_looks_valid(val, depth + 1) {
                return true;
            }
        }
    }
    false
}

fn quota_probe_port(agent: &ureq::Agent, port: u16) -> Option<Value> {
    let url = format!("http://127.0.0.1:{port}{QUOTA_PATH}");
    let resp = agent
        .get(&url)
        .timeout(Duration::from_millis(800))
        .set("User-Agent", "WorkBuddy")
        .set("Accept", "application/json")
        .call()
        .ok()?;
    let raw = resp.into_string().ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    quota_looks_valid(&v, 0).then_some(v)
}

/// 发现本机 quota 服务：*.port 声明端口 → 固定候选 → 有界端口段。
/// 返回已确认可用的 (port, 响应)（按发现序，至多 limit 个）。
pub fn discover_local_quota_services(
    agent: &ureq::Agent,
    limit: usize,
) -> Vec<(u16, Value)> {
    let mut found: Vec<(u16, Value)> = vec![];
    let mut seen: std::collections::HashSet<u16> = Default::default();
    let mut ports: Vec<u16> = vec![];
    // ① ~/.workbuddy/*.port（服务启动时落盘的端口声明，最多扫 16 个）
    let wb_dir = crate::platform::home_dir().join(".workbuddy");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&wb_dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "port").unwrap_or(false))
        .collect();
    files.sort();
    for f in files.into_iter().take(16) {
        if let Ok(txt) = std::fs::read_to_string(&f) {
            if let Some(first) = txt.split_whitespace().next() {
                if let Ok(p) = first.parse::<u16>() {
                    ports.push(p);
                }
            }
        }
    }
    // ② 固定候选 ③ 有界端口段
    for p in ports
        .into_iter()
        .chain(QUOTA_PORT_CANDIDATES.into_iter())
        .chain(QUOTA_PORT_RANGE.clone())
    {
        if seen.contains(&p) || p == 0 {
            continue;
        }
        seen.insert(p);
        if found.len() >= limit {
            return found;
        }
        if let Some(v) = quota_probe_port(agent, p) {
            found.push((p, v));
        }
    }
    found
}

/// 本地 quota 兜底余额：首个可用端点的 remaining/credits/balance 取数；无可用端点 None。
pub fn local_quota_balance(agent: &ureq::Agent) -> Option<f64> {
    for (_port, v) in discover_local_quota_services(agent, 2) {
        if let Some(num) = dig_num(fs_utils::dig(
            &v,
            &["remaining", "RemainingCapacity", "credits", "balance"],
        )) {
            return Some(num);
        }
    }
    None
}

/// 宽容取数：递归摘出首个数值（含数字字符串）。
pub fn dig_num(v: Option<&Value>) -> Option<f64> {
    let v = v?;
    match v {
        Value::Null => None,
        Value::Bool(_) => None,
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        Value::Object(m) => m.values().find_map(|c| dig_num(Some(c))),
        Value::Array(a) => a.iter().find_map(|c| dig_num(Some(c))),
    }
}

// ── 惰性刷新（F-09/F-55，原 wb_common.ensure_fresh）────────────────────────

/// 惰性刷新：距过期 < lazy_hours 才刷；一次调用最多一次刷新。
/// 返回 (creds, refreshed, note)，note ∈ no_credential/fresh/expired_needs_relogin/refreshed/refresh_failed。
pub fn ensure_fresh(
    state: &AppState,
    agent: &ureq::Agent,
    acct: &Value,
    lazy_hours: i64,
) -> (Creds, bool, &'static str) {
    let acct_id = acct.get("id").and_then(Value::as_str).unwrap_or("");
    let acct_uid = acct.get("uid").and_then(Value::as_str).unwrap_or("");
    let creds = effective_creds(state, acct_id, acct_uid);
    if creds.access_token.is_empty() {
        return (creds, false, "no_credential");
    }
    if let Some(exp) = creds.expires_at_ms {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let remain_h = (exp - now_ms) as f64 / 3_600_000.0;
        if remain_h > lazy_hours as f64 {
            return (creds, false, "fresh");
        }
        if remain_h < 0.0 && creds.refresh_token.is_empty() {
            return (creds, false, "expired_needs_relogin");
        }
    }
    if let Some(new) = refresh_token_once(agent, &creds) {
        let _ = save_token_store(state, acct_id, &new);
        return (new, true, "refreshed");
    }
    (creds, false, "refresh_failed")
}
