//! CodeArts（华为云码道，国内站）每日签到引擎。
//!
//! 端点（SDK-HMAC-SHA256 签名，见 tasks::codearts_sign；`/v1/ops/*` 必带小写
//! `agent-type: PromptCenter` 参与签名）：
//! - `GET  /v1/ops/delivery?channel=IDE`：活动列表 + 四态预检（唯一真相源）
//! - `POST /v1/ops/claim`：领取（幂等键 `claim_<id>_<毫秒>`）
//! - `POST /v1/ops/confirm`：确认（claim 后或 CLAIMED 态补，失败忽略）
//!
//! NDJSON 进度契约（sink 每行一个 Value）：
//! - start `{type:"start", total:N}`
//! - 每账号 `{index,user_id,name,status,message,reward}`，status ∈ success/already/fail
//! - done `{type:"done",ok,already,failed,failed_empty_campaigns,failed_permanent,failed_blocked}`
//!   （commands 层再补 exit 行）。
//!
//! 红线：全程零 AK/SK 输出（仅从 vault 读，不进日志/事件/落库文案）。

use serde_json::{json, Value};

use crate::fs_utils;
use crate::state::AppState;

use super::codearts_common::{self, CodeartsCreds, TRANSIENT_ERR_TAG};
use super::codearts_sign::{
    self, CodeartsIdentity, OPS_CLAIM_PATH, OPS_CONFIRM_PATH, OPS_DELIVERY_PATH, SNAP_HOST,
};
use super::http_agent;

/// 签到轮次参数（对齐 qoder_checkin::QoderCheckinOpts；user_ids=None 跑全池）。
#[derive(Clone, Default)]
pub struct CodeartsCheckinOpts {
    pub user_ids: Option<Vec<String>>,
    #[allow(dead_code)]
    pub skip_checked: bool,
}

impl CodeartsCheckinOpts {
    /// 每日计划任务默认（skip_checked=true）。
    pub fn daily() -> Self {
        Self { user_ids: None, skip_checked: true }
    }
}

// ── 解析辅助 ───────────────────────────────────────────────────────────────

fn s_of(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or("").to_string()
}

fn num_of(v: Option<&Value>) -> Option<f64> {
    let v = v?;
    match v {
        Value::Bool(_) => None,
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// campaignId 归一：数字原样、字符串保留（既能作 body 的 N，也能拼幂等键）。
fn campaign_id_of(c: &Value) -> Value {
    fs_utils::dig(c, &["campaignId", "campaign_id", "id"])
        .cloned()
        .unwrap_or(Value::Null)
}

fn campaign_id_key(c: &Value) -> String {
    match campaign_id_of(c) {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s,
        _ => String::new(),
    }
}

/// 业务码：0 / 缺失视为成功；非 0 返回 code（用于记 fail）。
fn envelope_code(body: &Value) -> i64 {
    num_of(fs_utils::dig(body, &["code"])).map(|n| n as i64).unwrap_or(0)
}

fn envelope_message(body: &Value) -> String {
    s_of(fs_utils::dig(body, &["message", "msg"]))
}

/// 活动数组宽容提取：信封 data 为数组直接用；data 为对象则取 list/campaigns/items。
fn extract_campaigns(body: &Value) -> Vec<Value> {
    let data = fs_utils::dig(body, &["data"]).unwrap_or(body);
    if let Some(arr) = data.as_array() {
        return arr.clone();
    }
    for k in ["list", "campaigns", "items", "records"] {
        if let Some(arr) = data.get(k).and_then(Value::as_array) {
            return arr.clone();
        }
    }
    Vec::new()
}

/// 硬排除活动类型：邀请注册 / 学生认证不参与签到。
fn is_excluded(c: &Value) -> bool {
    let t = s_of(fs_utils::dig(c, &["type", "campaignType"])).to_ascii_uppercase();
    t == "INVITE_USER" || t == "STUDENT_CERTIFIED"
}

/// 每日签到活动：type=="USER_LOGIN" 或 campaignId==1。
fn is_daily(c: &Value) -> bool {
    let t = s_of(fs_utils::dig(c, &["type", "campaignType"])).to_ascii_uppercase();
    if t == "USER_LOGIN" {
        return true;
    }
    num_of(fs_utils::dig(c, &["campaignId", "campaign_id", "id"]))
        .map(|n| n as i64 == 1)
        .unwrap_or(false)
}

/// claimable 标志（布尔或 1）。
fn is_claimable(c: &Value) -> bool {
    match fs_utils::dig(c, &["claimable"]) {
        Some(Value::Bool(b)) => *b,
        Some(v) => num_of(Some(v)).map(|n| n > 0.0).unwrap_or(false),
        None => false,
    }
}

// ── 签名 + ureq 请求（body 字节级一致）─────────────────────────────────────

/// 统一签名请求：extra 头 agent-type 小写参与签名；POST 用发送 bytes 签名，
/// 保证「签名 body == 实际发送 body」字节一致。
/// 返回 (http_status, parsed_json, raw_text)；status=0 网络/签名失败。
/// 401/403 由调用方单独分类为 permanent_auth。
fn signed_request(
    agent: &ureq::Agent,
    identity: &CodeartsIdentity,
    method: &str,
    url: &str,
    body: &[u8],
) -> (u16, Option<Value>, String) {
    let extra: [(&str, &str); 1] = [("agent-type", "PromptCenter")];
    let signed = match codearts_sign::sign(identity, method, url, body, &extra) {
        Ok(s) => s,
        Err(_) => return (0, None, String::new()),
    };
    let mut req = match method {
        "GET" => agent.get(url),
        "POST" => agent.post(url),
        _ => return (0, None, String::new()),
    };
    req = req
        .set("Authorization", &signed.authorization)
        .set("x-sdk-date", &signed.x_sdk_date)
        .set("content-type", &signed.content_type);
    if let Some(st) = &signed.x_security_token {
        req = req.set("x-security-token", st.as_str());
    }
    if let Some(sha) = &signed.x_sdk_content_sha256 {
        req = req.set("x-sdk-content-sha256", sha.as_str());
    }
    for (k, v) in &signed.extra {
        req = req.set(k.as_str(), v.as_str());
    }
    let call = if method == "POST" {
        req.send_bytes(body)
    } else {
        req.call()
    };
    match call {
        Ok(resp) => {
            let code = resp.status();
            let raw = resp.into_string().unwrap_or_default();
            (code, serde_json::from_str(&raw).ok(), raw)
        }
        Err(ureq::Error::Status(code, resp)) => {
            let raw = resp.into_string().unwrap_or_default();
            (code, serde_json::from_str(&raw).ok(), raw)
        }
        Err(_) => (0, None, String::new()),
    }
}

/// 账号级失败分类（写入进度行 fail_kind 与结果落库）。
const FAIL_PERMANENT: &str = "permanent_auth";
const FAIL_EMPTY: &str = "empty";
const FAIL_BLOCKED: &str = "blocked";

/// confirm 单活动（body {"campaignId":N}，签名/发送字节一致）；失败静默忽略。
fn confirm_one(agent: &ureq::Agent, identity: &CodeartsIdentity, cid: &Value) {
    let payload = json!({ "campaignId": cid });
    let body = serde_json::to_vec(&payload).unwrap_or_default();
    let url = format!("https://{SNAP_HOST}{OPS_CONFIRM_PATH}");
    let _ = signed_request(agent, identity, "POST", &url, &body);
}

/// 单账号处理：返回 (status, message, reward, fail_kind)。
/// status ∈ "success" / "already" / "fail"；fail_kind 仅 fail 时有意义。
fn process_account(
    _state: &AppState,
    agent: &ureq::Agent,
    acct: &Value,
    creds: &CodeartsCreds,
) -> (String, String, Option<f64>, String) {
    let uid = s_of(acct.get("id"));
    if !creds.is_valid() {
        return ("fail".into(), "未配置有效的 AK/SK（请在账号管理中重新录入）".into(), None, FAIL_PERMANENT.into());
    }
    let identity = crate::tasks::codearts_sign::identity_from_creds(creds);

    // ① 活动列表（GET，body 为空字节参与签名）
    let url = format!("https://{SNAP_HOST}{OPS_DELIVERY_PATH}?channel=IDE");
    let (status, body, raw) = signed_request(agent, &identity, "GET", &url, b"");
    if status == 401 || status == 403 {
        return ("fail".into(), format!("AK/SK 鉴权失败（HTTP {status}，请检查密钥）"), None, FAIL_PERMANENT.into());
    }
    if status == 429 {
        return ("fail".into(), format!("{TRANSIENT_ERR_TAG} 上游限流（HTTP 429），稍后自动重试"), None, FAIL_BLOCKED.into());
    }
    if status == 0 {
        return ("fail".into(), format!("{TRANSIENT_ERR_TAG} 网络不可达（活动列表）"), None, String::new());
    }
    if status != 200 {
        return ("fail".into(), format!("活动列表不可用（HTTP {status}）"), None, String::new());
    }
    let Some(b) = body.filter(|b| b.is_object()) else {
        let head: String = raw.chars().take(120).collect();
        return ("fail".into(), format!("活动列表响应非 JSON（{head}）"), None, String::new());
    };
    let code = envelope_code(&b);
    if code != 0 {
        return ("fail".into(), format!("活动列表业务错误（code={code}）：{}", envelope_message(&b)), None, String::new());
    }
    let campaigns = extract_campaigns(&b);
    if campaigns.is_empty() {
        return ("fail".into(), "当前不在活动期（活动列表为空）".into(), None, FAIL_EMPTY.into());
    }

    // ② 逐活动四态处理
    let mut reward = 0.0;
    let mut any_reward = false;
    let mut did_success = false;
    let mut did_already = false;
    let mut did_empty = false;
    let mut last_fail: Option<(String, String)> = None; // (message, fail_kind)

    for c in &campaigns {
        if is_excluded(c) {
            continue;
        }
        let st = s_of(fs_utils::dig(c, &["status"])).to_ascii_uppercase();
        let cid = campaign_id_of(c);
        let cid_key = campaign_id_key(c);
        let daily = is_daily(c);
        let claimable = is_claimable(c);
        // 只处理：每日签到活动，或其它 claimable 且 ELIGIBLE 的活动；
        // status 缺失（不在活动期）跳过。
        if st.is_empty() {
            did_empty = true;
            continue;
        }
        if !(daily || (claimable && st == "ELIGIBLE")) {
            continue;
        }
        match st.as_str() {
            "ELIGIBLE" => {
                // claim（幂等键毫秒）
                let ms = chrono::Utc::now().timestamp_millis();
                let payload = json!({
                    "campaignId": cid,
                    "channel": "IDE",
                    "idempotentKey": format!("claim_{uid}_{ms}"),
                });
                let body_bytes = serde_json::to_vec(&payload).unwrap_or_default();
                let claim_url = format!("https://{SNAP_HOST}{OPS_CLAIM_PATH}");
                let (cs, cb, craw) = signed_request(agent, &identity, "POST", &claim_url, &body_bytes);
                if cs == 401 || cs == 403 {
                    return ("fail".into(), format!("AK/SK 鉴权失败（claim HTTP {cs}）"), None, FAIL_PERMANENT.into());
                }
                if cs == 0 {
                    last_fail = Some((format!("{TRANSIENT_ERR_TAG} 网络不可达（claim）"), String::new()));
                    continue;
                }
                let cbody = cb.filter(|b| b.is_object());
                let cc = cbody.as_ref().map(envelope_code).unwrap_or(-1);
                if cc == 40001 {
                    did_already = true;
                } else if cc == 40401 || cc == 4006 {
                    did_empty = true;
                } else if cc == 0 {
                    did_success = true;
                } else {
                    let msg = cbody
                        .as_ref()
                        .map(envelope_message)
                        .filter(|m| !m.is_empty())
                        .unwrap_or_else(|| {
                            let head: String = craw.chars().take(120).collect();
                            if head.is_empty() { format!("claim 业务错误（code={cc}）") } else { head }
                        });
                    last_fail = Some((msg, String::new()));
                    continue;
                }
                // reward：claim 响应 benefitAmount 优先，活动条目兜底
                let r = cbody
                    .as_ref()
                    .and_then(|b| num_of(fs_utils::dig(b, &["benefitAmount", "benefit_amount", "reward"])))
                    .or_else(|| num_of(fs_utils::dig(c, &["benefitAmount", "benefit_amount", "reward"])));
                if let Some(v) = r {
                    reward += v;
                    any_reward = true;
                }
                // confirm（claim 成功后或 CLAIMED 态，失败忽略）
                confirm_one(agent, &identity, &cid);
            }
            "CLAIMED" => {
                // 只 confirm（幂等）
                did_already = true;
                confirm_one(agent, &identity, &cid);
            }
            "CONFIRMED" | "CONSUMED" => {
                did_already = true;
            }
            other => {
                last_fail = Some((format!("活动状态未识别（status={other}, campaignId={cid_key}）"), String::new()));
            }
        }
    }

    // ③ 汇总结论（success > fail > already > empty）
    if did_success {
        return ("success".into(), "领取成功".into(), if any_reward { Some(reward) } else { None }, String::new());
    }
    if let Some((msg, fk)) = last_fail {
        return ("fail".into(), msg, if any_reward { Some(reward) } else { None }, fk);
    }
    if did_already {
        return ("already".into(), "今日已领取/已确认，无需重复操作".into(), if any_reward { Some(reward) } else { None }, String::new());
    }
    if did_empty {
        return ("fail".into(), "当前不在活动期（无可领活动）".into(), None, FAIL_EMPTY.into());
    }
    ("fail".into(), "无可处理的签到活动".into(), None, FAIL_EMPTY.into())
}

// ── 结果落库 ───────────────────────────────────────────────────────────────

/// 每条签到结果落库（批量 UPSERT，pk=date|user_id|time_ms）。
fn append_results(state: &AppState, rows: &[Value]) {
    if rows.is_empty() {
        return;
    }
    let db = crate::store::db(&state.data_dir);
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let time = fs_utils::now_ts();
    let mut out: Vec<Value> = Vec::with_capacity(rows.len());
    for r in rows {
        let mut rec = json!({
            "date": date,
            "time": time,
            "time_ms": chrono::Utc::now().timestamp_millis(),
            "user_id": r.get("user_id").cloned().unwrap_or_default(),
            "name": r.get("name").cloned().unwrap_or_default(),
            "status": r.get("status").cloned().unwrap_or_default(),
            "message": r.get("message").cloned().unwrap_or_default(),
        });
        if let Some(rw) = r.get("reward").filter(|v| !v.is_null()) {
            rec["reward"] = rw.clone();
        }
        if let Some(fk) = r.get("fail_kind").filter(|v| v.is_string() && !v.as_str().unwrap_or("").is_empty()) {
            rec["fail_kind"] = fk.clone();
        }
        out.push(rec);
    }
    if let Err(e) = crate::store::docs::codearts_checkin_results_upsert(&db, &out) {
        fs_utils::app_log(&state.data_dir, &format!("[codearts] 签到结果落库失败: {e}"));
    }
}

// ── 轮次主流程 ─────────────────────────────────────────────────────────────

/// 跑一轮签到：start → 每账号进度 → 落库 → done。
pub fn run_checkin_round(
    state: &AppState,
    opts: &CodeartsCheckinOpts,
    sink: &mut dyn FnMut(&Value),
) -> Value {
    let db = crate::store::db(&state.data_dir);
    let pool = crate::store::docs::codearts_pool_load(&db);
    let mut accounts: Vec<Value> = pool
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if let Some(ids) = &opts.user_ids {
        accounts.retain(|a| {
            ids.iter().any(|id| a.get("id").and_then(Value::as_str) == Some(id.as_str()))
        });
    }
    let total = accounts.len();
    sink(&json!({ "type": "start", "total": total }));

    let agent = http_agent(30);
    let mut done = json!({
        "type": "done",
        "ok": 0,
        "already": 0,
        "failed": 0,
        "failed_empty_campaigns": 0,
        "failed_permanent": 0,
        "failed_blocked": 0,
    });
    let mut db_rows: Vec<Value> = Vec::with_capacity(total);

    for (i, acct) in accounts.iter().enumerate() {
        let uid = s_of(acct.get("id"));
        let name = {
            let n = s_of(fs_utils::dig(acct, &["name", "nickname"]));
            if n.is_empty() { uid.clone() } else { n }
        };
        let creds = match codearts_common::ensure_fresh_creds(&state.data_dir, &uid) {
            Ok(c) => c,
            Err(e) => {
                fs_utils::app_log(&state.data_dir, &format!("[codearts] 账号 {uid} 凭证读取失败: {e}"));
                CodeartsCreds::default()
            }
        };
        let (st, msg, reward, fail_kind) = process_account(state, &agent, acct, &creds);

        // done 累计（账号维度）
        match st.as_str() {
            "success" => {
                if let Some(obj) = done.as_object_mut() {
                    let n = obj["ok"].as_i64().unwrap_or(0);
                    obj.insert("ok".into(), json!(n + 1));
                }
            }
            "already" => {
                if let Some(obj) = done.as_object_mut() {
                    let n = obj["already"].as_i64().unwrap_or(0);
                    obj.insert("already".into(), json!(n + 1));
                }
            }
            _ => {
                if let Some(obj) = done.as_object_mut() {
                    let n = obj["failed"].as_i64().unwrap_or(0);
                    obj.insert("failed".into(), json!(n + 1));
                    let key = match fail_kind.as_str() {
                        FAIL_PERMANENT => "failed_permanent",
                        FAIL_EMPTY => "failed_empty_campaigns",
                        FAIL_BLOCKED => "failed_blocked",
                        _ => "",
                    };
                    if !key.is_empty() {
                        let m = obj[key].as_i64().unwrap_or(0);
                        obj.insert(key.into(), json!(m + 1));
                    }
                }
            }
        }
        let progress = json!({
            "index": i,
            "user_id": uid,
            "name": name,
            "status": st,
            "message": msg,
            "reward": reward,
        });
        let mut db_row = progress.clone();
        db_row["fail_kind"] = json!(fail_kind);
        db_rows.push(db_row);
        sink(&progress);
    }

    // 先落库，再发 done（commands 层补 exit）
    append_results(state, &db_rows);
    sink(&done);
    done
}
