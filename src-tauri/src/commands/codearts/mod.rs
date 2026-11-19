//! CodeArts（华为云码道，国内站）命令域。
//!
//! 全部命令 serde 参数 snake_case（Tauri 默认），与前端 src/lib/tauri.ts 对齐。
//! 模块路径 `commands::codearts::*` 保持命令名扁平（re-export 不改命令路径）。

pub mod accounts;
pub mod checkin;
pub mod common;
pub mod credits;
pub mod oauth;

pub use accounts::*;
pub use checkin::*;
pub use common::*;
pub use credits::*;
pub use oauth::*;
