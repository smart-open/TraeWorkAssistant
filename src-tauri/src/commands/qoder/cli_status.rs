//! Qoder CLI 状态桥（F-80 M4；R-3 裁决：CLI 无独立凭证通道，仅 status 只读）。
//!
//! `~/.qoder-cn/.qoder-app-status.json` 由 qoderclicn 主进程周期写入（writer 字段
//! 标明写入方），实测结构（2026-09-27）：
//! `{ logged_in, name, avatar_url, version, schema_version, product, snapshot_at, writer }`
//! ——不含任何 token/凭证字段（`.qoder-cn/.auth/` 亦无明文凭证，切号仍走 IDE 存储
//! 快照管线）。本桥只读解析供前端展示 CLI 登录态，绝不写回、绝不注入凭证。

use serde_json::{json, Value};

/// CLI status 文件：`~/.qoder-cn/.qoder-app-status.json`
/// macOS 适配预留：`.qoder-cn` 目录名本身跨平台同构（CLI 官方约定），仅主目录
/// 变量需分支——Windows 用 USERPROFILE，macOS 用 HOME（或统一 dirs::home_dir()）；
/// macOS 下本函数当前返回 None → available=false（fail-safe），不阻塞其他功能
fn cli_status_path() -> Option<std::path::PathBuf> {
    let home = std::env::var("USERPROFILE").ok()?;
    let home = home.trim();
    if home.is_empty() {
        return None;
    }
    Some(std::path::PathBuf::from(home).join(".qoder-cn").join(".qoder-app-status.json"))
}

/// status 原文 → 脱敏视图（白名单字段透传；schema_version 等非展示字段丢弃）
fn parse_status(raw: &str) -> Value {
    let v: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
    json!({
        "logged_in": v.get("logged_in").and_then(Value::as_bool).unwrap_or(false),
        "name": v.get("name").and_then(Value::as_str).unwrap_or(""),
        "avatar_url": v.get("avatar_url").and_then(Value::as_str).unwrap_or(""),
        "version": v.get("version").and_then(Value::as_str).unwrap_or(""),
        "product": v.get("product").and_then(Value::as_str).unwrap_or(""),
        "snapshot_at": v.get("snapshot_at").and_then(Value::as_str).unwrap_or(""),
        "writer": v.get("writer").and_then(Value::as_str).unwrap_or(""),
    })
}

/// Qoder CLI 登录状态（M4 status 只读桥；文件缺失/损坏返回 available=false + 原因）
#[tauri::command]
pub fn qoder_cli_status() -> Value {
    let Some(path) = cli_status_path() else {
        return json!({ "available": false, "reason": "无法定位用户主目录（USERPROFILE 未设置）" });
    };
    if !path.exists() {
        return json!({
            "available": false,
            "reason": "未检测到 CLI status 文件（~/.qoder-cn/.qoder-app-status.json）",
        });
    }
    match std::fs::read_to_string(&path) {
        Ok(raw) => {
            let mut out = parse_status(&raw);
            out["available"] = json!(true);
            out
        }
        Err(e) => json!({ "available": false, "reason": format!("status 文件读取失败：{e}") }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// R-3 实测样本（2026-09-27 ~/.qoder-cn/.qoder-app-status.json）
    #[test]
    fn parse_status_passes_whitelisted_fields() {
        let raw = r#"{"logged_in":true,"name":"u@x.com","avatar_url":"https://qoder.com.cn/users/a/avatars","version":"0.4.3","schema_version":1,"product":"qodercn","snapshot_at":"2026-09-27T00:59:07.593Z","writer":"main"}"#;
        let v = parse_status(raw);
        assert_eq!(v["logged_in"], json!(true));
        assert_eq!(v["name"], json!("u@x.com"));
        assert_eq!(v["version"], json!("0.4.3"));
        assert_eq!(v["product"], json!("qodercn"));
        assert_eq!(v["writer"], json!("main"));
        assert_eq!(v["snapshot_at"], json!("2026-09-27T00:59:07.593Z"));
        // schema_version 非展示字段不入视图（available 由命令层补写，亦不在解析层）
        assert!(v.get("schema_version").is_none());
        assert!(v.get("available").is_none());
    }

    #[test]
    fn parse_status_tolerates_broken_json() {
        let v = parse_status("{oops");
        assert_eq!(v["logged_in"], json!(false));
        assert_eq!(v["name"], json!(""));
    }
}
