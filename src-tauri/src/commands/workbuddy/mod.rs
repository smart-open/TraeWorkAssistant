//! WorkBuddy 应用接入（批次1）：环境检测 / 账号池 / auth 导入 / 凭证续期 / 签到 / 积分 / 设置。
//! 方案依据 docs/workbuddy-product-design.md §3.2~§3.8（M1~M5 + 命令契约）。
//!
//! ⚠ serde 命名约定：全部 snake_case，与前端 types.ts 严格对齐（同 doubao.rs 红线）。
//! 凭证红线：accessToken/refreshToken 等同密码——不进日志、不进 NDJSON、前端掩码展示。
//!
//! 拆分布局（原 workbuddy.rs 单文件机械拆分，函数逻辑零改动）：
//! - `common`：共享底层（路径/账号池/凭证库/设置读写/字段提取/脚本启动）
//! - `accounts`：环境检测与账号池 CRUD、凭证续期、导入导出
//! - `checkin`：签到 / 成长 / 定时任务 / UI 点击兜底 / 启动补签
//! - `credits`：积分余额 / 快照回退用量 / 官方请求用量 / 活动信息
//! - `cli`：CodeBuddy CLI 切号桥与五重防护自动轮换
//! - `chatdata`：会话三件套备份/恢复/复制
//! - `oauth`：OAuth 扫码登录
//! - `env_reset`：环境重置 / 彻底登出

mod accounts;
mod chatdata;
mod checkin;
mod cli;
mod common;
mod credits;
mod env_reset;
mod oauth;

// 对外 API 与拆分前单文件模块完全一致（main.rs 的 commands::workbuddy::xxx 全部不变）。
pub use accounts::*;
pub use chatdata::*;
pub use checkin::*;
pub use cli::*;
pub use common::*;
pub use credits::*;
pub use env_reset::*;
pub use oauth::*;
