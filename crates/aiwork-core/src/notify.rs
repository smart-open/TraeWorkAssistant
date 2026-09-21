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
//! 不阻塞主流程）；总开关 `enabled=false` 时全部静默。

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
}

fn default_true() -> bool {
    true
}

// 手工 Default：事件开关默认开，总开关默认关（未配置 = 不打扰）
impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bark_url: None,
            serverchan_sendkey: None,
            webhook_url: None,
            on_checkin_done: true,
            on_task_failed: true,
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
    push(st, &cfg, title, body, event)
}

/// 按给定配置实际推送（不检查 enabled：测试按钮在任何开关状态下都应可发）
fn push(st: &AppState, cfg: &NotifyConfig, title: &str, body: &str, event: &str) -> Value {
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
            log_result(st, "bark", event, &r);
            sent |= r.is_ok();
            result_str(&r)
        }
        None => Value::Null,
    };

    // Server酱 Turbo：form 表单 title + desp（Markdown 正文）
    let serverchan = match cfg
        .serverchan_sendkey
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(key) => {
            let r = agent
                .post(&format!("https://sctapi.ftqq.com/{key}.send"))
                .send_form(&[("title", title), ("desp", body)]);
            log_result(st, "serverchan", event, &r);
            sent |= r.is_ok();
            result_str(&r)
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
            log_result(st, "webhook", event, &r);
            sent |= r.is_ok();
            result_str(&r)
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

/// 渠道发送结果 → 前端可读字符串（"ok" / "HTTP 500" / 传输错误详情）
fn result_str(r: &Result<ureq::Response, ureq::Error>) -> Value {
    match r {
        Ok(_) => json!("ok"),
        Err(ureq::Error::Status(code, _)) => json!(format!("HTTP {code}")),
        Err(e) => json!(e.to_string()),
    }
}

/// 单渠道发送结果落应用日志（成功失败都记一行，便于排查渠道配置问题）
fn log_result(st: &AppState, channel: &str, event: &str, r: &Result<ureq::Response, ureq::Error>) {
    let msg = match r {
        Ok(_) => "ok".to_string(),
        Err(ureq::Error::Status(code, _)) => format!("HTTP {code}"),
        Err(e) => e.to_string(),
    };
    crate::fs_utils::app_log(&st.data_dir, &format!("[通知] {event}/{channel}: {msg}"));
}

// ── 命令桥入口（cmd_bridge 白名单：notify_config_get / notify_config_set / notify_test）──

/// 读取通知配置（命令 notify_config_get）
pub fn notify_config_get(st: &AppState) -> NotifyConfig {
    load_config(st)
}

/// 保存通知配置（命令 notify_config_set），返回保存后的生效值
pub fn notify_config_set(st: &AppState, config: NotifyConfig) -> Result<NotifyConfig, String> {
    save_config(st, &config)?;
    Ok(load_config(st))
}

/// 发送测试通知（命令 notify_test）：用表单传入配置直接发送，不落盘
pub fn notify_test(st: &AppState, config: NotifyConfig) -> Value {
    push(
        st,
        &config,
        "AIWork 测试通知",
        "这是一条测试通知，收到即表示该渠道配置可用。",
        "test",
    )
}
