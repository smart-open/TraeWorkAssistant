//! CodeArts（华为云码道，国内站）共享底层：账号池/设置读写、AK/SK 脱敏。
//!
//! 账号池记录结构对齐前端 CodeartsAccountView；AK/SK 明文不入池，
//! 只经 tasks::codearts_common 写 vault，池里仅存掩码与 has_credential 徽标。

use tauri::State;

use crate::state::AppState;

/// 账号池记录。id = "ca-" + sha256(AK:SK)[..12]（同凭据稳定同 id，防重复入池）。
#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
pub struct CodeartsAccount {
    #[serde(default)]
    pub id: String,
    /// 展示名（用户录入或默认 "CodeArts <尾号>"）
    #[serde(default)]
    pub name: String,
    /// AK 掩码（前4+****+后4，脱敏展示）
    #[serde(default)]
    pub ak_masked: String,
    #[serde(default)]
    pub group_id: String,
    #[serde(default)]
    pub note: String,
    /// token store 中有可用凭证（由列表组装时按需计算，持久化亦保留便于快照）
    #[serde(default)]
    pub has_credential: bool,
    #[serde(default)]
    pub credits_balance: Option<f64>,
    #[serde(default)]
    pub credits_fetched_at: Option<String>,
    #[serde(default)]
    pub credits_expire_at: Option<i64>,
    /// 是否参与每日签到
    #[serde(default = "default_true")]
    pub auto_checkin: bool,
    /// 入池时间戳（Unix 秒）
    #[serde(default)]
    pub added_at: i64,
}

fn default_true() -> bool {
    true
}

/// CodeArts 设置（kv `codearts_settings`）。
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct CodeartsSettings {
    #[serde(default = "default_true")]
    pub auto_checkin: bool,
    #[serde(default = "default_checkin_hhmm")]
    pub checkin_hhmm: String,
}

fn default_checkin_hhmm() -> String {
    "10:15".into()
}

impl Default for CodeartsSettings {
    fn default() -> Self {
        Self { auto_checkin: true, checkin_hhmm: default_checkin_hhmm() }
    }
}

/// AK 掩码：前4 + **** + 后4；短串整体打码。
pub fn mask_ak(ak: &str) -> String {
    let n = ak.chars().count();
    if n <= 8 {
        return "****".into();
    }
    let head: String = ak.chars().take(4).collect();
    let tail: String = ak.chars().skip(n - 4).collect();
    format!("{head}****{tail}")
}

// ── 账号池读写 ───────────────────────────────────────────────────────────────

pub(crate) fn load_pool(state: &AppState) -> Vec<CodeartsAccount> {
    load_pool_rows(state).0
}

fn load_pool_rows(state: &AppState) -> (Vec<CodeartsAccount>, Vec<serde_json::Value>) {
    let v = crate::store::docs::codearts_pool_load(&crate::store::db(&state.data_dir));
    let Some(rows) = v.get("accounts").and_then(|a| a.as_array()) else {
        return (Vec::new(), Vec::new());
    };
    let mut ok = Vec::new();
    let mut corrupt = Vec::new();
    for r in rows {
        match serde_json::from_value::<CodeartsAccount>(r.clone()) {
            Ok(a) => ok.push(a),
            Err(_) => corrupt.push(r.clone()),
        }
    }
    (ok, corrupt)
}

pub(crate) fn load_pool_checked(state: &AppState) -> Result<Vec<CodeartsAccount>, String> {
    let (accounts, corrupt) = load_pool_rows(state);
    if corrupt.is_empty() {
        return Ok(accounts);
    }
    let backup = state.data_dir.join("codearts_pool.corrupt.json");
    let _ = crate::fs_utils::write_json(&backup, &serde_json::json!({ "corrupt_rows": corrupt }));
    Err("CodeArts 账号池存在损坏行，已备份到 codearts_pool.corrupt.json；本次修改已取消以保护其余账号".into())
}

pub(crate) fn save_pool(state: &AppState, accounts: &[CodeartsAccount]) -> Result<(), String> {
    let v = serde_json::to_value(accounts).map_err(|e| format!("序列化失败: {e}"))?;
    crate::store::docs::codearts_pool_save(
        &crate::store::db(&state.data_dir),
        &serde_json::json!({ "accounts": v }),
    )
}

/// 池读-改-写互斥执行（持 state.codearts_pool_lock，防并发整池覆盖丢账号）。
pub(crate) fn with_pool_mut<T>(
    state: &AppState,
    f: impl FnOnce(&mut Vec<CodeartsAccount>) -> Result<T, String>,
) -> Result<T, String> {
    let _guard = state.codearts_pool_lock.lock().unwrap_or_else(|e| e.into_inner());
    let mut accounts = load_pool_checked(state)?;
    let out = f(&mut accounts)?;
    save_pool(state, &accounts)?;
    Ok(out)
}

pub fn load_settings(state: &AppState) -> CodeartsSettings {
    crate::store::db(&state.data_dir).kv_get("codearts_settings")
}

#[tauri::command]
pub fn codearts_settings_get(state: State<AppState>) -> CodeartsSettings {
    load_settings(&state)
}

#[tauri::command]
pub fn codearts_settings_set(state: State<AppState>, patch: CodeartsSettings) -> Result<(), String> {
    crate::store::db(&state.data_dir).kv_set("codearts_settings", &patch)
}

/// 账号 id（ca- + sha256("{ak}:{sk}")[..12]，单一实现在 tasks::codearts_common）。
pub(crate) use crate::tasks::codearts_common::account_id_of;
