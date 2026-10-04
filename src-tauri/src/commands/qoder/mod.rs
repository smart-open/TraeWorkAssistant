//! Qoder 命令域（F-80 M1，仿 commands/workbuddy/ 拆分）。
//!
//! ⚠ serde 命名约定：全部 snake_case，与前端 types.ts 严格对齐（同 doubao.rs 红线）。
//! 凭证红线：accessToken/PAT 等同密码——不进日志、不进 NDJSON、前端掩码展示。
//!
//! - `common`：账号池/设置读写、环境检测
//! - `accounts`：账号列表/改名/移除/PAT 导入（M1 最可靠凭证通道，M0 R-3 侦察结论）
//! - `checkin`：签到（NDJSON 管线）/ 签到结果 / 定时任务 / 启动补签
//! - `cli_status`：CLI 状态只读桥（本机 CLI 登录账号数据源）
//! - `credits`：积分查询 / 快照时序
//! - `data_io`：账号池导出/导入（M4，对照 WorkBuddy F-46 扩展同语义；
//!   含凭证导出走 AES-256-GCM 加密信封，kdf 字段记录派生轮数）
//! - `env_reset`：环境重置/彻底登出（M4，对照 WorkBuddy F-14 同语义）
//! - `groups`：账号分组（定义存 kv("qoder_groups")，成员落账号 group_id；
//!   defs 读改写持 qoder_pool_lock 互斥，与导入的分组合并互斥）
//! - `ide_store`：IDE/Work 客户端本地存储发现导入（含 WAL 残留检测）
//! - `oauth`：设备授权登录（浏览器授权 + 轮询导入；qoder-oauth-progress/done 事件）

mod accounts;
mod checkin;
mod cli_status;
mod common;
mod credits;
mod data_io;
mod env_reset;
mod groups;
mod ide_store;
mod oauth;

pub use accounts::*;
pub use checkin::*;
pub use cli_status::*;
pub use common::*;
pub use credits::*;
pub use data_io::*;
pub use env_reset::*;
pub use groups::*;
pub use ide_store::*;
pub use oauth::*;
