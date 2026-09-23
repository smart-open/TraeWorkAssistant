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

// ── F-19 通知渠道（Trae/Buddy 全平台共用，配置存 app Settings） ─────────────

/// 通知事件类别：用于按事件开关过滤推送（总开关之后二级过滤）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyEvent {
    /// 签到完成（一键签到/补签结果）
    CheckinDone,
    /// 调度任务失败（应用内调度器任务返回 Err）
    TaskFail,
    /// 其他手动操作结果（设备标识重置、测试等，不受事件开关限制）
    Other,
}

/// 可选通知渠道配置（来自 app Settings；None/空 = 关闭该渠道）
#[derive(Debug, Default, Clone)]
pub struct NotifyChannels {
    /// Bark 推送地址（https://api.day.app/<key>）
    pub bark_url: Option<String>,
    /// 通用 Webhook 地址（POST JSON，兼容企业微信群机器人等自建端）
    pub wechat_webhook: Option<String>,
    /// Server酱 SendKey（https://sctapi.ftqq.com/<sendkey>.send）
    pub serverchan_sendkey: Option<String>,
}

impl NotifyChannels {
    /// 三渠道均未配置
    pub fn is_empty(&self) -> bool {
        [self.bark_url.as_deref(), self.wechat_webhook.as_deref(), self.serverchan_sendkey.as_deref()]
            .iter()
            .all(|c| c.map_or(true, |s| s.trim().is_empty()))
    }
}

/// Bark：POST JSON 到 <server>/push（device_key 取地址末段；零新增依赖，ureq 同步调用）
pub fn notify_bark(bark_url: &str, title: &str, body: &str) -> Result<(), String> {
    let base = bark_url.trim().trim_end_matches('/');
    if !base.starts_with("https://") {
        return Err("Bark 推送地址需为 https://".into());
    }
    let device_key = base.rsplit('/').next().unwrap_or("").to_string();
    if device_key.is_empty() || !device_key.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err("Bark 地址末段（Device Key）格式不正确".into());
    }
    let payload = serde_json::json!({
        "device_key": device_key,
        "title": title,
        "body": body,
        "group": "AIWorkAssistant",
    });
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(10))
        .build();
    agent
        .post(&format!("{base}/push"))
        .set("Content-Type", "application/json; charset=utf-8")
        .send_string(&payload.to_string())
        .map_err(|e| format!("Bark 请求失败: {e}"))?;
    Ok(())
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
    if let Some(bark) = channels.bark_url.as_deref().filter(|s| !s.trim().is_empty()) {
        if let Err(e) = notify_bark(bark, title, body) {
            crate::fs_utils::app_log(data_dir, &format!("通知渠道(Bark)失败: {e}"));
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 三渠道空判定：默认全空；任一渠道有值（含空白包裹）即非空
    #[test]
    fn channels_empty_detection() {
        assert!(NotifyChannels::default().is_empty());
        assert!(!NotifyChannels { bark_url: Some(" https://api.day.app/k ".into()), ..Default::default() }.is_empty());
        assert!(!NotifyChannels { serverchan_sendkey: Some("SCT123".into()), ..Default::default() }.is_empty());
        assert!(!NotifyChannels { wechat_webhook: Some("https://qyapi.weixin.qq.com/x".into()), ..Default::default() }.is_empty());
    }

    /// Bark 地址校验（错误路径，不触网）：协议 / 空 Key / 非 alnum Key 均拒绝
    #[test]
    fn bark_url_validation_rejects_bad_input() {
        assert!(notify_bark("http://api.day.app/abc123", "t", "b").is_err(), "非 https 拒绝");
        assert!(notify_bark("https://api.day.app/", "t", "b").is_err(), "空 Key 拒绝");
        assert!(notify_bark("https://api.day.app/ab c!/", "t", "b").is_err(), "非法字符 Key 拒绝");
    }

    /// 事件门控映射完整（Other 不受事件开关限制，在 push_notify 层处理）
    #[test]
    fn notify_event_is_copy_and_distinct() {
        let e = NotifyEvent::CheckinDone;
        let f = e;
        assert!(e == f, "Copy 语义");
        assert!(NotifyEvent::TaskFail != NotifyEvent::Other);
    }

    // ── 集成测试：notify_all 渠道分发 × app_log 落盘 ─────────────────────────
    // 全部走「校验失败 / 本地不可达」路径，不依赖外网；用 app.log 作为发送尝试的可观测信号。

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("twa_notify_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn read_log(dir: &std::path::Path) -> String {
        std::fs::read_to_string(dir.join("logs").join("app.log")).unwrap_or_default()
    }

    /// 空渠道：is_empty 短路，既不触网也不写日志
    #[test]
    fn notify_all_empty_channels_no_log_no_panic() {
        let dir = tmp_dir("empty");
        notify_all(None, &dir, "标题", "内容", &NotifyChannels::default());
        assert!(!dir.join("logs").join("app.log").exists(), "空渠道不应产生任何日志");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 三渠道均配置无效值：分发到每个渠道 → 格式校验失败 → app_log 各落一条失败记录（不触网）
    #[test]
    fn notify_all_invalid_channels_all_logged() {
        let dir = tmp_dir("invalid");
        let ch = NotifyChannels {
            bark_url: Some("http://api.day.app/abc".into()),          // 非 https 拒绝
            wechat_webhook: Some("ftp://qyapi.example.com/x".into()), // 非 https 拒绝
            serverchan_sendkey: Some("SCT abc!".into()),              // 含空格/非法字符拒绝
        };
        notify_all(None, &dir, "标题", "内容", &ch);
        let log = read_log(&dir);
        assert!(log.contains("通知渠道(Bark)失败"), "log: {log}");
        assert!(log.contains("通知渠道(企业微信)失败"), "log: {log}");
        assert!(log.contains("通知渠道(Server酱)失败"), "log: {log}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 合法格式 + 本地不可达端点（127.0.0.1:1 保留端口，回环立即拒连）：
    /// 验证校验通过后的完整链路——请求真实发起 → 失败 → app_log 落盘
    #[test]
    fn notify_all_valid_format_unreachable_bark_logged() {
        let dir = tmp_dir("unreachable");
        let ch = NotifyChannels { bark_url: Some("https://127.0.0.1:1/abcKey123".into()), ..Default::default() };
        notify_all(None, &dir, "标题", "内容", &ch);
        let log = read_log(&dir);
        assert!(log.contains("通知渠道(Bark)失败"), "合法格式 Bark 请求失败应落日志: {log}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
