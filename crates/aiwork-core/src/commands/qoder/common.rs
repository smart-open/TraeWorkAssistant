//! Qoder 共享底层（F-80 M1 平移）：账号池/设置读写、账号 id。
//! 桌面专属（live_account_id/live_work_account_id/ide/work 客户端探测/环境检测/
//! 打开客户端）不移植——Web 版无本地客户端数据源。

use crate::state::AppState;

// ── 数据结构（§5.4 数据模型；对照 WorkBuddyAccount 裁剪）────────────────────

/// 账号池记录。id = "qd-" + sha256(token)[..12]（同 token 稳定同 id，防换发重复入池）。
#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
pub struct QoderAccount {
    #[serde(default)]
    pub id: String,
    /// 全家桶 uid（/api/v1/userinfo 回填；PAT 导入时 userinfo 失败可空）
    #[serde(default)]
    pub uid: String,
    #[serde(default)]
    pub nickname: String,
    /// 手机号掩码（userinfo 未提供时留空，M1 不做猜测）
    #[serde(default)]
    pub phone_masked: String,
    /// free | pro | pro+ | teams（只读提示）；userinfo 未提供时留空
    #[serde(default)]
    pub plan: String,
    /// 凭证来源：pat | ide_store | qoderwork_store | mitm | cli（M1 实装 pat）
    #[serde(default)]
    pub credential_source: String,
    /// token 过期时间（Unix 秒；PAT 长期凭证为 None）
    #[serde(default)]
    pub token_expires_at: Option<i64>,
    #[serde(default)]
    pub needs_relogin: bool,
    #[serde(default)]
    pub relogin_reason: String,
    #[serde(default)]
    pub group_id: String,
    #[serde(default)]
    pub note: String,
    /// 余额缓存（credits_fetch 成功后回写）
    #[serde(default)]
    pub credits_balance: Option<f64>,
    #[serde(default)]
    pub credits_fetched_at: Option<String>,
    /// 每账号稳定设备指纹（§5.10 多账号并发；入池时生成一次永不轮换，
    /// 存量账号经 ensure_pool_profiles 惰性回填）
    #[serde(default)]
    pub device_profile: Option<crate::tasks::qoder_device::QoderDeviceProfile>,
}

/// Qoder 设置（kv `qoder_settings`；对照 WorkBuddySettings 裁剪）
#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
pub struct QoderSettings {
    /// 应用内调度器 qoder-checkin 任务启用判定（默认开，§5.7）
    #[serde(default = "default_true")]
    pub auto_checkin: bool,
}

fn default_true() -> bool {
    true
}

// ── 账号池 / 设置读写 ───────────────────────────────────────────────────────

pub(crate) fn load_pool(state: &AppState) -> Vec<QoderAccount> {
    // 只读路径容错：逐行解析，损坏行跳过（原实现整组解析失败 → 静默空池）
    load_pool_rows(state).0
}

/// 逐行解析池：返回 (正常账号, 损坏行原始值)。写入路径必须走 load_pool_checked
/// 拒绝损坏池，避免「load 丢行 → save 整池覆盖」静默永久丢账号。
fn load_pool_rows(state: &AppState) -> (Vec<QoderAccount>, Vec<serde_json::Value>) {
    let v = crate::store::docs::qoder_pool_load(&crate::store::db(&state.data_dir));
    let Some(rows) = v.get("accounts").and_then(|a| a.as_array()) else {
        return (Vec::new(), Vec::new());
    };
    let mut ok = Vec::new();
    let mut corrupt = Vec::new();
    for r in rows {
        match serde_json::from_value::<QoderAccount>(r.clone()) {
            Ok(a) => ok.push(a),
            Err(_) => corrupt.push(r.clone()),
        }
    }
    (ok, corrupt)
}

/// 严格版池读（供 with_pool_mut 写路径把关）：存在损坏行时先把损坏行原文备份到
/// data_dir/qoder_pool.corrupt.json，再拒绝返回——整池覆盖前必须显式处理。
pub(crate) fn load_pool_checked(state: &AppState) -> Result<Vec<QoderAccount>, String> {
    let (accounts, corrupt) = load_pool_rows(state);
    if corrupt.is_empty() {
        return Ok(accounts);
    }
    let backup = state.data_dir.join("qoder_pool.corrupt.json");
    let _ = crate::fs_utils::write_json(&backup, &serde_json::json!({ "corrupt_rows": corrupt }));
    crate::fs_utils::app_log(
        &state.data_dir,
        "qoder 账号池存在损坏行，已备份到 qoder_pool.corrupt.json，拒绝整池覆盖以防丢账号",
    );
    Err(
        "账号池数据存在损坏行，已备份到 qoder_pool.corrupt.json；本次修改已取消以保护其余账号，请处理备份文件后重试"
            .into(),
    )
}

pub(crate) fn save_pool(state: &AppState, accounts: &[QoderAccount]) -> Result<(), String> {
    let v = serde_json::to_value(accounts).map_err(|e| format!("序列化失败: {e}"))?;
    crate::store::docs::qoder_pool_save(
        &crate::store::db(&state.data_dir),
        &serde_json::json!({ "accounts": v }),
    )
}

/// 池读-改-写互斥执行（F-80 I09）：OAuth/PAT 导入、改名/移除、分组回落等路径
/// 并发时整池覆盖会丢更新，统一经此函数持锁执行。
/// 注意：tasks 层 sync_pool_expiry/ensure_pool_profiles 直操原始 JSON 保留未知字段，
/// 不可走本函数（会解析成 Vec<QoderAccount> 丢字段），须自行持 state.qoder_pool_lock。
pub(crate) fn with_pool_mut<T>(
    state: &AppState,
    f: impl FnOnce(&mut Vec<QoderAccount>) -> Result<T, String>,
) -> Result<T, String> {
    // 中毒锁恢复（into_inner）：单次 panic 不应永久阻塞后续池写
    let _guard = state
        .qoder_pool_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut accounts = load_pool_checked(state)?;
    let out = f(&mut accounts)?;
    save_pool(state, &accounts)?;
    Ok(out)
}

pub(crate) fn load_settings(state: &AppState) -> QoderSettings {
    crate::store::db(&state.data_dir).kv_get("qoder_settings")
}

/// Qoder 设置读取（main 版 #[tauri::command] 转普通函数；cmd_bridge 白名单分发）
pub fn qoder_settings_get(state: &AppState) -> QoderSettings {
    load_settings(state)
}

/// Qoder 设置写入（patch 全量覆盖单字段设置）
pub fn qoder_settings_set(state: &AppState, patch: QoderSettings) -> Result<(), String> {
    crate::store::db(&state.data_dir).kv_set("qoder_settings", &patch)
}

// ── 账号 id（qd- + sha256[..12]，单一实现在 tasks::qoder_common，此处 re-export）──

pub(crate) use crate::tasks::qoder_common::account_id_of;

/// Qoder 自动签到开关（应用内调度器 qoder-checkin 任务的启用判定，
/// 对齐 workbuddy::wb_auto_checkin_enabled 惯例）
pub(crate) fn qoder_auto_checkin_enabled(state: &AppState) -> bool {
    load_settings(state).auto_checkin
}
