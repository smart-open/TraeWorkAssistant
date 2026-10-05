//! Qoder 命令域（F-80，自 main 分支 src-tauri/src/commands/qoder 平移）：
//! `#[tauri::command]` 壳删除，`State<AppState>` → `&AppState`，桌面事件推送改
//! Emitter 回调（经 server 桥到 SSE）；热重载改 `reload_pools_after_change`。
//! 桌面专属伴生件（cli_status/env_reset/ide_store/live_logins/env_check/open_ide/
//! open_work/schtasks 定时任务/startup_auto_checkin/open_in_browser）不移植——
//! Web 版调度走 tasks/scheduler.rs，浏览器授权页由前端打开。
//!
//! - `common`：账号池/设置读写
//! - `accounts`：账号列表/改名/移除/PAT 导入/手动续期
//! - `checkin`：签到（NDJSON 管线）/ 签到结果
//! - `credits`：积分查询 / 快照时序
//! - `data_io`：账号池导出/导入（含凭证走 AES-256-GCM 加密信封）
//! - `groups`：账号分组（定义存 kv("qoder_groups")，成员落账号 group_id）
//! - `oauth`：设备授权登录（qoder-oauth-progress/done 事件）

pub mod accounts;
pub mod checkin;
pub mod common;
pub mod credits;
pub mod data_io;
pub mod groups;
pub mod oauth;

pub use accounts::*;
pub use checkin::*;
pub use common::*;
pub use credits::*;
pub use data_io::*;
pub use groups::*;
pub use oauth::*;
