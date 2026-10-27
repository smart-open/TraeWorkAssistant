//! Custom 命令域（issue #149 P2，仿 commands/qoder/ 拆分）。
//!
//! - `sessions`：会话明细查询（网关请求级本地流水 `custom_sessions`，
//!   由 `record_usage_custom` 按请求写入；复用 Qoder 侧聚合核心与契约类型）

mod sessions;

pub use sessions::*;
