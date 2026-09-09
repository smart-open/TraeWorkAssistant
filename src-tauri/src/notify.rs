//! 系统通知封装（tauri-plugin-notification，Rust 侧）。
//!
//! 用于托盘操作、后台任务（签到完成 / API 服务启停）等无窗口交互场景的结果反馈。

use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;

/// 发送系统通知；失败静默（通知不可用不应影响主流程）
pub fn notify(app: &AppHandle, title: &str, body: &str) {
    let _ = app.notification().builder().title(title).body(body).show();
}
