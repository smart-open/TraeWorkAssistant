//! 豆包每日签到（需求③）：detail 查询签到状态 → 未签账号提交 landing/checkin →
//! 按日落库 doubao_checkin_results + 账号池回写（quota_total/签到缓存）+ 运维历史。
//! 端点（2026-10 抓包固化）：
//! - POST /alice/ug/campaign/v3/detail/      body {"campaign_group_code":"pc_incentive_task_mvp_v3"}
//!   → data.progress.total_amount（累计获得额度 = 「总额度」口径）、
//!     data.tasks[] task_kind=="daily_signin" 的 daily_checkin（今日已签/连续天数/上次日期）
//! - POST /alice/ug/campaign/landing/checkin body {"scene":"pc_incentive_task_mvp_v3"}
//!   → data.checked_in / reward_amount / continuous_days / idempotent
//! msToken/a_bogus 为一次性反爬 token（CLI 无法生成），实测仅稳定 query 参数 + Cookie 即可通过。
//! 红线：凭证等同密码，仅本地使用；仅执行官方签到动作，不做代刷。

use serde_json::{json, Value};
use std::time::Duration;

use crate::fs_utils;
use crate::state::AppState;

use super::http_agent;

const TIMEOUT_SECS: u64 = 15;
const HISTORY_MAX: usize = 400;
/// 签到活动 scene / campaign_group_code（抓包固化）
const SCENE: &str = "pc_incentive_task_mvp_v3";
/// detail 端点（含稳定 query 参数；msToken/a_bogus 省略）
const DETAIL_URL: &str = "https://www.doubao.com/alice/ug/campaign/v3/detail/?version_code=20800&language=zh&device_platform=web&aid=497858&real_aid=497858&pc_version=2.33.5&samantha_web=1&web_platform=desktop&use-olympus-account=1";
/// 签到执行端点（同上）
const CHECKIN_URL: &str = "https://www.doubao.com/alice/ug/campaign/landing/checkin?version_code=20800&language=zh&device_platform=web&aid=497858&real_aid=497858&pc_version=2.33.5&samantha_web=1&web_platform=desktop&use-olympus-account=1";
/// 签到路径（抓包参数可用时拼客户端真实 query）
const CHECKIN_BASE: &str = "https://www.doubao.com/alice/ug/campaign/landing/checkin";
const REFERER: &str = "https://www.doubao.com/pc/agent-explore";
/// 抓包 UA：桌面客户端自带 SamanthaDoubao 标识（区别于纯浏览器请求）
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36 SamanthaDoubao/2.33.5";
/// 批量模式下账号间隔（防上游频控，与 wb/trae 的 gap 思路一致，固定短间隔）
const ACCOUNT_GAP_MS: u64 = 800;

/// 构造 Cookie（sessionid + sid_guard + 可选 msToken + 抓包设备级 Cookie 追加）
fn cookie_of(
    sid: &str,
    sid_guard: Option<&str>,
    ms_token: Option<&str>,
    device_cookies: &[(String, String)],
) -> String {
    let mut cookie = format!("sessionid={sid}");
    if let Some(sg) = sid_guard.filter(|s| !s.is_empty()) {
        cookie.push_str(&format!("; sid_guard={sg}"));
    }
    if let Some(mt) = ms_token.filter(|s| !s.is_empty()) {
        cookie.push_str(&format!("; msToken={mt}"));
    }
    // 设备级 Cookie 追加（抓包对齐 2026-11-06）：会话键不追加（本账号会话为准），
    // 设备键值对来自本账号 profile 的客户端抓包
    for (k, v) in device_cookies {
        if k.is_empty() || v.is_empty() {
            continue;
        }
        if matches!(k.as_str(), "sessionid" | "sid_guard" | "msToken") {
            continue;
        }
        cookie.push_str(&format!("; {k}={v}"));
    }
    cookie
}

/// 客户端抓包参数（代理 try_capture_doubao_campaign 存 vault ns "doubao"）：
/// - query 用客户端真实参数集（device_id/web_id/fp 等设备指纹——CLI 缺失被上游风控
///   拒绝 710010702）
/// - msToken Cookie + ua（客户端版本随更新漂移，旧版本号 UA 也是风控信号）
/// - device_cookies 设备级 Cookie（ttwid/odin_tt/tt_scid/s_v_web_id，非会话凭证）：
///   landing/checkin 为发奖端点、风控强校验设备标识，仅 3 个会话 Cookie 会被拒
/// 2026-11-06 起按 sessionid 分键存储（设备指纹与登录会话绑定——朱羽程的 device_id
/// 配朱天伟的 sessionid 一样被拒），全局键 campaign_probe 兜底。
/// 24h 内新鲜有效（msToken 会话级时效），过期/缺失回退内置稳定参数。
#[derive(Clone, Debug, Default)]
pub(crate) struct CampaignProbe {
    checkin_query: String,
    ms_token: Option<String>,
    ua: Option<String>,
    device_cookies: Vec<(String, String)>,
}

/// probe vault 键（sessionid 维度）
pub(crate) fn probe_key_for_sid(sid: &str) -> String {
    format!("campaign_probe:sid:{}", sid.trim())
}

/// 本账号自己的 probe 是否存在且新鲜（**不含全局回退**）：
/// 保活触发判定专用——全局 probe 可能属于其他账号（设备指纹不匹配），
/// 把它当「已新鲜」会让保活永久跳过（2026-11-06 12:1x 实测教训）
pub(crate) fn probe_own_fresh(data_dir: &std::path::Path, sid: &str) -> bool {
    crate::vault::ns_get(data_dir, "doubao", &probe_key_for_sid(sid))
        .and_then(|v| parse_probe(&v))
        .is_some()
}

/// 按优先级加载 probe：本账号 sessionid 维度 → 全局兜底；解析失败/过期视为未命中继续尝试
pub(crate) fn load_campaign_probe(data_dir: &std::path::Path, sid: Option<&str>) -> Option<CampaignProbe> {
    let mut keys: Vec<String> = Vec::new();
    if let Some(s) = sid.map(str::trim).filter(|s| !s.is_empty()) {
        keys.push(probe_key_for_sid(s));
    }
    keys.push("campaign_probe".to_string());
    for key in keys {
        if let Some(v) = crate::vault::ns_get(data_dir, "doubao", &key) {
            if let Some(p) = parse_probe(&v) {
                return Some(p);
            }
        }
    }
    None
}

/// 解析 vault probe（24h 新鲜期；设备 Cookie/UA 字段缺失时留空由调用方回退常量）
fn parse_probe(v: &Value) -> Option<CampaignProbe> {
    let captured_at = v.get("captured_at").and_then(Value::as_str).unwrap_or("");
    let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(captured_at, "%Y-%m-%d %H:%M:%S") else {
        return None;
    };
    let age = chrono::Local::now().naive_local() - ndt;
    if age.num_hours() > 24 {
        return None; // 设备参数虽稳定，msToken 会过期——超时整组弃用走回退
    }
    let params = v.get("params")?.as_object()?;
    let query = params
        .iter()
        .map(|(k, val)| {
            let val_s = val.as_str().unwrap_or_default();
            format!("{k}={}", urlencoding::encode(val_s))
        })
        .collect::<Vec<_>>()
        .join("&");
    if query.is_empty() {
        return None;
    }
    let ua = v
        .get("ua")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty() && s.contains("SamanthaDoubao"))
        .map(str::to_string);
    let device_cookies = v
        .get("device_cookies")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, val)| {
                    val.as_str()
                        .filter(|s| !s.is_empty())
                        .map(|s| (k.clone(), s.to_string()))
                })
                .collect::<Vec<(String, String)>>()
        })
        .unwrap_or_default();
    Some(CampaignProbe {
        checkin_query: query,
        ms_token: v
            .get("ms_token")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        ua,
        device_cookies,
    })
}

/// POST JSON 通用（agw 信封）：code==0 视为成功返回 body；否则 Err（含登录失效提示）。
/// ua/extra_cookies 来自抓包 probe（2026-11-06 对齐客户端请求形态），缺省回退内置常量
fn post_json(
    agent: &ureq::Agent,
    url: &str,
    sid: &str,
    sg: Option<&str>,
    ms: Option<&str>,
    body: &Value,
    ua: &str,
    extra_cookies: &[(String, String)],
) -> Result<Value, String> {
    let resp = agent
        .post(url)
        .set("Cookie", &cookie_of(sid, sg, ms, extra_cookies))
        .set("User-Agent", ua)
        .set("Referer", REFERER)
        .set("Origin", "https://www.doubao.com")
        .set("Accept", "application/json, text/plain, */*")
        .set("Content-Type", "application/json")
        .set("agw-js-conv", "str")
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .send_string(&body.to_string());
    let raw: Value = match resp {
        Ok(r) => {
            let text = r.into_string().unwrap_or_default();
            serde_json::from_str(&text).map_err(|e| format!("响应解析失败: {e}"))?
        }
        Err(ureq::Error::Status(code, _)) => return Err(format!("HTTP {code}")),
        Err(e) => return Err(format!("请求失败: {e}")),
    };
    if !raw.is_object() {
        return Err("接口返回异常 code=?".to_string());
    }
    let code_v = raw.get("code");
    let ok_code = match code_v {
        None | Some(Value::Null) => true,
        Some(Value::Number(n)) => n.as_f64() == Some(0.0),
        Some(_) => false,
    };
    if !ok_code {
        let code_str = code_v
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_else(|| "?".to_string());
        // 已知码提示矩阵（与 docs/tech-framework.md 错误码矩阵对齐）：
        // 710012001 = 登录态失效（凭证过期/不属于当前登录账号）
        // 712010702 = Cookie/设备指纹缺失或编码错误（重新保存登录态可解）
        // 710022004 = 风控验证码（客户端人工验证后恢复）
        let code_num = code_v.and_then(Value::as_i64);
        let hint = match code_num {
            Some(710012001) => "登录态已失效，请开启代理重新抓取凭证或重新保存该账号登录态",
            Some(712010702) => "Cookie/设备指纹缺失或编码错误，请重新保存该账号登录态",
            Some(710022004) => "触发风控验证码，请在豆包客户端完成一次人工验证后重试",
            _ => "",
        };
        // 未知码必须透出上游 message（定位关键；如 710010702 等活动侧业务码）
        let upstream = raw
            .get("message")
            .or_else(|| raw.get("msg"))
            .or_else(|| raw.get("data").and_then(|d| d.get("message")))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("");
        let mut msg = if hint.is_empty() {
            format!("接口返回异常 code={code_str}")
        } else {
            format!("接口返回异常 code={code_str}：{hint}")
        };
        if !upstream.is_empty() {
            msg.push_str(&format!("（上游提示：{upstream}）"));
        }
        return Err(msg);
    }
    Ok(raw)
}

/// 解析 detail 响应 → 签到状态 + 总额度（progress.total_amount）。
/// tasks[] 无 daily_signin 项时按未签处理（checked_in_today=false）。
pub fn parse_detail(body: &Value) -> Value {
    let data = body.get("data");
    let total_amount = data
        .and_then(|d| d.get("progress"))
        .and_then(|p| p.get("total_amount"))
        .and_then(Value::as_i64);
    let mut checked_in_today = false;
    let mut continuous_days: Option<i64> = None;
    let mut last_checkin_date: Option<String> = None;
    let mut reward_amount: Option<i64> = None;
    let mut has_signin_task = false;
    if let Some(tasks) = data.and_then(|d| d.get("tasks")).and_then(Value::as_array) {
        for t in tasks {
            if t.get("task_kind").and_then(Value::as_str) != Some("daily_signin") {
                continue;
            }
            has_signin_task = true;
            if let Some(dc) = t.get("daily_checkin").filter(|d| d.is_object()) {
                checked_in_today = dc
                    .get("checked_in_today")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                continuous_days = dc.get("continuous_days").and_then(Value::as_i64);
                last_checkin_date = dc
                    .get("last_checkin_date")
                    .and_then(Value::as_str)
                    .map(|s| s.to_string());
                reward_amount = dc
                    .get("reward")
                    .and_then(|r| r.get("reward_amount"))
                    .and_then(Value::as_i64);
            }
            break;
        }
    }
    json!({
        "checked_in_today": checked_in_today,
        "has_signin_task": has_signin_task,
        "continuous_days": continuous_days,
        "last_checkin_date": last_checkin_date,
        "reward_amount": reward_amount,
        "total_amount": total_amount,
    })
}

/// 单账号一轮签到（detail → 判定 → 未签则提交 checkin）。
/// probe = 客户端抓包参数（None 回退内置稳定参数）。
/// 返回 outcome：status ∈ success | already | fail。
pub fn run_one(
    agent: &ureq::Agent,
    uid: &str,
    sid: &str,
    sg: Option<&str>,
    name: &str,
    probe: Option<&CampaignProbe>,
) -> Value {
    let ms = probe.and_then(|p| p.ms_token.as_deref());
    // 抓包对齐（2026-11-06）：UA/设备 Cookie 取本账号 probe，缺失回退内置常量
    let ua_s = probe
        .and_then(|p| p.ua.as_deref())
        .filter(|s| !s.is_empty())
        .unwrap_or(UA);
    let device_cookies: &[(String, String)] = probe
        .map(|p| p.device_cookies.as_slice())
        .unwrap_or(&[]);
    // ① detail：签到状态 + 总额度
    let detail = match post_json(
        agent,
        DETAIL_URL,
        sid,
        sg,
        ms,
        &json!({"campaign_group_code": SCENE}),
        ua_s,
        device_cookies,
    ) {
        Ok(body) => parse_detail(&body),
        Err(e) => {
            return json!({
                "uid": uid, "name": name, "status": "fail", "reward": Value::Null,
                "continuous_days": Value::Null, "total_amount": Value::Null,
                "last_checkin_date": Value::Null, "message": format!("detail 查询：{e}"),
                "day": crate::doubao_checkin_results::today_key(),
            })
        }
    };
    let checked_in_today = detail
        .get("checked_in_today")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let total_amount = detail.get("total_amount").cloned().unwrap_or(Value::Null);

    // ② 已签 → already（回写 detail 携带的连签/上次日期即可）
    if checked_in_today {
        return json!({
            "uid": uid, "name": name, "status": "already", "reward": Value::Null,
            "continuous_days": detail.get("continuous_days").cloned().unwrap_or(Value::Null),
            "total_amount": total_amount,
            "last_checkin_date": detail.get("last_checkin_date").cloned().unwrap_or(Value::Null),
            "message": "今日已签到",
            "day": crate::doubao_checkin_results::today_key(),
        });
    }

    // ③ 未签 → 提交 landing/checkin（有抓包参数时用客户端真实 query，否则内置稳定参数）
    let checkin_url = match probe {
        Some(p) if !p.checkin_query.is_empty() => {
            format!("{CHECKIN_BASE}?{}", p.checkin_query)
        }
        _ => CHECKIN_URL.to_string(),
    };
    match post_json(
        agent,
        &checkin_url,
        sid,
        sg,
        ms,
        &json!({"scene": SCENE}),
        ua_s,
        device_cookies,
    ) {
        Ok(body) => {
            let data = body.get("data");
            let checked = data
                .and_then(|d| d.get("checked_in"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let idempotent = data
                .and_then(|d| d.get("idempotent"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let reward = data
                .and_then(|d| d.get("reward_amount"))
                .and_then(Value::as_i64);
            let continuous = data
                .and_then(|d| d.get("continuous_days"))
                .and_then(Value::as_i64);
            let date = data
                .and_then(|d| d.get("checkin_date"))
                .and_then(Value::as_str)
                .map(|s| s.to_string());
            if idempotent || (!checked && date.is_some()) {
                // 幂等命中：当日实际已签（detail 未反映或并发窗口）
                return json!({
                    "uid": uid, "name": name, "status": "already", "reward": Value::Null,
                    "continuous_days": continuous.map(Value::from)
                        .or_else(|| detail.get("continuous_days").cloned()),
                    "total_amount": total_amount,
                    "last_checkin_date": date.map(Value::from)
                        .or_else(|| detail.get("last_checkin_date").cloned()),
                    "message": "今日已签到（幂等）",
                    "day": crate::doubao_checkin_results::today_key(),
                });
            }
            if checked {
                return json!({
                    "uid": uid, "name": name, "status": "success",
                    "reward": reward.map(Value::from).unwrap_or(Value::Null),
                    "continuous_days": continuous.map(Value::from)
                        .or_else(|| detail.get("continuous_days").cloned()),
                    "total_amount": total_amount,
                    "last_checkin_date": date.map(Value::from)
                        .or_else(|| detail.get("last_checkin_date").cloned()),
                    "message": format!("签到成功 +{} 额度", reward.unwrap_or(0)),
                    "day": crate::doubao_checkin_results::today_key(),
                });
            }
            json!({
                "uid": uid, "name": name, "status": "fail", "reward": Value::Null,
                "continuous_days": detail.get("continuous_days").cloned().unwrap_or(Value::Null),
                "total_amount": total_amount,
                "last_checkin_date": detail.get("last_checkin_date").cloned().unwrap_or(Value::Null),
                "message": "签到未生效（checked_in=false）",
                "day": crate::doubao_checkin_results::today_key(),
            })
        }
        Err(e) => {
            // 业务码失败 → detail 复查兜底：部分活动侧业务码（如 710010702）实际对应
            // 「已签/活动未覆盖」等状态。复查 detail 今日已签 → 按 already 落库（避免误报失败）；
            // detail 正常且未签 → 会话健康、活动端点单独拒绝，大概率无签到资格（给出明确指引）
            if e.starts_with("接口返回异常 code=") {
                if let Ok(re_body) = post_json(
                    agent,
                    DETAIL_URL,
                    sid,
                    sg,
                    ms,
                    &json!({"campaign_group_code": SCENE}),
                    ua_s,
                    device_cookies,
                ) {
                    let re = parse_detail(&re_body);
                    if re.get("checked_in_today").and_then(Value::as_bool).unwrap_or(false) {
                        return json!({
                            "uid": uid, "name": name, "status": "already", "reward": Value::Null,
                            "continuous_days": re.get("continuous_days").cloned().unwrap_or(Value::Null),
                            "total_amount": re.get("total_amount").cloned().unwrap_or(Value::Null),
                            "last_checkin_date": re.get("last_checkin_date").cloned().unwrap_or(Value::Null),
                            "message": format!("{e}；复查 detail 今日已签 → 按已签处理"),
                            "day": crate::doubao_checkin_results::today_key(),
                        });
                    }
                    // detail 正常但提交被拒：按 detail 里有无 daily_signin 任务项分流定性
                    // ① 无任务项 → scene 不匹配/无资格；② 有任务项 → 请求形态被风控（需抓包对齐）
                    let has_task = re.get("has_signin_task").and_then(Value::as_bool).unwrap_or(false);
                    let hint = if has_task {
                        "detail 有 daily_signin 任务但提交被拒 → 请求形态疑似被上游风控拦截；\
                         请开启代理后在豆包客户端手动签到一次，日志会捕获客户端真实请求用于对齐修复"
                    } else {
                        "detail 无 daily_signin 任务项 → 该账号未加入当前签到活动（scene 不匹配或无资格）；\
                         若客户端能手动签到，同样请开代理抓一次真实请求"
                    };
                    return json!({
                        "uid": uid, "name": name, "status": "fail", "reward": Value::Null,
                        "continuous_days": re.get("continuous_days").cloned().unwrap_or(Value::Null),
                        "total_amount": re.get("total_amount").cloned().unwrap_or(Value::Null),
                        "last_checkin_date": re.get("last_checkin_date").cloned().unwrap_or(Value::Null),
                        "message": format!("{e}；{hint}"),
                        "day": crate::doubao_checkin_results::today_key(),
                    });
                }
            }
            json!({
                "uid": uid, "name": name, "status": "fail", "reward": Value::Null,
                "continuous_days": detail.get("continuous_days").cloned().unwrap_or(Value::Null),
                "total_amount": total_amount,
                "last_checkin_date": detail.get("last_checkin_date").cloned().unwrap_or(Value::Null),
                "message": format!("签到提交：{e}"),
                "day": crate::doubao_checkin_results::today_key(),
            })
        }
    }
}

/// 追加滚动运维历史（同 doubao_quota::append_history，kind="checkin"；失败落 app_log）
fn append_history(state: &AppState, event: Value) {
    let store = crate::store::db(&state.data_dir);
    let mut events = crate::store::docs::doubao_health_load(&store);
    events.push(event);
    if events.len() > HISTORY_MAX {
        events.drain(0..events.len() - HISTORY_MAX);
    }
    if let Err(e) = crate::store::docs::doubao_health_save(&store, &events) {
        crate::fs_utils::app_log(
            &state.data_dir,
            &format!("[doubao_checkin] 运维历史写入失败（本次签到事件可能丢失）: {e}"),
        );
    }
}

/// 池回写合并（纯函数，两阶段短临界区调用）：将签到结果按 uid 合并进最新池——
/// 只更新仍存在账号的 quota_total / checkin_continuous_days / checkin_last_date /
/// checkin_last_run_at 四字段。quota_total 来自 detail 的 progress.total_amount
/// （累计获得额度），detail 查询成功即有效——fail 也回写（2026-10-28 修复：活动端点
/// 拒签时额度面板看不到 1400 累计额度）；仅 fail 且 detail 未成功（None）时签到字段
/// 保留旧值。被删账号不复活（返回 0 即无需写盘）、新入池账号不受影响、并发方更新的
/// 其他字段不覆盖。
fn merge_checkin_updates_into_pool(
    latest: &mut Value,
    updates: &[(String, String, Option<i64>, Option<i64>, Option<String>, String)],
) -> usize {
    // (uid, status, quota_total, continuous_days, last_date, run_at)
    let mut merged = 0usize;
    if let Some(arr) = latest.get_mut("accounts").and_then(Value::as_array_mut) {
        for (uid, status, quota_total, continuous, last_date, run_at) in updates {
            let Some(acc) = arr
                .iter_mut()
                .find(|a| a.get("user_id").and_then(Value::as_str) == Some(uid.as_str()))
            else {
                continue; // 签到期间被删除的账号不复活
            };
            let Some(obj) = acc.as_object_mut() else { continue };
            obj.insert("checkin_last_run_at".into(), Value::String(run_at.clone()));
            if let Some(t) = quota_total {
                obj.insert("quota_total".into(), Value::Number((*t).into()));
            }
            if status == "fail" {
                // 语义（2026-10-28）：quota_total 来自 detail（查询成功即有效），
                // fail 且 detail 成功也回写总额度（上方已统一插入）——签到字段保留旧值
                merged += 1;
                continue;
            }
            if let Some(d) = continuous {
                obj.insert("checkin_continuous_days".into(), Value::Number((*d).into()));
            }
            if let Some(dt) = last_date {
                obj.insert("checkin_last_date".into(), Value::String(dt.clone()));
            }
            merged += 1;
        }
    }
    merged
}

/// 只读 detail 探针：查签到状态 + 累计获得额度（progress.total_amount），
/// 供额度巡检/数据同步任务回写 quota_total——**与签到提交解耦**（detail 是只读接口，
/// 会话有效即可查，签到是否成功无关）。Ok(parse_detail) / Err(_) → None。
pub fn detail_probe(agent: &ureq::Agent, sid: &str, sg: Option<&str>, ms: Option<&str>) -> Option<Value> {
    let body = post_json(
        agent,
        DETAIL_URL,
        sid,
        sg,
        ms,
        &json!({"campaign_group_code": SCENE}),
        UA,
        &[],
    )
    .ok()?;
    Some(parse_detail(&body))
}

/// 收集池内可签到目标（uid + 凭证 + name），避免边遍历边写
fn collect_targets(state: &AppState) -> Result<Vec<(String, String, Option<String>, String)>, String> {
    let pool: Value =
        serde_json::to_value(crate::commands::doubao::load_pool(state)).unwrap_or(json!({}));
    let accounts = pool
        .get("accounts")
        .and_then(Value::as_array)
        .ok_or("账号池解析失败")?;
    Ok(accounts
        .iter()
        .filter_map(|acc| {
            let uid = acc.get("user_id").and_then(Value::as_str)?.to_string();
            let sid = acc.get("session_id").and_then(Value::as_str)?.to_string();
            if uid.is_empty() || sid.is_empty() {
                return None;
            }
            let sg = acc
                .get("sid_guard")
                .and_then(Value::as_str)
                .map(|s| s.to_string());
            let name = acc
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Some((uid, sid, sg, name))
        })
        .collect())
}

/// 落库 + 池回写 + 运维历史（batch/single 共用收尾）。
/// skip_on_busy 语义见 record_day_at：task 批量传 true（调度器/CLI 双进程并发时
/// 他方必完成同批写入，幂等跳过无损失）；manual 单账号传 false——GUI 手动签到与
/// CLI 批次并发时两批内容不保证相同，Busy 跳过会把手动签到结果丢成 fail，
/// 降级进程内锁继续写（池回写进程内锁保护，不受影响）。
/// 返回是否实际落库（审查 M-3）：false 仅在 task 批量 skip_on_busy=true 且跨进程
/// 落库锁 Busy 幂等跳过时出现——调用方 run_batch 据此打 skipped_busy 交调度器
/// 不 mark_run、下一轮重跑补录（已签账号幂等归 already），防结果丢失且无法补录。
fn persist_outcomes(state: &AppState, outcomes: &[Value], source: &str, skip_on_busy: bool) -> bool {
    // ① 按日落库（success/already/fail 全记；reward 仅 success 携带）。
    // day 取 outcome 产生时刻打点（跨零点批次各归各日）；旧 outcome 无 day 回退当日。
    // message 仅 fail 携带（失败原因落库，切页/重开后日历明细仍可排障）——
    // success/already 的 message 恒为固定成功文案，落库只增体积无价值
    let today = crate::doubao_checkin_results::today_key();
    let entries = outcomes.iter().map(|o| {
        (
            o.get("uid").and_then(Value::as_str).unwrap_or("").to_string(),
            o.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
            o.get("status").and_then(Value::as_str).unwrap_or("fail").to_string(),
            o.get("reward").and_then(Value::as_i64),
            (o.get("status").and_then(Value::as_str) == Some("fail"))
                .then(|| {
                    o.get("message")
                        .and_then(Value::as_str)
                        .filter(|m| !m.is_empty())
                        .map(String::from)
                })
                .flatten(),
            o.get("day").and_then(Value::as_str).unwrap_or(&today).to_string(),
        )
    });
    // skip_on_busy 由调用方按来源决定（task 批量=true / manual 单账号=false，见函数注释）
    let persisted = crate::doubao_checkin_results::record_day_at(&state.data_dir, entries, skip_on_busy);

    // ② 池回写（两阶段短临界区：重读最新池按 uid 合并）
    let run_at = fs_utils::now_ts();
    let updates: Vec<(String, String, Option<i64>, Option<i64>, Option<String>, String)> = outcomes
        .iter()
        .map(|o| {
            (
                o.get("uid").and_then(Value::as_str).unwrap_or("").to_string(),
                o.get("status").and_then(Value::as_str).unwrap_or("fail").to_string(),
                o.get("total_amount").and_then(Value::as_i64),
                o.get("continuous_days").and_then(Value::as_i64),
                o.get("last_checkin_date")
                    .and_then(Value::as_str)
                    .map(|s| s.to_string()),
                run_at.clone(),
            )
        })
        .collect();
    if !updates.is_empty() {
        let _guard = state.doubao_pool_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut latest: Value =
            serde_json::to_value(crate::commands::doubao::load_pool(state)).unwrap_or(json!({}));
        let merged = merge_checkin_updates_into_pool(&mut latest, &updates);
        if merged > 0 {
            // 回写失败不吞（收尾尽力而为，错误不上抛、不改变签名语义）：解析失败说明
            // 池结构异常、保存失败说明落库故障，均落 app_log 便于排查（静默丢弃难定位）
            match serde_json::from_value::<crate::commands::doubao::DoubaoAccountPool>(latest) {
                Ok(file) => {
                    if let Err(e) = crate::commands::doubao::save_pool(state, &file) {
                        fs_utils::app_log(
                            &state.data_dir,
                            &format!("[doubao-checkin] 池回写保存失败: {e}"),
                        );
                    }
                }
                Err(e) => fs_utils::app_log(
                    &state.data_dir,
                    &format!("[doubao-checkin] 池回写解析失败（本次签到字段回写丢弃）: {e}"),
                ),
            }
        }
    }

    // ③ 运维历史（逐账号，kind="checkin"）
    for o in outcomes {
        let ok = o.get("status").and_then(Value::as_str) != Some("fail");
        append_history(
            state,
            json!({
                "ts": fs_utils::now_ts(), "kind": "checkin", "uid": o.get("uid"), "ok": ok,
                "status": o.get("status"), "reward": o.get("reward"),
                "summary": o.get("message"), "source": source,
            }),
        );
    }
    persisted
}

/// 批量签到（调度器/CLI `--task-run doubao-checkin`）：遍历池内有凭证账号 → 签到 →
/// 按日落库 + 池回写 + 运维历史 → 汇总。空池打 skipped_empty_pool（奖励型任务豁免，
/// 见 scheduler tick 对该键不 mark_run 的注释）——签到漏跑=丢收益，mark_run 固化
/// 「当日已跑」会让当日导入的账号失去调度补打。
/// 跨进程轮次锁（对齐 wb-checkin try_acquire_wb_round 先例）：GUI 调度器与
/// schtasks CLI 双进程互斥；Busy 幂等跳过、机制性失败 fail-closed。
pub fn run_batch(state: &AppState, app: Option<&tauri::AppHandle>) -> Result<Value, String> {
    use crate::tasks::qoder_common::CrossProcLockFail;
    let _cross = match crate::tasks::qoder_common::CrossProcLock::try_acquire_ns(
        &state.data_dir,
        "doubao",
        "checkin-round",
        0,
    ) {
        (Some(g), _) => g,
        (None, fail) => {
            let fail = fail.unwrap_or(CrossProcLockFail::Busy);
            if fail != CrossProcLockFail::Busy {
                // 仅机制性失败（创建/等待系统错误）落日志；Busy 是正常互斥不刷屏
                fs_utils::app_log(
                    &state.data_dir,
                    &format!("豆包签到轮次锁获取失败：{}", fail.describe()),
                );
            }
            return Ok(json!({ "ok": true, "skipped": "其他进程正在执行签到，本轮跳过", "reason": fail.describe() }));
        }
    };
    let targets = collect_targets(state)?;
    // probe 自动保活（方案 A，2026-11-06）：probe 缺失/过期 → 自动开客户端抓包对齐；
    // 失败仅记日志，签到照常走既有 probe 链（缺失回退内置稳定参数——该形态大概率被
    // 上游风控拒）。2026-11-07 可观测性：错误透传到返回值/汇总 Err，前端批量签到
    // 失败时用户能直接看到「抓包参数未对齐」这一根因，不再只看到笼统失败
    let probe_keepalive_error: Option<String> =
        match super::doubao_probe::ensure_fresh_probes(state, app, &targets) {
            Ok(()) => None,
            Err(e) => {
                fs_utils::app_log(&state.data_dir, &format!("[probe-keepalive] 保活未完成: {e}"));
                Some(e)
            }
        };
    if targets.is_empty() {
        // 奖励型任务（漏跑=丢收益）空池豁免：调度器 tick 对 skipped_empty_pool 不
        // mark_run，当日导入账号后下一 tick 正常补打（对齐 buddy-ai #15 豁免口径）
        return Ok(json!({ "ok": true, "skipped": "无豆包账号", "skipped_empty_pool": true }));
    }

    let agent = http_agent(TIMEOUT_SECS);
    let mut outcomes: Vec<Value> = Vec::new();
    for (i, (uid, sid, sg, name)) in targets.iter().enumerate() {
        if i > 0 {
            std::thread::sleep(Duration::from_millis(ACCOUNT_GAP_MS));
        }
        // 回退语义由 load_campaign_probe 内部保证（本账号键 → 全局键两级回退）；
        // 外层再 or_else 全局快照属死分支，且命中时取的是循环前更旧快照
        let probe = load_campaign_probe(&state.data_dir, Some(sid));
        outcomes.push(run_one(&agent, uid, sid, sg.as_deref(), name, probe.as_ref()));
    }
    // 审查 M-3：落库锁 Busy 幂等跳过（结果未落库）时打 skipped_busy 交调度器不
    // mark_run、下一 tick 重跑补录（已签账号幂等归 already，无重复奖励）——否则
    // 返 Ok 会被 mark_run 固化「当日已跑」关闭当日重试链，丢失的结果无法补录。
    // skipped_busy 字段条件插入（tick 按字段存在性分流）：仅确因 Busy 未落库才带
    // 字段，正常落库不得恒置 false（否则会误入 busy 分支不记当日已跑）。
    // GUI 手动触发（调度开关关闭时）无自动补录，文案不承诺自动重试（审查 新-L1），
    // 如实提示稍后重试签到即可补录
    if !persist_outcomes(state, &outcomes, "task", true) {
        return Ok(json!({
            "ok": true,
            "skipped": "本轮签到结果未记录（落库锁被其他进程持有）；稍后重试签到即可补录（重复签到只回「已领」，无重复奖励）",
            "skipped_busy": true,
        }));
    }

    let success = outcomes
        .iter()
        .filter(|o| o.get("status").and_then(Value::as_str) == Some("success"))
        .count();
    let already = outcomes
        .iter()
        .filter(|o| o.get("status").and_then(Value::as_str) == Some("already"))
        .count();
    let failed = outcomes.len() - success - already;
    let reward_total: i64 = outcomes
        .iter()
        .filter_map(|o| o.get("reward").and_then(Value::as_i64))
        .sum();
    let errors: Vec<Value> = outcomes
        .iter()
        .filter(|o| o.get("status").and_then(Value::as_str) == Some("fail"))
        .map(|o| {
            json!({
                "user_id": o.get("uid"),
                "name": o.get("name"),
                "error": o.get("message"),
            })
        })
        .collect();

    // 存在失败账号时返 Err（对齐 qoder-checkin / buddy-ai-checkin 分发处惯例）：
    // CLI `--task-run` 退出码 1 可见 + 调度器走 mark_fail 当日冷却重试（返 Ok 会
    // mark_run 固化「当日已跑」关闭重试链）；落库/池回写/运维历史已在 persist_outcomes
    // 完成，Err 仅改变汇总出口。GUI「立即签到全部」经此路径收到 Err 时 toast 展示
    // 失败汇总，无数据副作用
    if failed > 0 {
        // 审查 L-1：汇总串逐条钳制（单行化 + 120 字截断）——超长多行错误文案会污染
        // CLI `--task-run` 退出输出与失败通知；errors 数组保原始完整供前端展示
        let clamp_msg = |m: &str| -> String {
            let one_line = m.replace(['\n', '\r'], " ");
            let mut clamped: String = one_line.chars().take(120).collect();
            if one_line.chars().count() > 120 {
                clamped.push('…');
            }
            clamped
        };
        let err_head = errors
            .iter()
            .take(3)
            .filter_map(|e| {
                e.get("error").and_then(Value::as_str).map(|m| {
                    format!(
                        "{}：{}",
                        e.get("name").and_then(Value::as_str).unwrap_or("未知账号"),
                        clamp_msg(m)
                    )
                })
            })
            .collect::<Vec<_>>()
            .join("；");
        let probe_hint = probe_keepalive_error
            .as_deref()
            .map(|e| format!("；此外抓包参数自动保活未完成（{e}），本次签到未对齐客户端指纹，易被风控拒绝——请开启代理并保持豆包客户端打开后重试"))
            .unwrap_or_default();
        return Err(format!(
            "豆包签到：{success} 成功 / {already} 已领 / {failed} 失败：{err_head}{probe_hint}"
        ));
    }

    Ok(json!({
        "ok": true, "mode": "all",
        "total": outcomes.len(), "success": success, "already": already, "failed": failed,
        "reward_total": reward_total,
        "errors": errors,
        "probe_keepalive_error": probe_keepalive_error,
        "finished_at": fs_utils::now_ts(),
    }))
}

/// 单账号签到（应用内手动路径）：读池凭证 → probe 保活 → run_one → 落库/回写/历史
pub fn checkin_single(state: &AppState, app: Option<&tauri::AppHandle>, uid: &str) -> Result<Value, String> {
    let targets = collect_targets(state)?;
    // 首查绑定目标引用复用，消除同谓词二次 find(...).unwrap()（let-else 已保证存在）
    let Some(target) = targets.iter().find(|(u, _, _, _)| u == uid) else {
        return Err(format!("账号 {uid} 不在账号池中或未录入 sessionid 凭证"));
    };
    let (_, sid, sg, name) = target;
    // probe 自动保活（方案 A）：本账号 probe 缺失/过期 → 自动开客户端抓包（失败照常签到，
    // 错误透传 probe_keepalive_error 供前端提示根因）
    let probe_keepalive_error =
        match super::doubao_probe::ensure_fresh_probes(state, app, std::slice::from_ref(target)) {
            Ok(()) => None,
            Err(e) => {
                fs_utils::app_log(&state.data_dir, &format!("[probe-keepalive] 保活未完成: {e}"));
                Some(e)
            }
        };
    let agent = http_agent(TIMEOUT_SECS);
    // 回退语义由 load_campaign_probe 内部保证（本账号键 → 全局键两级回退，与
    // run_batch 同口径）；外层再 or_else 全局查询属死分支（内部已试过全局键）
    let probe = load_campaign_probe(&state.data_dir, Some(sid));
    let outcome = run_one(&agent, uid, sid, sg.as_deref(), name, probe.as_ref());
    persist_outcomes(state, std::slice::from_ref(&outcome), "manual", false);
    let ok = outcome.get("status").and_then(Value::as_str) != Some("fail");
    Ok(json!({
        "ok": ok,
        "outcome": outcome,
        "probe_keepalive_error": probe_keepalive_error,
        "finished_at": fs_utils::now_ts(),
    }))
}

#[cfg(test)]
mod doubao_checkin_tests {
    use super::*;

    // ── probe 按 sessionid 分键（2026-11-06 设备-账号绑定对齐）──

    fn probe_json(hours_ago: i64, ms_token: &str) -> Value {
        let ts = (chrono::Local::now().naive_local() - chrono::Duration::hours(hours_ago))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        json!({
            "captured_at": ts,
            "params": {"device_id": "7288888888888888", "aid": "497858", "msToken": ms_token},
            "ms_token": ms_token,
            "ua": "Mozilla/5.0 Test SamanthaDoubao/2.35.4",
            "device_cookies": {"ttwid": "tt1", "odin_tt": "od1", "tt_scid": "sc1"},
        })
    }

    fn temp_probe_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("aiwork_checkin_probe_{tag}_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        d
    }

    #[test]
    fn probe_本账号sessionid维度优先_全局兜底() {
        let d = temp_probe_dir("prio");
        crate::vault::ns_set(&d, "doubao", "campaign_probe", &probe_json(1, "global_ms")).unwrap();
        // 全局新鲜、本账号键缺失 → 回退全局
        let p = load_campaign_probe(&d, Some("sid-A")).expect("应回退到全局");
        assert_eq!(p.ms_token.as_deref(), Some("global_ms"));
        // 本账号键存在且新鲜 → 优先本账号
        crate::vault::ns_set(&d, "doubao", &probe_key_for_sid("sid-A"), &probe_json(0, "own_ms")).unwrap();
        let p = load_campaign_probe(&d, Some("sid-A")).expect("应取本账号");
        assert_eq!(p.ms_token.as_deref(), Some("own_ms"));
        assert_eq!(p.device_cookies.iter().find(|(k, _)| k == "ttwid").map(|(_, v)| v.as_str()), Some("tt1"));
        assert!(p.ua.as_deref().unwrap_or("").contains("2.35.4"));
        // 其他账号不受影响 → 仍回退全局
        let p = load_campaign_probe(&d, Some("sid-B")).expect("B 应回退全局");
        assert_eq!(p.ms_token.as_deref(), Some("global_ms"));
    }

    #[test]
    fn probe_本账号过期_全局新鲜时回退全局() {
        let d = temp_probe_dir("stale");
        crate::vault::ns_set(&d, "doubao", &probe_key_for_sid("sid-X"), &probe_json(30, "stale_ms")).unwrap();
        crate::vault::ns_set(&d, "doubao", "campaign_probe", &probe_json(1, "fresh_ms")).unwrap();
        let p = load_campaign_probe(&d, Some("sid-X")).expect("过期键应跳过取全局");
        assert_eq!(p.ms_token.as_deref(), Some("fresh_ms"));
    }

    #[test]
    fn probe_全部过期缺失_返回none() {
        let d = temp_probe_dir("none");
        assert!(load_campaign_probe(&d, Some("sid-C")).is_none());
        assert!(load_campaign_probe(&d, None).is_none());
        crate::vault::ns_set(&d, "doubao", &probe_key_for_sid("sid-C"), &probe_json(25, "old")).unwrap();
        crate::vault::ns_set(&d, "doubao", "campaign_probe", &probe_json(25, "old")).unwrap();
        assert!(load_campaign_probe(&d, Some("sid-C")).is_none());
    }

    #[test]
    fn cookie_of_追加设备cookie_会话键不覆盖() {
        let dc = vec![
            ("ttwid".to_string(), "tt1".to_string()),
            ("msToken".to_string(), "should_skip".to_string()),
            ("sessionid".to_string(), "should_skip".to_string()),
            (String::new(), "empty_key".to_string()),
        ];
        let c = cookie_of("sid-1", Some("sg-1"), Some("ms-1"), &dc);
        assert!(c.starts_with("sessionid=sid-1"));
        assert!(c.contains("sid_guard=sg-1"));
        assert!(c.contains("msToken=ms-1"));
        assert!(c.contains("ttwid=tt1"));
        assert!(!c.contains("should_skip"));
        assert!(!c.contains("empty_key"));
    }

    #[test]
    fn parse_detail_抓包实测结构() {
        // 2026-10-27 抓包 detail 响应关键段
        let body = json!({
            "code": 0,
            "data": {
                "progress": {"total_amount": 1500},
                "tasks": [
                    {
                        "campaign_code": "daily_signin",
                        "task_kind": "daily_signin",
                        "daily_checkin": {
                            "checked_in_today": false,
                            "continuous_days": 5,
                            "last_checkin_date": "2026-10-26",
                            "reward": {"reward_amount": 200, "reward_type": "quota_grant"},
                            "total_days": 5
                        }
                    },
                    {"campaign_code": "browser_tool", "task_kind": "chat_activity"}
                ]
            }
        });
        let p = parse_detail(&body);
        assert_eq!(p["checked_in_today"], false);
        assert_eq!(p["continuous_days"], 5);
        assert_eq!(p["last_checkin_date"], "2026-10-26");
        assert_eq!(p["reward_amount"], 200);
        assert_eq!(p["total_amount"], 1500);
    }

    #[test]
    fn parse_detail_无签到任务按未签处理() {
        let body = json!({"code": 0, "data": {"progress": {"total_amount": 0}, "tasks": []}});
        let p = parse_detail(&body);
        assert_eq!(p["checked_in_today"], false);
        assert_eq!(p["continuous_days"], Value::Null);
        assert_eq!(p["total_amount"], 0);
    }

    /// 回归（两阶段回写）：签到合并只动签到相关字段——窗口内新入池账号保留、
    /// 被删账号不复活、并发方更新的其他字段不被覆盖、fail 仅回写运行时间。
    #[test]
    fn merge_checkin_updates_preserves_pool_changes_during_window() {
        let mut latest = json!({"accounts": [
            {"user_id": "u1", "name": "甲", "session_id": "sid-A", "quota_summary": "旧摘要"},
            {"user_id": "u3", "name": "新入池", "session_id": "sid-C"},
        ]});
        let updates = vec![
            (
                "u1".to_string(), "success".to_string(),
                Some(1700), Some(6), Some("2026-10-27".to_string()),
                "2026-10-27T09:15:00".to_string(),
            ),
            // u2：签到期间被删除 → 不得复活
            (
                "u2".to_string(), "already".to_string(),
                Some(100), Some(1), Some("2026-10-27".to_string()),
                "2026-10-27T09:15:00".to_string(),
            ),
        ];
        let merged = merge_checkin_updates_into_pool(&mut latest, &updates);
        assert_eq!(merged, 1, "被删账号不参与合并");
        let accounts = latest["accounts"].as_array().unwrap();
        assert_eq!(accounts.len(), 2, "被删账号不复活、无新账号混入");
        let a1 = accounts.iter().find(|a| a["user_id"] == "u1").unwrap();
        assert_eq!(a1["quota_total"], 1700);
        assert_eq!(a1["checkin_continuous_days"], 6);
        assert_eq!(a1["checkin_last_date"], "2026-10-27");
        assert_eq!(a1["checkin_last_run_at"], "2026-10-27T09:15:00");
        assert_eq!(a1["quota_summary"], "旧摘要", "并发方更新的其他字段不被覆盖");
    }

    #[test]
    fn merge_checkin_updates_fail_仅回写运行时间() {
        let mut latest = json!({"accounts": [{"user_id": "u1"}]});
        // 语义更新（2026-10-28）：quota_total 来自 detail（查询成功即有效），
        // fail 也回写总额度——否则活动端点拒签时额度面板永远看不到累计额度
        let updates = vec![(
            "u1".to_string(),
            "fail".to_string(),
            Some(1700),
            None,
            None,
            "2026-10-27T09:15:00".to_string(),
        )];
        let merged = merge_checkin_updates_into_pool(&mut latest, &updates);
        assert_eq!(merged, 1);
        let a1 = latest["accounts"].as_array().unwrap().first().unwrap();
        assert_eq!(a1["checkin_last_run_at"], "2026-10-27T09:15:00");
        assert_eq!(a1["quota_total"], 1700, "fail 且 detail 成功 → 回写总额度");
        assert_eq!(a1.get("checkin_last_date"), None, "失败不写签到日期");
        // fail 且 detail 未成功（quota_total=None）→ 不写，保留旧值
        let mut latest2 = json!({"accounts": [{"user_id": "u2", "quota_total": 900}]});
        let updates2 = vec![(
            "u2".to_string(),
            "fail".to_string(),
            None,
            None,
            None,
            "2026-10-27T09:15:00".to_string(),
        )];
        let _ = merge_checkin_updates_into_pool(&mut latest2, &updates2);
        let a2 = latest2["accounts"].as_array().unwrap().first().unwrap();
        assert_eq!(a2["quota_total"], 900, "fail 且 detail 未成功 → 保留旧总额度");
    }
}
