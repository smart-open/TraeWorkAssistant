//! AI Work 助手业务核心（Web 化改造，自 src-tauri 平移；零 tauri/桌面依赖）。
//! 模块与桌面版 v3.5.8 一致，仅签名去 tauri 化与 vault KeyProvider 改造；
//! 平移期桌面壳 src-tauri 为只读参照，Phase 2 删除。

pub mod checkin_results;
pub mod commands;
pub mod fs_utils;
pub mod icube_auth;
pub mod jwt;
pub mod legacy_types;
pub mod models;
pub mod notify;
pub mod scheduler;
pub mod state;
pub mod store;
pub mod tasks;
pub mod vault;

pub mod api_server;
