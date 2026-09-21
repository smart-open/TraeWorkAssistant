//! WorkBuddy 命令 impl 层（自 src-tauri/src/commands/workbuddy 平移，T5）。
//! 裁剪桌面链：cli（CodeBuddy 轮换）/ chatdata（会话备份）/ env_reset（本机环境重置）不平移。
//! 凭证红线：accessToken/refreshToken 等同密码——不进日志、不进 NDJSON、前端掩码展示。

pub mod accounts;
pub mod checkin;
pub mod common;
pub mod credits;
pub mod groups;
pub mod oauth;

// 对外 API 与拆分前单文件模块一致（命令桥按 commands::workbuddy::xxx 分发）。
pub use accounts::*;
pub use checkin::*;
pub use common::*;
pub use credits::*;
pub use groups::*;
pub use oauth::*;
