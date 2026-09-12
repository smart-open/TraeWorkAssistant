//! CC Switch 协同（T5.7/F-43）
//!
//! 「不自建切换器」：本机已用 CC Switch（com.ccswitch.desktop）管理
//! Claude Code / Codex 多 provider 时，把本网关的转换端点作为 provider
//! 条目注册进其 SQLite 配置（`~/.cc-switch/cc-switch.db` providers 表），
//! 由 CC Switch 负责切换与下发，本项目不重复造切换器。
//!
//! 红线：
//! - 只 upsert 自己的固定 id（`aiwork-gateway-<app_type>`），绝不读写
//!   用户其余 provider 条目；
//! - 写前整库备份到 `~/.cc-switch/backups/`（沿用 CC Switch 自身备份目录约定）；
//! - API Key 只写入 CC Switch 自己的存储（与其余 provider 同等安全边界），
//!   不进入日志、不回显前端。

use rusqlite::Connection;

/// 本项目条目的固定 id 前缀（upsert 依据）；Trae 与 WB 各一套，互不覆盖
const PROVIDER_ID_PREFIX: &str = "aiwork-gateway-";
const PROVIDER_WB_ID_PREFIX: &str = "aiwork-wb-gateway-";
const DEFAULT_MODEL: &str = "glm-5.3";
/// WB 侧条目未显式传模型时的兜底（wb_model_catalog.json 首个模型）
const DEFAULT_WB_MODEL: &str = "hy4";
/// CC Switch 数据库相对 home 的路径
const DB_REL: &str = ".cc-switch/cc-switch.db";

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CcSwitchStatus {
    pub installed: bool,
    pub db_path: String,
    pub claude_registered: bool,
    pub codex_registered: bool,
    pub wb_claude_registered: bool,
    pub wb_codex_registered: bool,
}

/// 查询 CC Switch 安装状态与两侧条目注册情况（只读；库不存在 → installed=false）
#[tauri::command]
pub fn ccswitch_status() -> CcSwitchStatus {
    let db = home_db_path();
    let mut st = CcSwitchStatus {
        installed: db.is_file(),
        db_path: db.display().to_string(),
        claude_registered: false,
        codex_registered: false,
        wb_claude_registered: false,
        wb_codex_registered: false,
    };
    if st.installed {
        if let Ok(conn) = Connection::open_with_flags(
            &db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        ) {
            for (prefix, (claude_field, codex_field)) in [
                (
                    PROVIDER_ID_PREFIX,
                    (&mut st.claude_registered, &mut st.codex_registered),
                ),
                (
                    PROVIDER_WB_ID_PREFIX,
                    (&mut st.wb_claude_registered, &mut st.wb_codex_registered),
                ),
            ] {
                for (app, registered) in [("claude", claude_field), ("codex", codex_field)] {
                    let id = format!("{}{}", prefix, app);
                    let ok = conn
                        .query_row(
                            "SELECT 1 FROM providers WHERE id = ?1",
                            rusqlite::params![id],
                            |_| Ok(()),
                        )
                        .is_ok();
                    *registered = ok;
                }
            }
        }
    }
    st
}

/// 注册/更新网关 provider 条目到 CC Switch。
/// `app_type`：claude（Anthropic 协议 /v1/messages）或 codex（Responses /v1/responses）。
/// `side`：trae（Trae 模型网关，缺省）或 wb（WB 上游网关）——两侧条目 id 不同，互不覆盖。
/// `api_key`：网关 API Key（网关未配 Key 时可空）；`model`：默认模型 id；
/// `port`：网关端口（缺省用应用设置 api_port）。
/// 返回说明文案（含备份路径；提醒重启 CC Switch 生效）。
#[tauri::command]
pub fn ccswitch_register(
    state: tauri::State<'_, crate::state::AppState>,
    app_type: String,
    side: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
    port: Option<u16>,
) -> Result<String, String> {
    let app = match app_type.as_str() {
        "claude" | "codex" => app_type.as_str(),
        other => return Err(format!("不支持的 app_type: {other}（仅 claude / codex）")),
    };
    let is_wb = match side.as_deref() {
        None | Some("trae") => false,
        Some("wb") => true,
        Some(other) => {
            return Err(format!("不支持的 side: {other}（仅 trae / wb）"));
        }
    };
    let db = home_db_path();
    if !db.is_file() {
        return Err("未检测到 CC Switch（~/.cc-switch/cc-switch.db 不存在），请先安装 CC Switch".into());
    }
    // 端口缺省与网关启动同源（gateway_settings，§8.2）：避免注册条目指向旧端口
    let port = port.unwrap_or_else(|| {
        crate::api_server::gateway_settings::load(&state.data_dir).port
    });
    let model = model
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| {
            if is_wb {
                DEFAULT_WB_MODEL.to_string()
            } else {
                DEFAULT_MODEL.to_string()
            }
        });
    let key = api_key.unwrap_or_default();

    // 红线：写前整库备份（沿用 CC Switch 自身 backups 目录）
    let backup_dir = db
        .parent()
        .map(|p| p.join("backups"))
        .ok_or("无法定位 CC Switch 目录")?;
    std::fs::create_dir_all(&backup_dir).map_err(|e| format!("创建备份目录失败: {e}"))?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let backup_path = backup_dir.join(format!("cc-switch.db.bak_aiwork_{}", ts));
    std::fs::copy(&db, &backup_path).map_err(|e| format!("备份 CC Switch 数据库失败: {e}"))?;

    let entry_id = if is_wb {
        format!("{}{}", PROVIDER_WB_ID_PREFIX, app)
    } else {
        format!("{}{}", PROVIDER_ID_PREFIX, app)
    };
    let settings_config = match app {
        "claude" => claude_settings_config(port, &model, &key),
        "codex" => codex_settings_config(port, &model, &key, is_wb),
        _ => unreachable!(),
    };
    let name = format!(
        "{}（{}）",
        if is_wb { "WorkBuddy 网关" } else { "AI Work 助手网关" },
        if app == "claude" { "Anthropic" } else { "Codex" }
    );
    let notes = format!(
        "{}本地网关注入（自动生成，可安全删除；base=http://127.0.0.1:{}，写入时间 {}）",
        if is_wb { "WorkBuddy 上游 " } else { "AI Work 助手 " },
        port,
        crate::fs_utils::now_iso()
    );
    let meta = serde_json::json!({ "commonConfigEnabled": false });
    let cfg_str = serde_json::to_string(&settings_config).map_err(|e| e.to_string())?;
    let meta_str = serde_json::to_string(&meta).map_err(|e| e.to_string())?;

    let conn = Connection::open(&db).map_err(|e| format!("打开 CC Switch 数据库失败: {e}"))?;
    // 表结构自检：providers 表不存在视为 CC Switch 版本不兼容
    let has_table: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='providers'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .map_err(|e| format!("读取 CC Switch 数据库结构失败: {e}"))?;
    if !has_table {
        return Err("CC Switch 数据库缺少 providers 表（版本不兼容），已中止写入".into());
    }

    let updated = conn
        .execute(
            "UPDATE providers SET name = ?2, settings_config = ?3, notes = ?4, meta = ?5 WHERE id = ?1",
            rusqlite::params![entry_id, name, cfg_str, notes, meta_str],
        )
        .map_err(|e| format!("更新条目失败: {e}"))?;
    if updated == 0 {
        let max_sort: i64 = conn
            .query_row("SELECT COALESCE(MAX(CAST(sort_index AS INTEGER)), -1) FROM providers", [], |r| {
                r.get(0)
            })
            .unwrap_or(-1);
        conn.execute(
            "INSERT INTO providers (id, app_type, name, settings_config, website_url, category, \
             created_at, sort_index, notes, icon, icon_color, meta, is_current, in_failover_queue, \
             cost_multiplier) VALUES (?1, ?2, ?3, ?4, ?5, 'custom', ?6, ?7, ?8, ?9, ?10, ?11, '0', '0', '1.0')",
            rusqlite::params![
                entry_id,
                app,
                name,
                cfg_str,
                "https://github.com/smart-open",
                ts * 1000, // CC Switch created_at 为毫秒时间戳
                (max_sort + 1).to_string(),
                notes,
                if app == "claude" { "anthropic" } else { "openai" },
                if app == "claude" { "#D4915D" } else { "#10A37F" },
                meta_str,
            ],
        )
        .map_err(|e| format!("插入条目失败: {e}"))?;
    }

    Ok(format!(
        "已在 CC Switch 注册「{}」provider（{} 协议 → http://127.0.0.1:{}）。\
数据库已备份至 {}。\
重启 CC Switch 后在对应应用下即可看到并切换该条目。",
        name,
        if app == "claude" { "/v1/messages" } else { "/v1/responses" },
        port,
        backup_path.display()
    ))
}

/// home 下 CC Switch 数据库路径
fn home_db_path() -> std::path::PathBuf {
    dirs_home().join(DB_REL)
}

fn dirs_home() -> std::path::PathBuf {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
}

/// claude 条目：扁平 env 结构（与 CC Switch 自定义 provider 同款）
/// Claude Code 请求 `{ANTHROPIC_BASE_URL}/v1/messages`，base 不带 /v1
fn claude_settings_config(port: u16, model: &str, key: &str) -> serde_json::Value {
    let base = format!("http://127.0.0.1:{}", port);
    serde_json::json!({
        "ANTHROPIC_BASE_URL": base,
        "ANTHROPIC_AUTH_TOKEN": key,
        "ANTHROPIC_API_KEY": key,
        "ANTHROPIC_MODEL": model,
        "ANTHROPIC_DEFAULT_SONNET_MODEL": model,
        "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME": model,
        "ANTHROPIC_DEFAULT_OPUS_MODEL": model,
        "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME": model,
        "ANTHROPIC_DEFAULT_HAIKU_MODEL": model,
        "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME": model,
        "CLAUDE_CODE_SUBAGENT_MODEL": model,
    })
}

/// codex 条目：auth + config.toml（wire_api=responses，直连 /v1/responses）
fn codex_settings_config(port: u16, model: &str, key: &str, is_wb: bool) -> serde_json::Value {
    let toml = format!(
        "model_provider = \"{provider_id}\"\n\
         model = \"{model}\"\n\
         model_reasoning_effort = \"high\"\n\
         disable_response_storage = true\n\
         \n\
         [model_providers.{provider_id}]\n\
         name = \"{provider_name}\"\n\
         base_url = \"http://127.0.0.1:{port}/v1\"\n\
         wire_api = \"responses\"\n\
         requires_openai_auth = true\n",
        provider_id = if is_wb { "aiwork-wb" } else { "aiwork" },
        provider_name = if is_wb { "WorkBuddy 网关" } else { "AI Work 助手网关" },
        model = model,
        port = port,
    );
    serde_json::json!({
        "auth": { "OPENAI_API_KEY": if key.is_empty() { "aiwork-local".to_string() } else { key.to_string() } },
        "config": toml,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_config_has_base_and_model_mapping() {
        let cfg = claude_settings_config(8899, "glm-5.3", "sk-test");
        assert_eq!(cfg["ANTHROPIC_BASE_URL"], serde_json::json!("http://127.0.0.1:8899"));
        assert_eq!(cfg["ANTHROPIC_DEFAULT_OPUS_MODEL"], serde_json::json!("glm-5.3"));
        assert_eq!(cfg["ANTHROPIC_AUTH_TOKEN"], serde_json::json!("sk-test"));
        let s = cfg.to_string();
        assert!(!s.contains("/v1\""), "base 不带 /v1（Claude Code 自行拼接 /v1/messages）");
    }

    #[test]
    fn codex_config_toml_has_wire_api_and_base_v1() {
        let cfg = codex_settings_config(8899, "hy4", "", false);
        assert_eq!(cfg["auth"]["OPENAI_API_KEY"], serde_json::json!("aiwork-local"), "空 Key 用占位");
        let toml = cfg["config"].as_str().unwrap();
        assert!(toml.contains("base_url = \"http://127.0.0.1:8899/v1\""));
        assert!(toml.contains("wire_api = \"responses\""));
        assert!(toml.contains("model = \"hy4\""));
        assert!(toml.contains("model_provider = \"aiwork\""));
    }

    #[test]
    fn wb_codex_config_uses_wb_provider_id() {
        let cfg = codex_settings_config(8899, "hy4", "", true);
        let toml = cfg["config"].as_str().unwrap();
        assert!(toml.contains("model_provider = \"aiwork-wb\""), "WB 侧 provider id 独立");
        assert!(toml.contains("WorkBuddy 网关"));
        assert!(!toml.contains("model_provider = \"aiwork\"\n"), "不得回落到 Trae 侧 provider id");
    }

    #[test]
    fn empty_key_allowed_for_claude() {
        let cfg = claude_settings_config(8899, "m", "");
        assert_eq!(cfg["ANTHROPIC_AUTH_TOKEN"], serde_json::json!(""));
    }
}
