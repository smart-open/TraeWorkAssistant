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
    let mut st = CcSwitchStatus {
        installed: false,
        db_path: String::new(),
        claude_registered: false,
        codex_registered: false,
        wb_claude_registered: false,
        wb_codex_registered: false,
    };
    // 加固：无法定位主目录 → 视为未安装（不再兜底 "."，避免误报 cwd 下的库）
    let Some(db) = home_db_path() else {
        return st;
    };
    st.installed = db.is_file();
    st.db_path = db.display().to_string();
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
/// `api_key`：网关 API Key（未传时自动使用「CC Switch 专用」Key，复用或新建）；
/// `model`：默认模型 id；
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
    let Some(db) = home_db_path() else {
        return Err("无法定位用户主目录（USERPROFILE / HOME 均未设置），中止注册".into());
    };
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
    // 前端未显式传 Key 时使用「CC Switch 专用」Key（复用或自动创建，不限配额）：
    // 不再复用业务 Key——业务 Key 可能设了每日限额（如 100/100 已耗尽），注册后
    // 请求全被 429 拒绝，用户需手动换 Key 才能用（2026-09-20 实测反馈）
    let (key, key_note) = match api_key {
        Some(k) => (k, String::new()),
        None => (
            ensure_ccswitch_key(&state.data_dir)?,
            "网关鉴权使用「CC Switch 专用」Key（不限每日配额，可在 API 管理·子 Key 查看）。".to_string(),
        ),
    };

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
    // 整库备份需连带 WAL/SHM：SQLite 未 checkpoint 时部分数据仍在 -wal 中，
    // 只拷 .db 会得到缺提交的旧快照，恢复后丢最近写入（审查 P2）
    for suffix in ["-wal", "-shm"] {
        let side = std::path::PathBuf::from(format!("{}{}", db.display(), suffix));
        if side.is_file() {
            let side_backup = backup_dir.join(format!("cc-switch.db{}.bak_aiwork_{}", suffix, ts));
            if let Err(e) = std::fs::copy(&side, &side_backup) {
                crate::fs_utils::app_log(
                    &state.data_dir,
                    &format!("备份 CC Switch {suffix} 文件失败（忽略）: {e}"),
                );
            }
        }
    }
    // 审查：备份无限累积 → 只保留最近 10 份本工具创建的备份（文件名带 .bak_aiwork_ 标记，
    // 不触碰 CC Switch 自身备份）
    const KEEP_BACKUPS: usize = 10;
    if let Ok(entries) = std::fs::read_dir(&backup_dir) {
        let mut baks: Vec<std::path::PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.contains(".bak_aiwork_"))
                    .unwrap_or(false)
            })
            .collect();
        if baks.len() > KEEP_BACKUPS {
            // 文件名含固定宽度秒级时间戳，按名排序即按时间排序；刚创建的最新备份不会被删
            baks.sort();
            let overflow = baks.len() - KEEP_BACKUPS;
            for old in baks.iter().take(overflow) {
                if let Err(e) = std::fs::remove_file(old) {
                    crate::fs_utils::app_log(
                        &state.data_dir,
                        &format!("清理 CC Switch 旧备份失败（忽略）: {} - {e}", old.display()),
                    );
                }
            }
        }
    }

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
    // 审查：CC Switch 运行中可能持有写锁，等待 3s 而非立即报 database is locked
    conn.busy_timeout(std::time::Duration::from_millis(3000))
        .map_err(|e| format!("设置数据库等待超时失败: {e}"))?;
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
        // 列级兼容：cc-switch main（SCHEMA_VERSION=19）基线 providers 已无
        // cost_multiplier 列，固定列名 INSERT 在新库上报 no such column；旧库可能
        // 仍保留该列（可能 NOT NULL 无默认），按实际存在的列动态拼装
        let has_cost_multiplier: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('providers') WHERE name='cost_multiplier'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n > 0)
            .unwrap_or(false);
        let mut values: Vec<rusqlite::types::Value> = vec![
            entry_id.into(),
            app.to_string().into(),
            name.clone().into(),
            cfg_str.into(),
            "https://github.com/smart-open".to_string().into(),
            rusqlite::types::Value::Integer((ts * 1000) as i64), // CC Switch created_at 为毫秒时间戳
            (max_sort + 1).to_string().into(),
            notes.into(),
            (if app == "claude" { "anthropic" } else { "openai" }).to_string().into(),
            (if app == "claude" { "#D4915D" } else { "#10A37F" }).to_string().into(),
            meta_str.into(),
        ];
        let sql = if has_cost_multiplier {
            values.push("1.0".to_string().into());
            "INSERT INTO providers (id, app_type, name, settings_config, website_url, category, \
             created_at, sort_index, notes, icon, icon_color, meta, is_current, in_failover_queue, \
             cost_multiplier) VALUES (?1, ?2, ?3, ?4, ?5, 'custom', ?6, ?7, ?8, ?9, ?10, ?11, '0', '0', ?12)"
        } else {
            "INSERT INTO providers (id, app_type, name, settings_config, website_url, category, \
             created_at, sort_index, notes, icon, icon_color, meta, is_current, in_failover_queue) \
             VALUES (?1, ?2, ?3, ?4, ?5, 'custom', ?6, ?7, ?8, ?9, ?10, ?11, '0', '0')"
        };
        conn.execute(sql, rusqlite::params_from_iter(values.iter()))
            .map_err(|e| format!("插入条目失败: {e}"))?;
    }

    Ok(format!(
        "已在 CC Switch 注册「{}」provider（{} 协议 → http://127.0.0.1:{}）。\
数据库已备份至 {}。\
重启 CC Switch 后在对应应用下即可看到并切换该条目。{}",
        name,
        if app == "claude" { "/v1/messages" } else { "/v1/responses" },
        port,
        backup_path.display(),
        key_note
    ))
}

/// CC Switch 注册专用 Key 名（自动创建/复用；与业务 Key 隔离，不受业务每日配额影响）
const CC_SWITCH_KEY_NAME: &str = "CC Switch 专用";

/// 取「CC Switch 专用」Key：已有同名 Key → 复用（若被禁用则恢复启用）；
/// 无 → 新建（ck_ + 128-bit 随机 hex，不限每日配额）。
/// 持久化随 api_keys::save 落盘（内存权威副本同步刷新）；失败返回 Err 由
/// 注册流程中止——宁可报错也不写占位 Key 导致请求全 401/429。
fn ensure_ccswitch_key(data_dir: &std::path::Path) -> Result<String, String> {
    let mut file = crate::api_server::api_keys::load(data_dir);
    let existing = file
        .keys
        .iter()
        .find(|k| k.name == CC_SWITCH_KEY_NAME)
        .map(|e| (e.enabled, e.key.clone()));
    if let Some((enabled, key)) = existing {
        if !enabled {
            // 被手动禁用过：注册是显式使用意图，恢复启用
            if let Some(e) = file
                .keys
                .iter_mut()
                .find(|k| k.name == CC_SWITCH_KEY_NAME)
            {
                e.enabled = true;
            }
            crate::api_server::api_keys::save(data_dir, &file);
        }
        return Ok(key);
    }
    let entry = crate::api_server::api_keys::ApiKeyEntry {
        id: uuid::Uuid::new_v4().to_string(),
        name: CC_SWITCH_KEY_NAME.to_string(),
        // uuid simple() = 32 位小写 hex，与前端生成的 ck_ 子 Key 格式一致（总长 35）
        key: format!("ck_{}", uuid::Uuid::new_v4().simple()),
        enabled: true,
        daily_limit: 0,
        created_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        used_date: String::new(),
        used_today: 0,
        allowed_accounts: Vec::new(),
        schedule_mode: String::new(),
        dedicated_account: String::new(),
        bind_pool: String::new(),
        daily_stats: Vec::new(),
    };
    let key = entry.key.clone();
    file.keys.push(entry);
    crate::api_server::api_keys::save(data_dir, &file);
    Ok(key)
}

/// home 下 CC Switch 数据库路径；无法定位主目录 → None（调用方报错或视为未安装）
fn home_db_path() -> Option<std::path::PathBuf> {
    dirs_home().map(|h| h.join(DB_REL))
}

fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .filter(|h| !h.trim().is_empty())
        .map(std::path::PathBuf::from)
}

/// claude 条目：`{"env": {...}}` 嵌套结构（官方文档「添加供应商 → Claude 配置格式」
/// 及 cc-switch main provider.rs 均以 settings["env"] 读取；本机真实条目实证一致）。
/// 此前误用扁平结构会导致 cc-switch 读不到端点/凭据，切换后 Claude Code 的
/// settings.json 顶层无 env 键，条目完全失效（对照官网文档复审确认）。
/// Claude Code 请求 `{ANTHROPIC_BASE_URL}/v1/messages`，base 不带 /v1
fn claude_settings_config(port: u16, model: &str, key: &str) -> serde_json::Value {
    let base = format!("http://127.0.0.1:{}", port);
    serde_json::json!({
        "env": {
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
        }
    })
}

/// TOML basic string 转义（审查：插值未转义，值含引号/反斜杠/控制字符会生成非法 TOML）
fn toml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// codex 条目：auth + config.toml（wire_api=responses，直连 /v1/responses）。
/// TOML 内 provider id 固定为 `custom`：CC Switch 的模型目录/切换逻辑按其常量
/// `CC_SWITCH_CODEX_MODEL_PROVIDER_ID = "custom"` 改写 `model_provider`，若此处用
/// 其它 id（如 aiwork）会与其产生「键在表不在」，Codex 启动即报
/// "Model provider `custom` not found"（issue #20）。两侧 DB 条目 id 仍不同，
/// 切换为整体替换 config.toml，互不覆盖。
fn codex_settings_config(port: u16, model: &str, key: &str, is_wb: bool) -> serde_json::Value {
    let provider_name = if is_wb { "WorkBuddy 网关" } else { "AI Work 助手网关" };
    let toml = format!(
        "model_provider = \"custom\"\n\
         model = \"{model_esc}\"\n\
         model_reasoning_effort = \"high\"\n\
         \n\
         [model_providers.custom]\n\
         name = \"{name_esc}\"\n\
         base_url = \"http://127.0.0.1:{port}/v1\"\n\
         wire_api = \"responses\"\n\
         requires_openai_auth = true\n",
        model_esc = toml_escape(model),
        name_esc = toml_escape(provider_name),
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
    fn ccswitch_dedicated_key_create_and_reuse() {
        let dir = std::env::temp_dir().join(format!(
            "twa_ccswitch_key_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // 首次：自动创建（ck_ 前缀、32 位 hex、不限配额、启用）
        let k1 = ensure_ccswitch_key(&dir).unwrap();
        assert!(k1.starts_with("ck_"));
        assert_eq!(k1.len(), 35);
        // 再次：复用同一 Key，不新建条目
        let k2 = ensure_ccswitch_key(&dir).unwrap();
        assert_eq!(k1, k2, "同名专用 Key 必须复用");
        let f = crate::api_server::api_keys::load(&dir);
        let dedicated: Vec<_> = f.keys.iter().filter(|k| k.name == CC_SWITCH_KEY_NAME).collect();
        assert_eq!(dedicated.len(), 1);
        assert!(dedicated[0].enabled);
        assert_eq!(dedicated[0].daily_limit, 0);
        // 与既有业务 Key 格式一致（ck_ + 32 hex）
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn claude_config_has_base_and_model_mapping() {
        let cfg = claude_settings_config(8899, "glm-5.3", "sk-test");
        // 官方文档/cc-switch main：settings_config 顶层必须是 env 包裹层
        assert_eq!(
            cfg["env"]["ANTHROPIC_BASE_URL"],
            serde_json::json!("http://127.0.0.1:8899")
        );
        assert_eq!(cfg["env"]["ANTHROPIC_DEFAULT_OPUS_MODEL"], serde_json::json!("glm-5.3"));
        assert_eq!(cfg["env"]["ANTHROPIC_AUTH_TOKEN"], serde_json::json!("sk-test"));
        let s = cfg.to_string();
        assert!(!s.contains("/v1\""), "base 不带 /v1（Claude Code 自行拼接 /v1/messages）");
    }

    #[test]
    fn toml_escape_quotes_and_control_chars() {
        assert_eq!(toml_escape("hy\"4"), "hy\\\"4");
        assert_eq!(toml_escape("a\\b"), "a\\\\b");
        assert_eq!(toml_escape("x\ny"), "x\\ny");
        assert_eq!(toml_escape("tab\tc"), "tab\\tc");
        assert_eq!(toml_escape("\u{1}"), "\\u0001");
        assert_eq!(toml_escape("普通文本"), "普通文本", "常规字符不转义");
    }

    #[test]
    fn codex_config_toml_has_wire_api_and_base_v1() {
        let cfg = codex_settings_config(8899, "hy4", "", false);
        assert_eq!(cfg["auth"]["OPENAI_API_KEY"], serde_json::json!("aiwork-local"), "空 Key 用占位");
        let toml = cfg["config"].as_str().unwrap();
        assert!(toml.contains("base_url = \"http://127.0.0.1:8899/v1\""));
        assert!(toml.contains("wire_api = \"responses\""));
        assert!(toml.contains("model = \"hy4\""));
        assert!(toml.contains("model_provider = \"custom\""));
        // issue #20：model_provider 指向的表必须同串成对，否则 Codex 报
        // "Model provider `custom` not found"
        assert!(toml.contains("[model_providers.custom]"));
    }

    #[test]
    fn wb_codex_config_uses_fixed_custom_id_with_distinct_name() {
        let cfg = codex_settings_config(8899, "hy4", "", true);
        let toml = cfg["config"].as_str().unwrap();
        // 两侧 DB 条目 id 不同，但 TOML 内 provider id 统一固定为 CC Switch 常量 custom
        assert!(toml.contains("model_provider = \"custom\""), "WB 侧同样使用固定 id custom");
        assert!(toml.contains("[model_providers.custom]"));
        assert!(toml.contains("WorkBuddy 网关"), "WB 侧以 name 区分");
        assert!(!toml.contains("aiwork"), "TOML 内不得残留 aiwork provider id");
    }

    #[test]
    fn empty_key_allowed_for_claude() {
        let cfg = claude_settings_config(8899, "m", "");
        assert_eq!(cfg["env"]["ANTHROPIC_AUTH_TOKEN"], serde_json::json!(""));
    }
}
