//! 通知渠道模块（Phase 3 T11）：Bark / Server酱 / 通用 webhook 推送，替代桌面系统通知。
//!
//! ## 配置
//!
//! kv `notify_config`（全字段 `#[serde(default)]` 向前兼容；键缺失/损坏回退 Default）。
//!
//! ## 事件接入
//!
//! - `checkin`：签到完成（调度器 trae/wb 签到任务 + 手动签到，受 `on_checkin_done` 控制）
//! - `task_failed`：调度任务失败（全部任务，受 `on_task_failed` 控制）
//! - `test`：设置页「发送测试」（用表单传入配置直接发送，不落盘）
//!
//! ## 发送语义
//!
//! 三渠道顺序推送：未配置跳过、单渠道失败仅记日志（通知是旁路，不 panic、
//! 不阻塞主流程）；总开关 `enabled=false` 时全部静默。各渠道结果落盘
//! kv `notify_last_result`（issue #222 渠道回显，设置页「最近推送」展示）。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::state::AppState;

/// 通知配置（设置页「通知渠道」卡片；字段 snake_case 与前端对齐）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotifyConfig {
    /// 总开关：关闭时一切通知静默（默认关，未配置不打扰）
    #[serde(default)]
    pub enabled: bool,
    /// Bark 推送地址（形如 https://api.day.app/<key>）
    #[serde(default)]
    pub bark_url: Option<String>,
    /// Server酱 SendKey（sct.ftqq.com 获取）
    #[serde(default)]
    pub serverchan_sendkey: Option<String>,
    /// 通用 webhook 地址（POST JSON：event/title/body/ts）
    #[serde(default)]
    pub webhook_url: Option<String>,
    /// 签到完成时通知（默认开）
    #[serde(default = "default_true")]
    pub on_checkin_done: bool,
    /// 调度任务失败时通知（默认开）
    #[serde(default = "default_true")]
    pub on_task_failed: bool,
    /// 定时调度任务完成推送（issue #222，默认关——成功推送易耗尽 Server酱
    /// 免费档每日 5 条；独立开关，仅签到/成长奖励任务白名单生效）
    #[serde(default)]
    pub on_task_success: bool,
}

fn default_true() -> bool {
    true
}

// 手工 Default：事件开关默认开（task_success 例外默认关），总开关默认关（未配置 = 不打扰）
impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bark_url: None,
            serverchan_sendkey: None,
            webhook_url: None,
            on_checkin_done: true,
            on_task_failed: true,
            on_task_success: false,
        }
    }
}

/// 读取通知配置（kv `notify_config`；缺失/损坏回退 Default）
pub fn load_config(st: &AppState) -> NotifyConfig {
    crate::store::db(&st.data_dir).kv_get("notify_config")
}

/// 保存通知配置
fn save_config(st: &AppState, cfg: &NotifyConfig) -> Result<(), String> {
    crate::store::db(&st.data_dir).kv_set("notify_config", cfg)
}

/// 推送一条通知（调度器/签到接入点调用；旁路永不报错），返回各渠道结果：
/// `{ "sent": bool, "reason"?: string, "bark"|"serverchan"|"webhook": "ok"|失败原因|null }`
/// 各渠道 null = 未配置；`sent` = 至少一个渠道发送成功。
pub fn send(st: &AppState, title: &str, body: &str, event: &str) -> Value {
    let cfg = load_config(st);
    if !cfg.enabled {
        return json!({ "sent": false, "reason": "通知未启用" });
    }
    push(&st.data_dir, &cfg, title, body, event, "push")
}

/// 后台线程推送（issue #222 移植，对齐 main push_notify_bg）：四渠道 HTTP 最坏
/// ~40s，调度 tick 循环内调用不阻塞调度循环；渠道失败静默，绝不影响调用方。
/// 线程创建失败（极端资源耗尽）落日志放弃本轮推送
pub fn send_bg(st: &AppState, title: String, body: String, event: &'static str) {
    let st_bg = st.clone();
    let spawned = std::thread::Builder::new()
        .name("notify-bg".into())
        .spawn(move || {
            let _ = send(&st_bg, &title, &body, event);
        });
    if spawned.is_err() {
        // title/body 已被闭包 move，无法同步重发；建线程失败属极端资源耗尽，落日志放弃
        crate::fs_utils::app_log(&st.data_dir, "[notify] 后台推送线程创建失败，本轮推送放弃");
    }
}

/// 按给定配置实际推送（不检查 enabled：测试按钮在任何开关状态下都应可发）。
/// `src` 标记推送来源（"push" 真实推送 / "test" 设置页测试），随渠道结果落盘
/// 「最近推送」面板区分展示
fn push(
    data_dir: &std::path::Path,
    cfg: &NotifyConfig,
    title: &str,
    body: &str,
    event: &str,
    src: &str,
) -> Value {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(10))
        .build();
    let mut sent = false;

    // Bark：POST JSON {title, body, group}
    let bark = match cfg.bark_url.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(url) => {
            let r = agent
                .post(url)
                .set("Content-Type", "application/json")
                .send_string(&json!({ "title": title, "body": body, "group": "AIWork" }).to_string());
            let res = to_result(&r, "Bark");
            log_result(data_dir, "bark", event, &res);
            record_channel_result(data_dir, "bark", src, &res);
            sent |= r.is_ok();
            result_str(&res)
        }
        None => Value::Null,
    };

    // Server酱 Turbo：form 表单 title + desp（Markdown 正文）。
    // HTTP 200 仍可能业务失败（issue #222：SendKey 无效/免费档每日 5 条超限），
    // 必须查响应体业务码才能给出可诊断错误
    let serverchan = match cfg
        .serverchan_sendkey
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(key) => {
            let desp = body.replace('\n', "  \n");
            let res = match agent
                .post(&format!("https://sctapi.ftqq.com/{key}.send"))
                .send_form(&[("title", title), ("desp", desp.as_str())])
            {
                Ok(resp) => match resp.into_string() {
                    Ok(text) => parse_serverchan_resp(&text).map_err(|e| format!("Server酱 {e}")),
                    Err(e) => Err(format!("Server酱 响应读取失败: {e}")),
                },
                // HTTP 4xx/5xx：错误体同样含业务码（如超限），解析后带出具体原因
                Err(ureq::Error::Status(code, resp)) => {
                    let text = resp.into_string().unwrap_or_default();
                    parse_serverchan_resp(&text).map_err(|e| format!("Server酱 HTTP {code}: {e}"))
                }
                Err(e) => Err(format!("Server酱 {}", ureq_err_desc(&e))),
            };
            log_result(data_dir, "serverchan", event, &res);
            record_channel_result(data_dir, "serverchan", src, &res);
            sent |= res.is_ok();
            result_str(&res)
        }
        None => Value::Null,
    };

    // 通用 webhook：POST JSON {event, title, body, ts}
    let webhook = match cfg.webhook_url.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(url) => {
            let r = agent
                .post(url)
                .set("Content-Type", "application/json")
                .send_string(
                    &json!({
                        "event": event,
                        "title": title,
                        "body": body,
                        "ts": chrono::Utc::now().to_rfc3339(),
                    })
                    .to_string(),
                );
            let res = to_result(&r, "webhook");
            log_result(data_dir, "webhook", event, &res);
            record_channel_result(data_dir, "webhook", src, &res);
            sent |= r.is_ok();
            result_str(&res)
        }
        None => Value::Null,
    };

    if sent {
        json!({ "sent": true, "bark": bark, "serverchan": serverchan, "webhook": webhook })
    } else {
        json!({
            "sent": false,
            "reason": "未配置任何渠道或全部发送失败",
            "bark": bark,
            "serverchan": serverchan,
            "webhook": webhook,
        })
    }
}

/// ureq 调用结果 → 渠道统一 Result<(), String>（issue #222 渠道回显口径）
fn to_result(r: &Result<ureq::Response, ureq::Error>, label: &str) -> Result<(), String> {
    match r {
        Ok(_) => Ok(()),
        Err(e) => Err(format!("{label} {}", ureq_err_desc(e))),
    }
}

/// ureq 错误 → 可落日志/回显面板的脱敏文案（2026-11-21 审查修复）：
/// 传输层错误 Display 以完整 URL 开头——Bark key / Server酱 SendKey /
/// webhook token 嵌在 URL 中，原样进 app.log 与「最近推送」面板即泄漏凭证
///（与「凭证不入日志」红线同源）。仅保留错误类别与消息，不回显 URL
fn ureq_err_desc(e: &ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, _) => format!("HTTP {code}"),
        ureq::Error::Transport(t) => match t.message() {
            Some(m) if !m.trim().is_empty() => format!("传输失败({}): {m}", t.kind()),
            _ => format!("传输失败({})", t.kind()),
        },
    }
}

/// 解析 Server酱 响应体（issue #222）：sctapi 成功返回 `{"code":0,...}`，业务失败
/// （如 SendKey 无效 / 免费档每日 5 条超限）也带 code/message——HTTP 200 与 4xx 均
/// 可能出现，必须查 body 才能给出可诊断的错误信息。错误文本不带「Server酱」前缀，
/// 由调用方按 HTTP 语境统一加前缀
fn parse_serverchan_resp(body: &str) -> Result<(), String> {
    let v: serde_json::Value = serde_json::from_str(body.trim())
        .map_err(|_| "响应非 JSON（网关或网络异常）".to_string())?;
    let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code == 0 {
        return Ok(());
    }
    let msg = v.get("message").and_then(|m| m.as_str()).unwrap_or("").trim();
    if msg.is_empty() {
        Err(format!("返回业务码 {code}"))
    } else {
        Err(format!("返回业务码 {code}: {msg}"))
    }
}

/// 渠道发送结果 → 前端可读字符串（"ok" / "HTTP 500" / 传输错误详情）
fn result_str(res: &Result<(), String>) -> Value {
    match res {
        Ok(()) => json!("ok"),
        Err(e) => json!(e),
    }
}

/// 单渠道发送结果落应用日志（成功失败都记一行，便于排查渠道配置问题）
fn log_result(data_dir: &std::path::Path, channel: &str, event: &str, res: &Result<(), String>) {
    let msg = match res {
        Ok(()) => "ok".to_string(),
        Err(e) => e.clone(),
    };
    crate::fs_utils::app_log(data_dir, &format!("[通知] {event}/{channel}: {msg}"));
}

/// 渠道推送结果落盘（issue #222 可观测性）：kv `notify_last_result` 按渠道记录
/// 最近一次推送的时间/成败/错误/来源。此前渠道失败只静默写 app.log，通知面板上看
/// 不到「为什么没推」（如 Server酱免费档每日 5 条超限）；记录写失败不影响推送主
/// 流程（落盘失败留痕 app.log，可观测性功能自身静默停更将无线索）。
/// docker 适配：无 tauri 全局句柄/事件总线，实时刷新由前端打开面板时经
/// notify_last_result_get 拉取 + 通知配置保存/测试后的轮询兜底
pub(crate) fn record_channel_result(
    data_dir: &std::path::Path,
    channel: &str,
    src: &str,
    res: &Result<(), String>,
) {
    let now = chrono::Utc::now().timestamp_millis();
    let entry = match res {
        Ok(()) => serde_json::json!({ "ts": now, "ok": true, "src": src }),
        Err(e) => serde_json::json!({ "ts": now, "ok": false, "err": e, "src": src }),
    };
    let updated = crate::store::db(data_dir).kv_update("notify_last_result", |doc| {
        // kv 缺键/损坏时 doc 可能为 Null 等非 object 值——非 object 一律重置为
        // 单条目新对象（原实现静默不插入会导致记录停更无线索）
        if !doc.is_object() {
            *doc = serde_json::json!({});
        }
        if let Some(obj) = doc.as_object_mut() {
            obj.insert(channel.to_string(), entry);
        }
        Ok::<_, String>(true)
    });
    if let Err(e) = updated {
        crate::fs_utils::app_log(
            data_dir,
            &format!("通知渠道({channel})最近推送结果落盘失败: {e}"),
        );
    }
}

/// 账号状态跃迁告警（issue #220 P3）：网关 Forbidden / 积分保护冷却等状态变化
/// → 渠道推送（语义归入失败类，走 `on_task_failed` 开关过滤）。冷却防重：
/// 同一账号同一事件 24h 仅推一次（kv `notify_state:<account>:<event>` 存最近推送
/// 毫秒时刻，跨重启仍生效）——上游反复报错不会刷屏。
/// 推送为旁路后台线程执行（ureq 推送最坏 ~30s，不阻塞网关 Fatal/调度循环）；
/// 冷却后置（审查修复）：推送前写 pending 在途标记（防并发重复推送；TTL 120s
/// 兜底进程中断后标记泄漏），推送结束后（无论渠道成败，统一口径「推送至少尝试过
/// 一次即进冷却」）再写冷却标记覆盖 pending；被开关过滤/总开关关闭未发起推送时
/// 不写冷却，下轮状态跃迁重试。
pub fn notify_account_state(
    data_dir: &std::path::Path,
    account_id: &str,
    event_kind: &str,
    title: &str,
    body: &str,
) {
    const COOLDOWN_MS: i64 = 24 * 3600 * 1000;
    // pending 标记保留既有 ts（审查修复 2026-11-20）：整值覆盖会抹掉上次推送成功
    // 时刻——被开关过滤的尝试不写冷却标记（值停留在 pending 形态），24h 冷却记忆
    // 丢失 → 之后每次状态跃迁（间隔 >120s）都重复推送
    const PENDING_TTL_MS: i64 = 120 * 1000;
    let key = format!("notify_state:{account_id}:{event_kind}");
    let db = crate::store::db(data_dir);
    let now = chrono::Utc::now().timestamp_millis();
    // 审查修复（2026-11-21，竞态收口）：冷却/pending 判定与 pending 写入改在
    // kv_update 事务内完成——原 kv_get→kv_set 两步非原子，两线程同账号同事件
    // 并发跃迁会双双读到「无 pending/未冷却」→ 双写 pending 双推（注释宣称的
    // 「防并发重复推送」实际不成立）。kv_update 走 BEGIN IMMEDIATE，同键读-判-写
    // 串行化，恰好一个线程获得推送权。事务出错时不推送（宁可漏推不双推）
    let mut acquired = false;
    let mut rollback_val = serde_json::json!({});
    let outcome = db.kv_update(&key, |doc| {
        if !doc.is_object() {
            *doc = serde_json::json!({});
        }
        let obj = doc.as_object_mut().expect("已归一为 object");
        if let Some(ts) = obj.get("ts").and_then(serde_json::Value::as_i64) {
            if now - ts < COOLDOWN_MS {
                return Ok::<_, String>(false);
            }
        }
        if let Some(pa) = obj.get("pending_at").and_then(serde_json::Value::as_i64) {
            if now - pa < PENDING_TTL_MS {
                return Ok(false);
            }
        }
        // spawn 失败回滚快照：写入前形态剔除 pending_at（保留旧冷却 ts），
        // 下次状态跃迁可正常重试
        let mut snap = obj.clone();
        snap.remove("pending_at");
        rollback_val = serde_json::Value::Object(snap);
        let mut pending = serde_json::json!({ "pending_at": now });
        if let Some(ts) = obj.get("ts") {
            pending["ts"] = ts.clone();
        }
        *doc = pending;
        acquired = true;
        Ok(true)
    });
    // 事务出错（序列化/INSERT/COMMIT 失败，事务已 ROLLBACK）时 acquired 虽在闭包
    // 内置位，但 pending 标记并未持久化——同账号并发跃迁的另一线程同样会走到
    // 「事务失败但 acquired=true」→ 双推。宁可漏推不双推：落库失败一并视为未获权
    if let Err(e) = &outcome {
        crate::fs_utils::app_log(data_dir, &format!("账号状态告警 pending 标记写入失败: {e}"));
    }
    if !acquired || outcome.is_err() {
        return;
    }
    let dir = data_dir.to_path_buf();
    let title = title.to_string();
    let body = body.to_string();
    let key_bg = key.clone();
    let spawn = std::thread::Builder::new()
        .name("notify-account-state".to_string())
        .spawn(move || {
            let cfg: NotifyConfig = crate::store::db(&dir).kv_get("notify_config");
            // 语义归入失败类：总开关或失败开关关闭 → 不推送不进冷却（审查修复：
            // 冷却标记原先在此分支之外无条件写入，开关关闭的跃迁也被盖 24h 冷却，
            // 与文档注释「不推送不进冷却」矛盾——现移入分支内 push 之后）
            if cfg.enabled && cfg.on_task_failed {
                push(&dir, &cfg, &title, &body, "account_state", "push");
                // 推送至少尝试过一次即进冷却（统一口径，无论渠道成败）；开关关闭
                // 未发起推送则不写冷却，下轮状态跃迁重试
                if let Err(e) = crate::store::db(&dir).kv_set(
                    &key_bg,
                    &serde_json::json!({ "ts": chrono::Utc::now().timestamp_millis() }),
                ) {
                    crate::fs_utils::app_log(&dir, &format!("账号状态告警冷却标记写入失败: {e}"));
                }
            }
        });
    if let Err(e) = spawn {
        // 后台线程起不来：回滚 pending 标记——写回早段暂存的 prev 原值（保留旧
        // 冷却 ts、剔除 pending 在途标记）。审查修复：原实现写 json!({"ts": now})
        // 会把「一次推送尝试都没发生」的情况写成 24h 冷却起点，下次跃迁被静默
        // 吞掉；回滚须还原到 pending 写入前的形态保证可重试
        crate::fs_utils::app_log(data_dir, &format!("账号状态告警推送线程启动失败: {e}"));
        let _ = db.kv_set(&key, &rollback_val);
    }
}

// ── 命令桥入口（cmd_bridge 白名单：notify_config_get / notify_config_set / notify_test / notify_last_result_get）──

/// 读取通知配置（命令 notify_config_get）
pub fn notify_config_get(st: &AppState) -> NotifyConfig {
    load_config(st)
}

/// 保存通知配置（命令 notify_config_set），返回保存后的生效值
pub fn notify_config_set(st: &AppState, config: NotifyConfig) -> Result<NotifyConfig, String> {
    save_config(st, &config)?;
    Ok(load_config(st))
}

/// 读取「最近推送」渠道回显（命令 notify_last_result_get，issue #222）。
/// 边界归一化（审查修复移植）：kv_get 缺键/损坏回退 Value::Null，前端
/// Object.keys(null) 会抛 TypeError——非 object 一律归一为空对象
pub fn notify_last_result_get(st: &AppState) -> Value {
    let doc: Value = crate::store::db(&st.data_dir).kv_get("notify_last_result");
    if doc.is_object() {
        doc
    } else {
        serde_json::json!({})
    }
}

/// 发送测试通知（命令 notify_test）：用表单传入配置直接发送，不落盘。
/// 渠道结果仍记入「最近推送」（src="test"，面板区分展示）
pub fn notify_test(st: &AppState, config: NotifyConfig) -> Value {
    push(
        &st.data_dir,
        &config,
        "AIWork 测试通知",
        "这是一条测试通知，收到即表示该渠道配置可用。",
        "test",
        "test",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Server酱 响应码解析（纯函数，不触网，issue #222）：code=0 成功；业务码带出
    /// message（每日 5 条超限等）；非 JSON 拒绝
    #[test]
    fn parse_serverchan_resp_codes() {
        assert!(parse_serverchan_resp(r#"{"code":0,"message":"","data":[]}"#).is_ok());
        assert!(
            parse_serverchan_resp(r#"{"code":40001,"message":"错误的SENDKEY，请检查您的SENDKEY"}"#)
                .is_err()
        );
        assert!(parse_serverchan_resp(r#"{"code":40002,"message":"每日次数超过免费上限5次"}"#).is_err());
        assert!(parse_serverchan_resp(r#"{"code":40002,"message":""}"#).is_err());
        assert!(parse_serverchan_resp("不是 JSON").is_err());
    }
}
