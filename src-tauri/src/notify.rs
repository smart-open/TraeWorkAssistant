//! 系统通知封装（tauri-plugin-notification，Rust 侧）+ 失败通知渠道分发（F-19，批次3 T3.6）。
//!
//! 用于托盘操作、后台任务（签到完成 / API 服务启停）等无窗口交互场景的结果反馈。
//! F-19 渠道扩展：企业微信群机器人 webhook / Server酱——作为桌面通知之外的可选渠道，
//! 由 workbuddy_settings.json 配置；渠道失败静默记日志，绝不影响主流程。

use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;

/// 发送系统通知；失败静默（通知不可用不应影响主流程）
pub fn notify(app: &AppHandle, title: &str, body: &str) {
    let _ = app.notification().builder().title(title).body(body).show();
}

// ── F-19 通知渠道 ─────────────────────────────────────────────────────────

/// 可选通知渠道配置（来自 workbuddy_settings.json；None/空 = 关闭该渠道）
#[derive(Debug, Default, Clone)]
pub struct NotifyChannels {
    /// 企业微信群机器人 webhook（https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=xxx）
    pub wechat_webhook: Option<String>,
    /// Server酱 SendKey（https://sctapi.ftqq.com/<sendkey>.send）
    pub serverchan_sendkey: Option<String>,
}

impl NotifyChannels {
    /// 两渠道均未配置
    pub fn is_empty(&self) -> bool {
        self.wechat_webhook.as_deref().map_or(true, |s| s.trim().is_empty())
            && self.serverchan_sendkey.as_deref().map_or(true, |s| s.trim().is_empty())
    }
}

/// 企业微信群机器人：POST text 消息（零新增依赖，ureq 同步调用）
pub fn notify_wechat_webhook(webhook: &str, title: &str, body: &str) -> Result<(), String> {
    let url = webhook.trim();
    if !url.starts_with("https://") {
        return Err("企业微信 webhook 需为 https:// 地址".into());
    }
    let payload = serde_json::json!({
        "msgtype": "text",
        "text": { "content": format!("{title}\n{body}") },
    });
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(10))
        .build();
    agent
        .post(url)
        .set("Content-Type", "application/json")
        .send_string(&payload.to_string())
        .map_err(|e| format!("企业微信 webhook 请求失败: {e}"))?;
    Ok(())
}

/// Server酱：表单提交 title/desp（Turbo 版 sctapi 端点）
pub fn notify_serverchan(sendkey: &str, title: &str, body: &str) -> Result<(), String> {
    let key = sendkey.trim();
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err("Server酱 SendKey 格式不正确".into());
    }
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(10))
        .build();
    // desp 支持 markdown：换行转两空格+换行保持段落
    let desp = body.replace('\n', "  \n");
    agent
        .post(&format!("https://sctapi.ftqq.com/{key}.send"))
        .send_form(&[("title", title), ("desp", desp.as_str())])
        .map_err(|e| format!("Server酱 请求失败: {e}"))?;
    Ok(())
}

/// 统一分发入口：桌面通知（有 AppHandle 时）+ 可选渠道；渠道失败仅记日志。
pub fn notify_all(app: Option<&AppHandle>, data_dir: &std::path::Path, title: &str, body: &str, channels: &NotifyChannels) {
    if let Some(app) = app {
        notify(app, title, body);
    }
    if channels.is_empty() {
        return;
    }
    if let Some(webhook) = channels.wechat_webhook.as_deref().filter(|s| !s.trim().is_empty()) {
        if let Err(e) = notify_wechat_webhook(webhook, title, body) {
            crate::fs_utils::app_log(data_dir, &format!("通知渠道(企业微信)失败: {e}"));
        }
    }
    if let Some(sendkey) = channels.serverchan_sendkey.as_deref().filter(|s| !s.trim().is_empty()) {
        if let Err(e) = notify_serverchan(sendkey, title, body) {
            crate::fs_utils::app_log(data_dir, &format!("通知渠道(Server酱)失败: {e}"));
        }
    }
}
