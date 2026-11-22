//! 豆包每日签到（需求③）：launch 读活动组 → detail 查签到状态 → 未签账号提交
//! item/reward/claim 领取 → 按日落库 doubao_checkin_results + 账号池回写
//! （quota_total/签到缓存）+ 运维历史。
//! 端点（2026-11-17 magpie-doubao-work-auth 交叉验证修正）：
//! - POST /alice/user/launch                     body {} → data.ug_campaign_info.campaign_group_code
//!   （账号实际投放的活动组；为空 = 未投放签到活动，任何提交必然失败）
//! - POST /alice/ug/campaign/v3/detail/          body {"campaign_group_code": group}
//!   → data.progress.total_amount（累计获得额度 = 「总额度」口径）、
//!     data.tasks[] task_kind=="daily_signin" 的 daily_checkin（今日已签/连续天数/上次日期）
//!     与 task_status（**2026-11-21 活动改版双口径**：v3 新口径 1=未签到可签——客户端
//!     实测未签态 daily_signin task_status=1、checked_in_today=false 后直签成功；旧
//!     magpie 口径 2=可领取。按值分派提交端点，见 submit_endpoint_for）
//! - POST /alice/ug/campaign/landing/checkin     body {"scene": group}（v3 活动客户端
//!   真实提交端点，2026-11-21 抓包实锤：客户端点签到按钮即发此请求 → data.checked_in/
//!   idempotent/reward_amount，直签成功不回「确认中」）
//! - POST /alice/ug/campaign/item/reward/claim/  body {"task_id":"","campaign_group_code":group,
//!   "campaign_item_code":"daily_signin"}（旧口径 task_status=2 兼容路径）
//!   历史（magpie 期）「landing/checkin 直调一律 710010702」结论是当时端点选错所致；
//!   v3 下 landing/checkin 即正确端点，直调若仍被拒按 fail + detail 复查兜底处理。
//!   字段名红线 campaign_item_code（写成 campaign_code 得 710010202 common invalid param）。
//!   旧口径下领取被服务端限死在豆包电脑版客户端（HTTP 直调回 biz_error.code=3「确认中」）。
//! msToken/a_bogus 为一次性反爬 token（CLI 无法生成），实测仅稳定 query 参数 + Cookie 即可通过。
//! 红线：凭证等同密码，仅本地使用；仅执行官方签到动作，不做代刷、不做失败重试刷接口
//! （高频无效写请求正是风控最敏感的模式，magpie 同款保守原则）。

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::fs_utils;
use crate::state::AppState;

use super::http_agent;

const TIMEOUT_SECS: u64 = 15;
const HISTORY_MAX: usize = 400;
/// 签到活动 scene / campaign_group_code 兜底值（实际以 launch 返回为准，修复⑤）
const SCENE: &str = "pc_incentive_task_mvp_v3";
/// detail 端点（含稳定 query 参数；msToken/a_bogus 省略）
const DETAIL_URL: &str = "https://www.doubao.com/alice/ug/campaign/v3/detail/?version_code=20800&language=zh&device_platform=web&aid=497858&real_aid=497858&pc_version=2.33.5&samantha_web=1&web_platform=desktop&use-olympus-account=1";
/// 领取端点（2026-11-17 修复①：landing/checkin → item/reward/claim，magpie 实测定论）
const CLAIM_URL: &str = "https://www.doubao.com/alice/ug/campaign/item/reward/claim/?version_code=20800&language=zh&device_platform=web&aid=497858&real_aid=497858&pc_version=2.33.5&samantha_web=1&web_platform=desktop&use-olympus-account=1";
/// 领取路径（抓包参数可用时拼客户端真实 query）
const CLAIM_BASE: &str = "https://www.doubao.com/alice/ug/campaign/item/reward/claim/";
/// v3 活动提交端点（2026-11-21 抓包实锤：客户端在 pc_incentive_task_mvp_v3 下点签到
/// 按钮发 POST landing/checkin，body 仅 {"scene":"pc_incentive_task_mvp_v3"}，直签成功）
const LANDING_CHECKIN_URL: &str = "https://www.doubao.com/alice/ug/campaign/landing/checkin?version_code=20800&language=zh&device_platform=web&aid=497858&real_aid=497858&pc_version=2.33.5&samantha_web=1&web_platform=desktop&use-olympus-account=1";
/// v3 提交路径（抓包参数可用时拼客户端真实 query）
const LANDING_CHECKIN_BASE: &str = "https://www.doubao.com/alice/ug/campaign/landing/checkin";
/// 活动组读取端点（修复⑤：GET 语义但走 POST 空体，magpie session.json('/alice/user/launch',{}) 同形）
const LAUNCH_URL: &str = "https://www.doubao.com/alice/user/launch?version_code=20800&language=zh&device_platform=web&aid=497858&real_aid=497858&pc_version=2.33.5&samantha_web=1&web_platform=desktop&use-olympus-account=1";
const REFERER: &str = "https://www.doubao.com/pc/agent-explore";
/// 抓包 UA：桌面客户端自带 SamanthaDoubao 标识（区别于纯浏览器请求）
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36 SamanthaDoubao/2.33.5";
/// 批量模式下账号间隔基准（修复④：800ms → 2s + 0~2s 随机抖动，防上游频控/风控画像）。
/// 审查 #5：nanos % 2000 非密码学随机（零依赖取舍，本仓无 rand crate），但实际间隔
/// = 2s 基准 + 上一账号 run_one 网络耗时（天然波动大）+ 抖动，三重叠加后节拍画像
/// 难度已显著高于原固定间隔；更显著的画像信号（固定下限/串行模式）为批量语义固有，
/// 不在此处理
const ACCOUNT_GAP_MS: u64 = 2000;
/// 修复④（重试策略收敛，设计文档 §0.3）：每账号每日提交上限 2 次（首发 + 非确定性
/// 失败重试后的一次补提兜底），超限只读不发 claim 写请求——高频无效写请求正是风控
/// 最敏感的模式
const DAILY_SUBMIT_LIMIT: i64 = 2;
/// 修复②：pending「确认中」10 分钟后 detail-only 复查延迟（设计文档 §0.3 修复②口径）
const PENDING_RECHECK_DELAY_SECS: u64 = 600;

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
    // 修复⑥：task_status（magpie 口径 2=可领取，1=未满足条件/未投放）——提交前门槛判定用
    let mut task_status: Option<i64> = None;
    if let Some(tasks) = data.and_then(|d| d.get("tasks")).and_then(Value::as_array) {
        for t in tasks {
            if t.get("task_kind").and_then(Value::as_str) != Some("daily_signin") {
                continue;
            }
            has_signin_task = true;
            task_status = t.get("task_status").and_then(Value::as_i64);
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
        "task_status": task_status,
        "continuous_days": continuous_days,
        "last_checkin_date": last_checkin_date,
        "reward_amount": reward_amount,
        "total_amount": total_amount,
    })
}

/// 审查修复（2026-11-18 #2）：launch 投放组按 sid 短 TTL 缓存——campaign_group_code
/// 随账号基本不变，而签到与 quota 巡检每账号每轮各发一次 launch。仅缓存「成功且非空」
/// 结果：显式空串（未投放判定）/字段缺失/网络失败一律不缓存，保留实时判定的
/// skip 语义与回退 SCENE 语义。
static CAMPAIGN_GROUP_CACHE: LazyLock<Mutex<HashMap<String, (String, Instant)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
const CAMPAIGN_GROUP_TTL: Duration = Duration::from_secs(6 * 3600);

fn campaign_group_cached(sid: &str) -> Option<String> {
    CAMPAIGN_GROUP_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(sid)
        .filter(|(_, at)| at.elapsed() < CAMPAIGN_GROUP_TTL)
        .map(|(g, _)| g.clone())
}

fn campaign_group_store(sid: &str, group: &str) {
    CAMPAIGN_GROUP_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(sid.to_string(), (group.to_string(), Instant::now()));
}

/// 修复⑤：launch 现取账号实际投放的活动组（magpie campaignGroup 同形）。
/// 返回 Ok(Some(空串)) = 服务端**显式**返回空 campaign_group_code = 未投放签到活动
/// （此时任何 detail/claim 都注定失败，调用方直接短路）；
/// Ok(None) = data.ug_campaign_info / campaign_group_code **字段缺失**（响应形态变化/
/// 接口升级信号）——语义与「显式空串」不同，调用方不得据此判未投放，应回退 SCENE
/// 继续 detail 二次判定（审查 #1：unwrap_or_default 把两者折叠成空串会让形态变化的
/// 账号被统一误判「未投放」）；Err → 网络层/信封层失败，同样回退 SCENE。
fn campaign_group_from_launch(
    agent: &ureq::Agent,
    sid: &str,
    sg: Option<&str>,
    ms: Option<&str>,
    ua: &str,
    device_cookies: &[(String, String)],
) -> Result<Option<String>, String> {
    let body = post_json(agent, LAUNCH_URL, sid, sg, ms, &json!({}), ua, device_cookies)?;
    let Some(info) = body
        .get("data")
        .and_then(|d| d.get("ug_campaign_info"))
        .and_then(|c| c.as_object())
    else {
        return Ok(None); // 字段缺失 ≠ 未投放：回退 SCENE 走 detail 二次判定
    };
    // 字段缺失或存在但非字符串（null/数字——unwrap_or_default 会把 null 折叠成空串，
    // 进而被误判「未投放」短路）→ 同口径回退 SCENE 走 detail 二次判定；
    // 显式空串（含纯空白）仍返回 Some("") 判「未投放」（magpie 口径不变）
    let Some(code) = info
        .get("campaign_group_code")
        .and_then(Value::as_str)
        .map(str::trim)
    else {
        return Ok(None);
    };
    Ok(Some(code.to_string()))
}

/// 修复②：claim 回执业务层解析。信封 code==0 ≠ 领取成功——「确认中」形态
/// {"data":{"biz_error":{"code":3,"message":"领取结果确认中，请稍后重试"},"task_status":0},"code":0}
/// 返回 (is_success, is_already, is_pending, message)：
/// - is_success：checked_in=true（本次实签）
/// - is_already：idempotent=true（幂等命中——此前已领过，审查 #2：归 already 不归
///   success，与 handler.rs 落库链路 `if checked {success} else {already}` 口径一致，
///   否则 CLI 统计虚增成功数且「签到成功 +0 额度」文案误导）
/// - is_pending：biz_error.code=3「领取结果确认中」——服务端异步延迟确认，当日不再
///   重试提交（审查 #3：立即复查 detail 大概率未翻转，砍掉；明示客户端路径兜底）
fn parse_claim_outcome(body: &Value) -> (bool, bool, bool, String) {
    let data = body.get("data");
    // 本次实签：checked_in=true
    if data
        .and_then(|d| d.get("checked_in"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return (true, false, false, String::new());
    }
    // 幂等命中：此前已领过 → already（口径与落库链路一致）
    if data
        .and_then(|d| d.get("idempotent"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return (false, true, false, String::new());
    }
    // 旧兜底恢复（防回执形态变化误判 fail 触发重提）：checkin_date 有值且
    // checked_in=false → 当日实际已签（detail 未反映或并发窗口），归 already
    //（口径对齐历史逻辑 `idempotent || (!checked && date.is_some())` 的后半支）
    if data
        .and_then(|d| d.get("checkin_date"))
        .and_then(Value::as_str)
        .is_some()
    {
        return (false, true, false, String::new());
    }
    // 业务层拒绝：biz_error.code（3=确认中/其余=明确拒绝）
    let biz = data.and_then(|d| d.get("biz_error"));
    if let Some(b) = biz.filter(|b| !b.is_null()) {
        let bcode = b.get("code").and_then(Value::as_i64).unwrap_or(-1);
        let bmsg = b
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if bcode == 3 {
            return (false, false, true, if bmsg.is_empty() { "领取结果确认中，请稍后复查".into() } else { bmsg });
        }
        return (false, false, false, format!("biz_error code={bcode}：{bmsg}"));
    }
    // 无 checked_in 也无 biz_error → 未知形态，按失败处理
    (false, false, false, "签到未生效（checked_in=false 且无 biz_error）".into())
}

/// 2026-11-21 活动改版：task_status → 提交端点分派。
/// 返回 Some(true)=走 landing/checkin（v3 新口径 1=未签到可签，客户端实测形态）；
/// Some(false)=走旧 item/reward/claim（旧 magpie 口径 2=可领取；task_status 字段缺失
/// /旧形态响应 None 时保持旧流程兼容）；None=其余已知值确定性不可签（调用方 skip）。
/// 判定依据：2026-11-21 proxy_req 抓包——客户端 v3/detail 未签态 daily_signin
/// task_status=1 + checked_in_today=false，随后 landing/checkin 直签成功（reward 200）。
fn submit_endpoint_for(task_status: Option<i64>) -> Option<bool> {
    match task_status {
        None | Some(2) => Some(false),
        Some(1) => Some(true),
        Some(_) => None,
    }
}

// ── 修复④/② 当日 kv 标记（设计文档 §0.3 修复④：重试策略收敛）──
// 用 SQLite kv 而非进程内静态：GUI 调度器与 schtasks CLI 双进程须同口径防重复提交；
// 键内嵌当日日期（today_key 本地时区口径，同落库 day 键风格）自然失效，无需清理。

/// 提交类确定性拒绝当日标记键（claim 被拒 fail / pending 确认中 → 当日不再重试提交）
fn claim_rejected_key(uid: &str, day: &str) -> String {
    format!("doubao_claim_rejected:{uid}:{day}")
}

/// 每账号每日提交计数键（run_one 发 claim 后无论成败 +1，防任何路径超限第三刷）
fn claim_submit_count_key(uid: &str, day: &str) -> String {
    format!("doubao_claim_submit_count:{uid}:{day}")
}

/// pending「确认中」标记键（值含落库时间戳供排障；复查线程完成后删除）
fn claim_pending_key(uid: &str, day: &str) -> String {
    format!("doubao_claim_pending:{uid}:{day}")
}

/// 落当日「提交类确定性拒绝」标记（修复④）。写失败必须落 app_log：标记丢失会让
/// 调度器冷却重跑 run_batch 时再次提交，触碰「不做失败重试刷接口」风控红线
fn claim_rejected_mark(data_dir: &std::path::Path, uid: &str, day: &str) {
    if let Err(e) = crate::store::db(data_dir).kv_set(&claim_rejected_key(uid, day), &true) {
        crate::fs_utils::app_log(
            data_dir,
            &format!("[doubao-checkin] 当日拒绝标记写入失败（调度重试链可能再次提交）: {e}"),
        );
    }
}

/// 单账号一轮签到（launch 活动组 → detail 判定 → 未签则按 task_status 分派提交端点：
/// v3 口径 task_status=1 走 landing/checkin，旧口径 2 走 item/reward/claim）。
/// probe = 客户端抓包参数（None 回退内置稳定参数）。
/// 返回 outcome：status ∈ success | already | skip | fail
/// （skip = 确定性不可签到：未投放/不可签 task_status，落库但不进调度重试链，审查 #8）。
/// 修复⑥（2026-11-21 改版）：task_status 分派见 submit_endpoint_for——v3 未签态
/// task_status=1 是可签态（旧逻辑误判 skip 导致当日三账号全部漏签）。
/// 修复③：claim 被拒/确认中时给出「豆包客户端点一下」明示（客户端手动签到会经
/// 代理捕获链路按日落库，handler.rs try_capture_doubao_campaign 兜底闭环）。
/// 修复④（重试策略收敛）：当日已有「提交类确定性拒绝」标记（claim 被拒/确认中）
/// 或当日提交次数达上限时，不再发 claim 写请求，只查 detail（已签由 ③ already 兜住，
/// 未签落「请打开豆包客户端点击签到领取」只读文案）；claim 提交无论成败计数 +1。
pub fn run_one(
    agent: &ureq::Agent,
    data_dir: &std::path::Path,
    uid: &str,
    sid: &str,
    sg: Option<&str>,
    name: &str,
    probe: Option<&CampaignProbe>,
) -> Value {
    let day = crate::doubao_checkin_results::today_key();
    // 修复④：当日「提交类确定性拒绝」标记（claim 被拒/确认中落库时写入）→ 本次不发
    // claim 写请求（launch/detail 只读不受影响），提交前据此短路落只读文案
    let claim_rejected_today = crate::store::db(data_dir)
        .kv_get::<bool>(&claim_rejected_key(uid, &day));
    let ms = probe.and_then(|p| p.ms_token.as_deref());
    // 抓包对齐（2026-11-06）：UA/设备 Cookie 取本账号 probe，缺失回退内置常量
    let ua_s = probe
        .and_then(|p| p.ua.as_deref())
        .filter(|s| !s.is_empty())
        .unwrap_or(UA);
    let device_cookies: &[(String, String)] = probe
        .map(|p| p.device_cookies.as_slice())
        .unwrap_or(&[]);
    let fail = |message: String, detail: Option<&Value>| {
        json!({
            "uid": uid, "name": name, "status": "fail", "reward": Value::Null,
            "continuous_days": detail.map(|d| d.get("continuous_days").cloned()).flatten().unwrap_or(Value::Null),
            "total_amount": detail.map(|d| d.get("total_amount").cloned()).flatten().unwrap_or(Value::Null),
            "last_checkin_date": detail.map(|d| d.get("last_checkin_date").cloned()).flatten().unwrap_or(Value::Null),
            "message": message,
            "day": day,
        })
    };

    // ① launch：账号实际投放的活动组（修复⑤）；显式空串 = 未投放 → skip（不发 detail/claim）
    // 审查 #8：未投放/task_status≠2 是确定性不可签到（非故障），归 skip 不归 fail——
    // fail 会触发调度器当日冷却重试，对确定性账号每日反复整批重跑 + probe 保活
    // （开客户端抓包）纯属无效开销；skip 落库与运维历史照常，仅不进重试链
    let skip = |message: String, detail: Option<&Value>| {
        json!({
            "uid": uid, "name": name, "status": "skip", "reward": Value::Null,
            "continuous_days": detail.map(|d| d.get("continuous_days").cloned()).flatten().unwrap_or(Value::Null),
            "total_amount": detail.map(|d| d.get("total_amount").cloned()).flatten().unwrap_or(Value::Null),
            "last_checkin_date": detail.map(|d| d.get("last_checkin_date").cloned()).flatten().unwrap_or(Value::Null),
            "message": message,
            "day": day,
        })
    };
    let group = match campaign_group_cached(sid) {
        Some(g) => g,
        None => match campaign_group_from_launch(agent, sid, sg, ms, ua_s, device_cookies) {
            Ok(Some(g)) if !g.is_empty() => {
                campaign_group_store(sid, &g);
                g
            }
            Ok(Some(_)) => {
                return skip("该账号当前未投放签到活动（launch 活动组为空）——跳过提交，请确认账号在豆包客户端能看到签到入口".into(), None);
            }
            // 字段缺失（Ok(None)）或网络/信封失败（Err）：回退 SCENE 常量继续 detail——
            // detail 自身能反映任务有无（has_signin_task/task_status），launch 层面的
            // 不确定不改变签到流程（审查 #1：字段缺失 ≠ 未投放，不得据此短路）
            Ok(None) | Err(_) => SCENE.to_string(),
        },
    };

    // ② detail：签到状态 + 总额度 + task_status（修复⑥）
    let detail = match post_json(
        agent,
        DETAIL_URL,
        sid,
        sg,
        ms,
        &json!({"campaign_group_code": group}),
        ua_s,
        device_cookies,
    ) {
        Ok(body) => parse_detail(&body),
        Err(e) => return fail(format!("detail 查询：{e}"), None),
    };
    let checked_in_today = detail
        .get("checked_in_today")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let total_amount = detail.get("total_amount").cloned().unwrap_or(Value::Null);

    // ③ 已签 → already（回写 detail 携带的连签/上次日期即可）
    if checked_in_today {
        return json!({
            "uid": uid, "name": name, "status": "already", "reward": Value::Null,
            "continuous_days": detail.get("continuous_days").cloned().unwrap_or(Value::Null),
            "total_amount": total_amount,
            "last_checkin_date": detail.get("last_checkin_date").cloned().unwrap_or(Value::Null),
            "message": "今日已签到",
            "day": day,
        });
    }

    // ④ task_status 门槛（2026-11-21 活动改版双口径）：旧逻辑「非 2 即 skip」把 v3
    // 未签态（task_status=1）误判为不可领取——当天三个账号全部 skip 而客户端实际可签。
    // 现按 submit_endpoint_for 分派：1 → landing/checkin（v3 客户端真实形态），
    // 2/字段缺失 → 旧 item/reward/claim，其余已知值 → skip
    let task_status = detail.get("task_status").and_then(Value::as_i64);
    let use_landing = match submit_endpoint_for(task_status) {
        Some(u) => u,
        None => {
            return skip(
                format!(
                    "今日签到暂不可领取（task_status={}，非可签/可领态）——豆包按账号投放该活动；\
                     若客户端能手动签到，请开启代理后在豆包客户端点一次，日志会捕获并按日落库",
                    task_status.unwrap_or(-1)
                ),
                Some(&detail),
            );
        }
    };

    // ④′ 修复④：当日「提交类确定性拒绝」→ 只读复查不重试提交（对齐 magpie「提交
    // 一次、回查确认；没确认就当天只查不重提」）。detail 已签由 ③ already 兜住；
    // 未签落客户端领取明示（skip 确定性态，不进调度重试链）
    if claim_rejected_today {
        return skip(
            "当日已提交过签到且被拒绝/确认中——请打开豆包客户端点击签到领取（代理开着会自动捕获落库）"
                .into(),
            Some(&detail),
        );
    }
    // ④″ 修复④：每账号每日提交上限 2 次，超限只读（任何路径不得第三刷）
    let submit_count = crate::store::db(data_dir).kv_get::<i64>(&claim_submit_count_key(uid, &day));
    if submit_count >= DAILY_SUBMIT_LIMIT {
        return skip(
            "今日签到提交次数已达上限——请打开豆包客户端点击签到领取（代理开着会自动捕获落库）"
                .into(),
            Some(&detail),
        );
    }

    // ⑤ 未签且可签 → 提交。v3（task_status=1）走 landing/checkin，body 仅
    // {"scene": group}（2026-11-21 客户端抓包实锤形态）；旧口径走 item/reward/claim
    // （字段名红线 campaign_item_code）。
    // 审查 #4（有意识取舍）：probe.checkin_query 回放到提交端点——a_bogus 一次性
    // token 已在捕获侧剔除不入 vault（handler.rs 2026-10-29 审查修复），msToken 为
    // 会话级复用值非一次性（实测仅稳定 query + Cookie 即可通过）；24h 新鲜期到期整组
    // 弃用走内置稳定参数，回放窗口内失效属可接受残余风险
    let (submit_url, submit_body) = if use_landing {
        let url = match probe {
            Some(p) if !p.checkin_query.is_empty() => {
                format!("{LANDING_CHECKIN_BASE}?{}", p.checkin_query)
            }
            _ => LANDING_CHECKIN_URL.to_string(),
        };
        (url, json!({ "scene": group }))
    } else {
        let url = match probe {
            Some(p) if !p.checkin_query.is_empty() => format!("{CLAIM_BASE}?{}", p.checkin_query),
            _ => CLAIM_URL.to_string(),
        };
        (
            url,
            json!({
                "task_id": "",                       // daily_signin 无 task_id，空串为正确形态
                "campaign_group_code": group,
                "campaign_item_code": "daily_signin", // 字段名红线：campaign_item_code
            }),
        )
    };
    let claim_res = post_json(agent, &submit_url, sid, sg, ms, &submit_body, ua_s, device_cookies);
    // 修复④：提交类请求无论成败（含 pending/业务码/网络层失败）完成后计数 +1——
    // 上限口径是「已发出的提交尝试」；写失败落日志（计数丢失会让超限防线失效）
    if let Err(e) = crate::store::db(data_dir)
        .kv_set(&claim_submit_count_key(uid, &day), &(submit_count + 1))
    {
        crate::fs_utils::app_log(
            data_dir,
            &format!("[doubao-checkin] 当日提交计数写入失败（超限防线可能失效）: {e}"),
        );
    }
    match claim_res {
        Ok(body) => {
            let (is_success, is_already, is_pending, bmsg) = parse_claim_outcome(&body);
            let data = body.get("data");
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
            if is_success {
                return json!({
                    "uid": uid, "name": name, "status": "success",
                    // day 取轮次处理时刻口径（跨零点轮次与 record_day_at 注释一致）；
                    // macos_main 合并审查 Low：删除重复键（旧 today_key() 首键被静默覆盖）
                    "day": day,
                    "reward": reward.map(Value::from).unwrap_or(Value::Null),
                    // P3-13 修复补全：reward 缺失 = checked_in=true 但响应未携带
                    // reward_amount（服务端确认中形态）——显式标记供前端区分「真实
                    // +0」与「确认中」。此前前端靠 `'reward' in o` 判定，但本分支恒写
                    // reward 键（None→null），判定恒真为死代码（审查修复）
                    "reward_present": reward.is_some(),
                    "continuous_days": continuous.map(Value::from)
                        .or_else(|| detail.get("continuous_days").cloned()),
                    "total_amount": total_amount,
                    "last_checkin_date": date.map(Value::from)
                        .or_else(|| detail.get("last_checkin_date").cloned()),
                    // reward 缺失时不再输出「签到成功 +0 额度」误导文案（确认中形态
                    // 与 run_one pending 分支口径一致，以复查/落库结果为准）
                    "message": match reward {
                        Some(r) => format!("签到成功 +{r} 额度"),
                        None => "签到已提交，额度确认中（以复查结果为准）".to_string(),
                    },
                });
            }
            if is_already {
                // 幂等命中（审查 #2）：此前已领过 → already（不虚增成功统计）
                return json!({
                    "uid": uid, "name": name, "status": "already", "reward": Value::Null,
                    "continuous_days": continuous.map(Value::from)
                        .or_else(|| detail.get("continuous_days").cloned()),
                    "total_amount": total_amount,
                    "last_checkin_date": date.map(Value::from)
                        .or_else(|| detail.get("last_checkin_date").cloned()),
                    "message": "今日已签到（claim 幂等命中）",
                    "day": day,
                });
            }
            if is_pending {
                // 确认中（修复②）：服务端异步延迟确认，立即复查 detail 大概率未翻转
                // （审查 #3：砍掉立即复查，省一次无效读请求）——落 fail + 客户端兜底
                // 明示（修复③）；当日不再重试提交（修复④：落当日拒绝标记，调度器
                // 冷却重跑 run_batch 时 run_one 据此只读复查），10 分钟后仅 detail-only
                // 复查一次（spawn_pending_recheck，修复②）；若实际已到账，客户端侧
                // 代理捕获链路/复查补记会落 success（record_day fail-不覆盖-success 规则
                // 保最终事实）
                claim_rejected_mark(data_dir, uid, &day);
                let _ = crate::store::db(data_dir).kv_set(
                    &claim_pending_key(uid, &day),
                    &json!({ "ts": fs_utils::now_ts() }),
                );
                let mut o = fail(
                    format!(
                        "领取确认中未完成（{bmsg}）——豆包把领取限死在电脑版客户端，\
                         请打开豆包客户端点一下签到按钮（代理开着会自动捕获落库）；\
                         当日不再重试提交，10 分钟后仅复查一次结果（只读）"
                    ),
                    Some(&detail),
                );
                // 修复④：结构化标记供 run_batch 失败分流（Err→Ok，不进调度冷却重试链）
                o["claim_rejected"] = json!(true);
                o["pending_recheck"] = json!(true);
                return o;
            }
            // 领取被拒（修复④：信封 code=0 的 biz_error 明确拒绝属提交类确定性拒绝）
            // ——当日不再重试提交，调度器冷却重跑时 run_one 据标记只读复查
            claim_rejected_mark(data_dir, uid, &day);
            let mut o = fail(
                format!(
                    "领取被拒：{bmsg}；若客户端能手动签到，请开启代理后在客户端点一次（当日不再重试提交）"
                ),
                Some(&detail),
            );
            o["claim_rejected"] = json!(true);
            return o;
        }
        Err(e) => {
            // 业务码失败 → detail 复查兜底：部分活动侧业务码实际对应「已签/活动未覆盖」
            if e.starts_with("接口返回异常 code=") {
                if let Ok(re_body) = post_json(
                    agent,
                    DETAIL_URL,
                    sid,
                    sg,
                    ms,
                    &json!({"campaign_group_code": group}),
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
                            "day": day,
                        });
                    }
                    let has_task = re.get("has_signin_task").and_then(Value::as_bool).unwrap_or(false);
                    // 2026-11-21 排障：发奖端点 710010702 两层嫌疑——①抓包参数缺失/过期
                    // （回退内置参数的形态已知被拒）；②账号级风控（客户端手动签到同报
                    // 「签到失败」即为②，伪装 system error，通常 24h 内过期）。文案分层明示
                    let probe_hint = if probe_own_fresh(data_dir, sid) {
                        String::new()
                    } else {
                        // 文案覆盖两级回退（本账号键缺失时 load_campaign_probe 仍可能
                        // 命中全局兜底 probe，未必是内置参数——2026-11-21 二轮审查修正）
                        "本账号抓包参数缺失/已过期（本次使用回退参数：全局兜底或内置稳定参数，\
                         设备指纹不对齐时发奖端点大概率被拒）——\
                         请在豆包设置开启「签到前自动抓包」或开代理后用「以账号打开」刷新参数再重试；"
                            .to_string()
                    };
                    let hint = if has_task {
                        "detail 有 daily_signin 任务但提交被拒——若客户端能手动签到，\
                         请开启代理后在豆包客户端点一次（日志会捕获并按日落库）；\
                         若客户端同样报「签到失败」，为账号级风控（通常 24h 内过期后自愈，勿反复重试提交）"
                    } else {
                        "detail 无 daily_signin 任务项 → 该账号未加入当前签到活动（活动组不匹配或无资格）"
                    };
                    return fail(format!("{e}；{probe_hint}{hint}"), Some(&re));
                }
            }
            fail(format!("签到提交：{e}"), Some(&detail))
        }
    }
}

/// 追加滚动运维历史（同 doubao_quota::append_history，kind="checkin"；失败落 app_log）。
/// 参数收窄为 data_dir：persist_outcomes 与 pending 复查后台线程（无 AppState 句柄，
/// 修复②）共用
fn append_history(data_dir: &std::path::Path, event: Value) {
    let store = crate::store::db(data_dir);
    let mut events = crate::store::docs::doubao_health_load(&store);
    events.push(event);
    if events.len() > HISTORY_MAX {
        events.drain(0..events.len() - HISTORY_MAX);
    }
    if let Err(e) = crate::store::docs::doubao_health_save(&store, &events) {
        crate::fs_utils::app_log(
            data_dir,
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
/// 审查 #7 同步修复⑤：campaign_group_code 不再硬编码 SCENE 常量——先经 launch
/// 读账号实际投放组（失败/缺失回退 SCENE，与 run_one 同语义）；UA/设备 Cookie/
/// msToken 走本账号抓包 probe（缺失回退内置常量），与签到请求形态一致降低风控拒率。
pub fn detail_probe(
    agent: &ureq::Agent,
    data_dir: &std::path::Path,
    sid: &str,
    sg: Option<&str>,
    ms: Option<&str>,
) -> Option<Value> {
    let probe = load_campaign_probe(data_dir, Some(sid));
    probe_detail(agent, sid, sg, ms, probe.as_ref())
}

/// detail 探针核心（probe 由调用方注入，便于单测与上层复用）
pub fn probe_detail(
    agent: &ureq::Agent,
    sid: &str,
    sg: Option<&str>,
    ms: Option<&str>,
    probe: Option<&CampaignProbe>,
) -> Option<Value> {
    let p_ms = probe.and_then(|p| p.ms_token.as_deref()).or(ms);
    let ua_s = probe
        .and_then(|p| p.ua.as_deref())
        .filter(|s| !s.is_empty())
        .unwrap_or(UA);
    let device_cookies: &[(String, String)] = probe
        .map(|p| p.device_cookies.as_slice())
        .unwrap_or(&[]);
    // 活动组：launch 实际投放组优先（审查修复 #2：命中缓存免重复 launch），
    // 失败/字段缺失回退 SCENE（与 run_one 同语义）
    let group = match campaign_group_cached(sid) {
        Some(g) => g,
        None => campaign_group_from_launch(agent, sid, sg, p_ms, ua_s, device_cookies)
            .ok()
            .flatten()
            .filter(|g| !g.is_empty())
            .map(|g| {
                campaign_group_store(sid, &g);
                g
            })
            .unwrap_or_else(|| SCENE.to_string()),
    };
    let body = post_json(
        agent,
        DETAIL_URL,
        sid,
        sg,
        p_ms,
        &json!({"campaign_group_code": group}),
        ua_s,
        device_cookies,
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
/// 返回 false = 当日记录未落库（Busy 跳过/读库中止，语义见 record_day_at）——
/// 批量调用方据此改打 skipped_busy 交调度器不 mark_run、下一轮补录。
fn persist_outcomes(state: &AppState, outcomes: &[Value], source: &str, skip_on_busy: bool) -> bool {
    // ① 按日落库（success/already/fail/skip 全记；reward 仅 success 携带）。
    // day 取 outcome 产生时刻打点（跨零点批次各归各日）；旧 outcome 无 day 回退当日。
    // message 仅 fail/skip 携带（失败原因/确定性跳过原因落库，切页/重开后日历明细
    // 仍可排障）——success/already 的 message 恒为固定成功文案，落库只增体积无价值
    let today = crate::doubao_checkin_results::today_key();
    let entries = outcomes.iter().map(|o| {
        let status = o.get("status").and_then(Value::as_str).unwrap_or("fail");
        (
            o.get("uid").and_then(Value::as_str).unwrap_or("").to_string(),
            o.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
            status.to_string(),
            o.get("reward").and_then(Value::as_i64),
            // fail/skip 携带 message（skip = 确定性跳过原因落库，main 3.9.4 口径）；
            // 钳制同 mac 审查修复（截断 500 字符 + 控制字符清洗）
            (status == "fail" || status == "skip")
                .then(|| clamp_fail_message(o.get("message").and_then(Value::as_str)))
                .flatten(),
            o.get("day").and_then(Value::as_str).unwrap_or(&today).to_string(),
        )
    });
    // skip_on_busy 由调用方按来源决定（task 批量=true / manual 单账号=false，见函数注释）
    let day_recorded = crate::doubao_checkin_results::record_day_at(&state.data_dir, entries, skip_on_busy);

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
            match serde_json::from_value::<crate::commands::doubao::DoubaoAccountPool>(latest) {
                Ok(file) => {
                    if let Err(e) = crate::commands::doubao::save_pool(state, &file) {
                        // vault 写失败等场景（issue #153 语义返回 Err）：池四字段本次更新丢失，留痕
                        crate::fs_utils::app_log(
                            &state.data_dir,
                            &format!("[doubao_checkin] 签到结果回写账号池失败: {e}"),
                        );
                    }
                }
                Err(e) => {
                    // 反序列化失败：本轮签到四字段（quota_total 等）全部丢弃，必须留痕
                    crate::fs_utils::app_log(
                        &state.data_dir,
                        &format!("[doubao_checkin] 签到结果回写账号池反序列化失败（本次四字段更新丢失）: {e}"),
                    );
                }
            }
        }
    }

    // ③ 运维历史（逐账号，kind="checkin"）；skip 是确定性跳过非故障，不算 ok=false；
    // summary 同款钳制防多行日志干扰
    for o in outcomes {
        let status = o.get("status").and_then(Value::as_str).unwrap_or("fail");
        let ok = status != "fail";
        append_history(
            &state.data_dir,
            json!({
                "ts": fs_utils::now_ts(), "kind": "checkin", "uid": o.get("uid"), "ok": ok,
                "status": o.get("status"), "reward": o.get("reward"),
                "summary": clamp_fail_message(o.get("message").and_then(Value::as_str)),
                "source": source,
            }),
        );
    }
    day_recorded
}

/// 失败原因落库/入历史前的钳制（审查修复）：截断 500 字符 + 剔除控制字符——
/// 防上游异常文案把按日表（365 天窗口全量反序列化是看板成本敏感路径）撑爆、
/// 防多行内容干扰日志 grep（日志注入面）；前端 toast 已另截 80 字符
fn clamp_fail_message(v: Option<&str>) -> Option<String> {
    v.filter(|m| !m.is_empty())
        .map(|m| {
            m.chars()
                // 换行/制表先映射为空格（审查发现：原 is_control 直删会把多行文案
                // 在行边界粘连成 "HTTP 401详情：xxx" 式畸形串，下游 clamp_msg 的
                // 单行化 replace 恒不命中成死代码）；其余控制字符仍剔除
                .map(|c| if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c })
                .filter(|c| !c.is_control())
                .take(500)
                .collect::<String>()
        })
}

/// 修复④：失败分流（纯函数，run_batch Err/Ok 判定与单测共用）——
/// 返回 (提交类确定性拒绝失败数, 其他失败数)。前者 = outcome 带 claim_rejected=true
/// 的 fail（claim 被拒/确认中，run_one 已落当日「不再重试提交」标记）；后者 = 网络层/
/// 系统错误等非确定性失败，维持调度重试链
fn split_claim_rejected_fails(outcomes: &[Value]) -> (usize, usize) {
    let rejected = outcomes
        .iter()
        .filter(|o| {
            o.get("status").and_then(Value::as_str) == Some("fail")
                && o.get("claim_rejected").and_then(Value::as_bool).unwrap_or(false)
        })
        .count();
    let failed = outcomes
        .iter()
        .filter(|o| o.get("status").and_then(Value::as_str) == Some("fail"))
        .count();
    (rejected, failed - rejected)
}

/// 修复②：pending「确认中」10 分钟后 detail-only 复查（设计文档 §0.3 修复②口径）。
/// 选 std::thread::spawn + sleep（仓内 buddy_ai SSE 读线程 / probe 等待同款先例，
/// 最小侵入；调度器侧无更自然挂载点——tick 粒度与 10 分钟延迟不匹配）。复查只读
/// 不发 claim 写请求：
/// - checked_in_today 翻 true → 补记 success（带 detail.reward_amount，看板 reward_total
///   完整）；防护：当日已有 success/already 记录则跳过（落库保护逻辑——success 是
///   最强事实但补记可能无 reward，不得降级覆盖既有带 reward 的成功记录）
/// - 未翻转 → 落「请打开豆包客户端点击签到领取」skip（确定性结局，不进重试链；
///   record_day 允许 skip 覆盖 pending 落的 fail、不回退 success）；当日不再提交由
///   pending 时已落的 claim_rejected 标记保证（与修复④第 1 项同一标记）
/// 跨零点放弃复查（detail 状态属新的一天，无法用于补记 pending 所在日）；
/// skip_on_busy=false：复查结果是本线程独有，幂等跳过即永久丢失（与 manual 同口径）
fn spawn_pending_recheck(
    state: &AppState,
    uid: &str,
    sid: &str,
    sg: Option<&str>,
    name: &str,
    probe: Option<CampaignProbe>,
) {
    let data_dir = state.data_dir.clone();
    let uid = uid.to_string();
    let sid = sid.to_string();
    let sg = sg.map(str::to_string);
    let name = name.to_string();
    let day = crate::doubao_checkin_results::today_key();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(PENDING_RECHECK_DELAY_SECS));
        // 跨零点（pending 落在 23:5x）：放弃复查，pending 标记清理，当日记录保持
        // fail 原状（次日 run_one 走新日正常流程，昨日键自然失效）
        if crate::doubao_checkin_results::today_key() != day {
            let _ = crate::store::db(&data_dir).kv_delete(&claim_pending_key(&uid, &day));
            return;
        }
        let agent = http_agent(TIMEOUT_SECS);
        let detail = probe_detail(&agent, &sid, sg.as_deref(), None, probe.as_ref());
        // pending 标记无论复查成败都清理（当日不再提交由 claim_rejected 标记保证）
        let _ = crate::store::db(&data_dir).kv_delete(&claim_pending_key(&uid, &day));
        let Some(detail) = detail else {
            // detail 查询失败：保持 fail 原状（下次 run_batch 时 run_one 标记分支
            // 只读复查兜底，detail 恢复后仍能补记/明示）
            crate::fs_utils::app_log(
                &data_dir,
                &format!("[doubao-checkin] pending 复查 detail 查询失败（uid={uid}），保持当日不提交口径"),
            );
            return;
        };
        let checked = detail
            .get("checked_in_today")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !checked {
            // 未翻转：skip 明示（覆盖 pending 落的 fail；不回退既有 success/already）
            crate::doubao_checkin_results::record_day_at(
                &data_dir,
                [(
                    uid.clone(),
                    name.clone(),
                    "skip".to_string(),
                    None,
                    Some("领取确认中未翻转——请打开豆包客户端点击签到领取（当日不再重试提交）".to_string()),
                    day.clone(),
                )],
                false,
            );
            append_history(
                &data_dir,
                json!({
                    "ts": fs_utils::now_ts(), "kind": "checkin", "uid": uid, "ok": true,
                    "status": "skip", "reward": Value::Null,
                    "summary": "pending 复查：确认中未翻转，明示客户端领取",
                    "source": "pending-recheck",
                }),
            );
            return;
        }
        // 翻 true：补记 success。防护（落库保护逻辑）：当日已有 success/already
        // （客户端代理捕获/重试轮已补记）则不覆盖——避免无 reward 的补记降级既有
        // 成功记录；fail/skip/无记录才补记升级
        let f = crate::doubao_checkin_results::load(&data_dir);
        let already_recorded = f
            .days
            .get(&day)
            .and_then(|d| d.accounts.get(&uid))
            .map(|p| matches!(p.status.as_str(), "success" | "already"))
            .unwrap_or(false);
        if already_recorded {
            return;
        }
        let reward = detail.get("reward_amount").and_then(Value::as_i64);
        crate::doubao_checkin_results::record_day_at(
            &data_dir,
            [(
                uid.clone(),
                name.clone(),
                "success".to_string(),
                reward,
                None,
                day.clone(),
            )],
            false,
        );
        append_history(
            &data_dir,
            json!({
                "ts": fs_utils::now_ts(), "kind": "checkin", "uid": uid, "ok": true,
                "status": "success", "reward": reward,
                "summary": "pending 复查：detail 确认已签，补记 success",
                "source": "pending-recheck",
            }),
        );
    });
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
            // 审查发现（对齐 825-833 落库锁 Busy 与 wb/qoder 同场景口径）：轮次锁
            // Busy 跳过也须携带 skipped_busy——否则调度器 tick 落入 mark_run 分支
            // 固化「当日已跑」，赢家（他进程）批次失败时当日重试链与首败通知全丢
            // P3-12：skipped 保留字符串（整轮幂等跳过语义，调度器 push guard 按类型分流）
            return Ok(json!({ "ok": true, "skipped_busy": true, "skipped": "其他进程正在执行签到，本轮跳过", "reason": fail.describe() }));
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
        // skipped_empty_pool（审查修复）：签到是奖励型任务（漏跑=丢收益），普通
        // skipped 会被调度器 mark_run 固化「当日已跑」，当日导入账号失去补打窗口；
        // 对齐同批 doubao-quota / doubao-keepalive 的 #15 空池豁免口径。
        // P3-12：skipped 保留字符串（整轮跳过语义，push guard 按类型分流）
        return Ok(json!({ "ok": true, "skipped": "无豆包账号", "skipped_empty_pool": true }));
    }

    let agent = http_agent(TIMEOUT_SECS);
    // 逐账号取本账号 probe：load_campaign_probe(Some(sid)) 内部已做「本账号键→
    // 全局键 campaign_probe」两级回退（审查清理：删除外层 or_else 全局快照——
    // 仅在两次 vault 读之间全局键恰好变化时才命中，且取到的还是循环前的更旧
    // 快照，2026-11-06 起按 sessionid 分键，全局键本就是兜底）
    let mut outcomes: Vec<Value> = Vec::new();
    for (i, (uid, sid, sg, name)) in targets.iter().enumerate() {
        if i > 0 {
            // 修复④：2s 基准 + 0~2s 随机抖动（固定短间隔是可被上游画像的机器节拍）。
            // 零依赖原则：用纳秒取模做伪随机（本仓无 rand crate）
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as u64)
                .unwrap_or(0);
            std::thread::sleep(Duration::from_millis(ACCOUNT_GAP_MS + nanos % 2000));
        }
        let probe = load_campaign_probe(&state.data_dir, Some(sid));
        let outcome = run_one(&agent, &state.data_dir, uid, sid, sg.as_deref(), name, probe.as_ref());
        // 修复②：确认中（pending）→ 10 分钟后 detail-only 复查（后台线程，不占调度
        // 轮；复查只读不发 claim 写请求，见 spawn_pending_recheck）
        if outcome
            .get("pending_recheck")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            spawn_pending_recheck(state, uid, sid, sg.as_deref(), name, probe.clone());
        }
        outcomes.push(outcome);
    }
    let day_recorded = persist_outcomes(state, &outcomes, "task", true);

    let success = outcomes
        .iter()
        .filter(|o| o.get("status").and_then(Value::as_str) == Some("success"))
        .count();
    let already = outcomes
        .iter()
        .filter(|o| o.get("status").and_then(Value::as_str) == Some("already"))
        .count();
    // skip（审查 #8）：确定性不可签到（未投放/task_status≠2）——不计失败不触发
    // 调度器当日冷却重试（mark_fail），落库照常仅不进重试链
    let skipped = outcomes
        .iter()
        .filter(|o| o.get("status").and_then(Value::as_str) == Some("skip"))
        .count();
    let failed = outcomes.len() - success - already - skipped;
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
    // 失败汇总，无数据副作用。
    // 修复④：失败分流——仅存在**非确定性失败**（网络/系统错误）才维持 Err（触发
    // mark_fail → 30/60/120min 冷却重跑 run_batch；重跑时确定性拒绝账号已被 run_one
    // 当日标记拦住只读复查，不会再提交）；failed 全部为「提交类确定性拒绝」时改返
    // Ok——Err 会让调度器当日反复重跑整批，对已确定性拒绝的账号纯属再次提交（触碰
    // 「不做失败重试刷接口」红线），失败已在结果明细/summary 中体现
    let (claim_rejected_fails, other_fails) = split_claim_rejected_fails(&outcomes);
    if failed > 0 && other_fails > 0 {
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
                // 汇总文案同款钳制（审查修复）：errors 数组保原始 message 供前端
                // 展示，Err 字符串走 CLI 退出输出/通知渠道，超长多行文案会污染两者
                clamp_fail_message(e.get("error").and_then(Value::as_str)).map(|m| {
                    format!(
                        "{}：{}",
                        e.get("name").and_then(Value::as_str).unwrap_or("未知账号"),
                        clamp_msg(m.as_str())
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
            "豆包签到：{success} 成功 / {already} 已领 / {failed} 失败（另有 {skipped} 个账号确定性跳过）：{err_head}{probe_hint}"
        ));
    }
    // 修复④：failed 全部为「提交类确定性拒绝」（claim 被拒/确认中）→ 返 Ok——
    // 调度器 mark_run 固化「当日已跑」，当日不再重跑提交（Err 会触发冷却重试链
    // 对已拒绝账号再次提交）；失败已在 outcome 落库、errors 明细与 summary 计数中
    // 体现；pending 账号另有 10 分钟后 detail-only 复查兜底（spawn_pending_recheck）
    if claim_rejected_fails > 0 {
        let mut ret = json!({
            "ok": true, "mode": "all",
            "total": outcomes.len(), "success": success, "already": already, "failed": failed,
            "skipped": skipped,
            "claim_rejected": claim_rejected_fails,
            "reward_total": reward_total,
            "errors": errors,
            "probe_keepalive_error": probe_keepalive_error,
            "finished_at": fs_utils::now_ts(),
        });
        if !day_recorded {
            // 审查修复：与常规出口同口径——落库被其他进程占用时打 skipped_busy 交
            // 调度器不 mark_run，下一 tick 重跑补录；否则失败明细与「当日已跑」
            // 双重固化（claim_rejected 结果永久不落库）。已知权衡：补录轮会对已
            // 拒绝账号再次提交（与常规出口补录轮一致，DAILY_SUBMIT_LIMIT 封顶）
            ret["skipped_busy"] = json!(true);
            // P3-12 审查修复：skipped 保持数字计数不覆写，落库提示挂 skipped_reason
            //（常规出口同款，见上方说明——字符串覆写会吞掉成功推送）
            ret["skipped_reason"] = json!("签到结果落库被其他进程占用，本轮结果未记录；稍后重试签到即可补录");
        }
        return Ok(ret);
    }

    let mut ret = json!({
        "ok": true, "mode": "all",
        "total": outcomes.len(), "success": success, "already": already, "failed": failed,
        "skipped": skipped,
        "reward_total": reward_total,
        "errors": errors,
        "probe_keepalive_error": probe_keepalive_error,
        "finished_at": fs_utils::now_ts(),
    });
    if !day_recorded {
        // Busy 落库跳过（审查修复）：本轮签到已实际执行但当日记录未落库——打
        // skipped_busy 交调度器不 mark_run，下一 tick 重跑补录（账号已签→already，
        // 幂等）；若固化 mark_run 会「结果丢了且当日已跑」双重损失。字段仅在
        // true 时存在（tick 按字段存在性分流，恒置 false 会误入 busy 分支）。
        // 文案不承诺「自动补录」：调度器开启时由下一 tick 补录，GUI 手动路径
        // 需用户稍后重试（签到幂等，重试只回 already，无重复奖励）。
        // 已知缺口（审查披露）：补录轮对已签账号走 already（reward=None）——
        // 若本轮 success 账号的 reward 随落库失败一起丢失，服务端已发放的额度
        // 明细不会回填当日结果表（池回写与运维历史不受影响），看板「今日新增」
        // 可能缺失该笔；CLI 并发对下他方进程会写入同批 reward，窗口仅池变更/
        // 跨版本并存时打开
        ret["skipped_busy"] = json!(true);
        // P3-12 审查修复：skipped_busy 落库提示改挂 skipped_reason（字符串）——
        // skipped 统一为数字型批内跳过计数，调度器 push guard 按类型分流
        //（字符串=整轮幂等跳过不推送 / 数字=已真实执行可推送），此处若覆写字符串
        // 会把「已执行成功」轮伪装成整轮跳过，成功通知被永久吞掉（P1-2 回归面）
        ret["skipped_reason"] = json!("签到结果落库被其他进程占用，本轮结果未记录；稍后重试签到即可补录");
    }
    Ok(ret)
}

/// 单账号签到（应用内手动路径）：读池凭证 → probe 保活 → run_one → 落库/回写/历史。
/// P2 审查修复：补跨进程轮次锁（与 run_batch 的 "doubao"/"checkin-round" 同键互斥）
/// ——GUI 手动单签与调度器/CLI 批量并发时，DAILY_SUBMIT_LIMIT 的 kv 计数是
/// read-modify-write（run_one 内读→提交后写），无锁下两路径可同时通过上限检查，
/// TOCTOU 突破每日提交上限（风控红线）。Busy 返 ok+skipped_busy 幂等跳过
/// （GUI 层拿到明确提示，调度器侧语义与其他任务 skipped_busy 口径一致）。
pub fn checkin_single(state: &AppState, app: Option<&tauri::AppHandle>, uid: &str) -> Result<Value, String> {
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
                fs_utils::app_log(
                    &state.data_dir,
                    &format!("豆包单账号签到轮次锁获取失败：{}", fail.describe()),
                );
            }
            return Ok(json!({
                "ok": true,
                "skipped_busy": true,
                // P3-12：skipped 保留字符串（整轮幂等跳过，push guard 按类型分流）
                "skipped": "其他进程正在执行豆包签到，本轮跳过",
                "reason": fail.describe(),
                "finished_at": fs_utils::now_ts(),
            }));
        }
    };
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
    // 手动触发与调度路径同受修复④约束：当日拒绝标记/每日提交上限在 run_one 内生效
    let outcome = run_one(&agent, &state.data_dir, uid, sid, sg.as_deref(), name, probe.as_ref());
    // 修复②：确认中 → 10 分钟后 detail-only 复查（与 run_batch 同款后台线程）
    if outcome
        .get("pending_recheck")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        spawn_pending_recheck(state, uid, sid, sg.as_deref(), name, probe.clone());
    }
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

    // ── parse_claim_outcome（修复②：信封 code=0 ≠ 领取成功；审查 #2：幂等归 already）──

    #[test]
    fn claim_成功形态_checked_in_true() {
        let b = json!({"code": 0, "data": {"checked_in": true, "reward_amount": 200}});
        let (ok, already, pending, msg) = parse_claim_outcome(&b);
        assert!(ok && !already && !pending && msg.is_empty());
    }

    #[test]
    fn claim_幂等命中_归already不归success() {
        // 审查 #2：幂等命中 = 此前已领过，须与 handler.rs 落库链路 already 口径一致，
        // 否则 CLI 统计虚增成功数（「签到成功 +0 额度」误导）
        let b = json!({"code": 0, "data": {"idempotent": true}});
        let (ok, already, pending, _) = parse_claim_outcome(&b);
        assert!(!ok && already && !pending);
    }

    #[test]
    fn claim_checkin_date有值且checked_in_false_兜底归already() {
        // 旧兜底恢复（防回执形态变化误判 fail 触发重提）：checkin_date 有值且
        // checked_in=false → 当日实际已签，对齐历史口径 `!checked && date.is_some()`
        let b = json!({"code": 0, "data": {"checked_in": false, "checkin_date": "2026-11-20"}});
        let (ok, already, pending, _) = parse_claim_outcome(&b);
        assert!(!ok && already && !pending);
    }

    #[test]
    fn claim_确认中_biz_error_code_3() {
        // magpie 实测形态：信封 code=0 但实际未领取成功
        let b = json!({"code": 0, "data": {
            "biz_error": {"code": 3, "message": "领取结果确认中，请稍后重试"},
            "task_status": 0
        }});
        let (ok, already, pending, msg) = parse_claim_outcome(&b);
        assert!(!ok && !already && pending && msg.contains("确认中"));
    }

    #[test]
    fn claim_业务层明确拒绝_非确认中() {
        let b = json!({"code": 0, "data": {
            "biz_error": {"code": 8, "message": "当前暂不符合领取条件"}
        }});
        let (ok, already, pending, msg) = parse_claim_outcome(&b);
        assert!(!ok && !already && !pending && msg.contains("不符合领取条件"));
    }

    #[test]
    fn claim_无checked_in无biz_error_按失败() {
        let b = json!({"code": 0, "data": {"task_status": 0}});
        let (ok, already, pending, msg) = parse_claim_outcome(&b);
        assert!(!ok && !already && !pending && msg.contains("未生效"));
    }

    #[test]
    fn submit_endpoint_分派_20261121改版双口径() {
        // 2026-11-21 抓包实锤：v3 未签态 task_status=1（旧逻辑误 skip 导致漏签）
        assert_eq!(submit_endpoint_for(Some(1)), Some(true)); // landing/checkin
        assert_eq!(submit_endpoint_for(Some(2)), Some(false)); // 旧 item/reward/claim
        assert_eq!(submit_endpoint_for(None), Some(false)); // 字段缺失保持旧流程兼容
        assert_eq!(submit_endpoint_for(Some(0)), None); // 其余已知值 → skip
        assert_eq!(submit_endpoint_for(Some(3)), None);
    }

    // ── campaign_group_from_launch 返回语义（审查 #1：缺失 ≠ 显式空串）──
    // 注：网络请求函数不直接单测（依赖 ureq Agent），此处验证返回类型契约——
    // Ok(Some("")) = 显式空串（未投放短路）、Ok(None) = 字段缺失（回退 SCENE）。
    // launch 解析逻辑由 probe_detail 集成路径覆盖（campaign_group_code 缺失回退）。

    #[test]
    fn trend_聚合_skip独立计数不虚增失败率() {
        // 审查 #8：skip（未投放/task_status≠2）是确定性不可签到，归 failed 会虚增
        // 失败率——trend_from 单列 skip 计数（2026-11-20 看板：不再并入 already，
        // 前端堆叠图单独灰柱展示）
        let mut f = crate::doubao_checkin_results::ResultsFile::default();
        f.record_day("2026-11-17", [
            ("u1".into(), "账号1".into(), "success".into(), Some(200), None),
            ("u2".into(), "账号2".into(), "skip".into(), None, Some("未投放".into())),
            ("u3".into(), "账号3".into(), "fail".into(), None, Some("网络失败".into())),
        ]);
        let trend = crate::doubao_checkin_results::trend_from(&f, 7);
        assert_eq!(trend.len(), 1);
        assert_eq!(trend[0].ok, 1);
        assert_eq!(trend[0].already, 0); // skip 不并入 already
        assert_eq!(trend[0].skip, 1); // 单列 skip 计数
        assert_eq!(trend[0].failed, 1); // 仅真实 fail 计失败
        assert_eq!(trend[0].reward_total, 200);
    }

    /// skip 是当日确定性事实：后续重试轮 fail 不得覆盖（与 success/already 同规则）
    #[test]
    fn record_day_fail不覆盖skip() {
        let mut f = crate::doubao_checkin_results::ResultsFile::default();
        f.record_day("2026-11-17", [
            ("u1".into(), "账号1".into(), "skip".into(), None, Some("未投放".into())),
        ]);
        f.record_day("2026-11-17", [
            ("u1".into(), "账号1".into(), "fail".into(), None, Some("网络失败".into())),
        ]);
        let day = f.days.get("2026-11-17").unwrap();
        let st = day.accounts.get("u1").unwrap();
        assert_eq!(st.status, "skip");
        assert_eq!(st.message.as_deref(), Some("未投放"));
    }

    // ── parse_detail task_status（修复⑥：可领取态门槛）──

    #[test]
    fn parse_detail_透出task_status供提交门槛() {
        let body = json!({"code": 0, "data": {"tasks": [
            {"campaign_code": "daily_signin", "task_kind": "daily_signin",
             "task_status": 2,
             "daily_checkin": {"checked_in_today": false}}
        ]}});
        let p = parse_detail(&body);
        assert_eq!(p["task_status"], 2);
        assert_eq!(p["has_signin_task"], true);
        // 无投放账号：task_status=1（未满足条件）→ 提交门槛直接短路
        let body = json!({"code": 0, "data": {"tasks": [
            {"campaign_code": "daily_signin", "task_kind": "daily_signin", "task_status": 1}
        ]}});
        let p = parse_detail(&body);
        assert_eq!(p["task_status"], 1);
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

    // ── 修复④：失败分流（run_batch Err/Ok 判定口径）──

    #[test]
    fn 分流_全为提交类确定性拒绝_其他失败为零() {
        let outcomes = vec![
            json!({"uid": "u1", "status": "fail", "claim_rejected": true, "message": "领取被拒"}),
            json!({"uid": "u2", "status": "fail", "claim_rejected": true, "pending_recheck": true}),
            json!({"uid": "u3", "status": "success"}),
        ];
        let (rejected, other) = split_claim_rejected_fails(&outcomes);
        assert_eq!((rejected, other), (2, 0), "全部确定性拒绝 → 返 Ok 不进重试链");
    }

    #[test]
    #[allow(non_snake_case)]
    fn 分流_混合网络失败_维持Err() {
        let outcomes = vec![
            json!({"uid": "u1", "status": "fail", "claim_rejected": true}),
            json!({"uid": "u2", "status": "fail", "message": "签到提交：请求失败"}),
            json!({"uid": "u3", "status": "skip"}),
        ];
        let (rejected, other) = split_claim_rejected_fails(&outcomes);
        assert_eq!((rejected, other), (1, 1), "存在非确定性失败 → 维持 Err");
    }

    #[test]
    fn 分流_无fail_零值() {
        let outcomes = vec![
            json!({"uid": "u1", "status": "success"}),
            json!({"uid": "u2", "status": "already"}),
            json!({"uid": "u3", "status": "skip"}),
        ];
        assert_eq!(split_claim_rejected_fails(&outcomes), (0, 0));
        // fail 但未带 claim_rejected 标记（如 detail 查询失败）→ 归其他失败
        let outcomes2 = vec![json!({"uid": "u4", "status": "fail", "message": "detail 查询：请求失败"})];
        assert_eq!(split_claim_rejected_fails(&outcomes2), (0, 1));
    }

    // ── 修复④/②：当日 kv 标记键格式（today_key 本地时区口径，跨进程共享）──

    #[test]
    fn 当日kv标记键_格式与日期内嵌() {
        let day = crate::doubao_checkin_results::today_key();
        assert_eq!(claim_rejected_key("u1", "2026-11-20"), "doubao_claim_rejected:u1:2026-11-20");
        assert_eq!(claim_submit_count_key("u1", &day), format!("doubao_claim_submit_count:u1:{day}"));
        assert_eq!(claim_pending_key("u2", &day), format!("doubao_claim_pending:u2:{day}"));
    }

    /// 当日拒绝标记 kv 往返 + 跨日自然失效（键内嵌日期，次日读取为 false）
    #[test]
    fn 当日拒绝标记_写入读取与跨日失效() {
        let d = std::env::temp_dir().join(format!("aiwork_checkin_flag_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        let day = crate::doubao_checkin_results::today_key();
        let db = crate::store::db(&d);
        assert!(!db.kv_get::<bool>(&claim_rejected_key("u1", &day)), "未标记默认 false");
        claim_rejected_mark(&d, "u1", &day);
        assert!(db.kv_get::<bool>(&claim_rejected_key("u1", &day)), "标记后当日生效");
        // 昨日键独立，不受今日标记影响（跨日自然失效口径）
        assert!(!db.kv_get::<bool>(&claim_rejected_key("u1", "2020-01-01")));
        // 计数键往返（i64）
        db.kv_set(&claim_submit_count_key("u1", &day), &2i64).unwrap();
        assert_eq!(db.kv_get::<i64>(&claim_submit_count_key("u1", &day)), 2);
        let _ = std::fs::remove_dir_all(&d);
    }
}
