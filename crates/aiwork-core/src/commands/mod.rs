//! 管理命令 impl 层（自 src-tauri/src/commands 平移，T5）：
//! `#[tauri::command]` 壳不复存在，直接暴露为 core 函数；
//! `State<AppState>` → `&AppState`，桌面事件推送改回调/广播（签到进度经 server 桥到 SSE）。
//! 命令名 → 函数的白名单分发表见 aiwork-server 的 admin/cmd_bridge。

pub mod accounts;
pub mod api_server;
pub mod checkin;
pub mod misc;
pub mod oauth;
pub mod wb_config;
pub mod workbuddy;
