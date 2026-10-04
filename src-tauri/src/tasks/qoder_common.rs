//! Qoder 公共请求层（F-80 M1，对照 wb_common.rs 模式裁剪）。
//!
//! 端点与鉴权情报基线（docs/tmp/f80-qoder-support-design.md §2.3，M0 定稿前均为待验证）：
//! - OpenAPI 域：`openapi.qoder.com.cn`（sash 活动 / quota 用量 / userinfo / deviceToken）
//! - `Cosy-ClientType` 必带（缺失时 sash 返回 `campaigns: []` 静默空列表——最隐蔽的坑）；
//!   CN 客户端真实取值 R-4 待抓包确认，缺省 "10"（社区逆向脚本取值）
//! - token 三前缀：`pt-`（PAT）/ `jt-`（job token）/ `dt-`（device token）
//! - 设备头 `Cosy-MachineId` / `Cosy-MachineToken`：真实捕获优先透传；缺失时由
//!   `effective_creds` 注入账号绑定指纹（§5.10 多账号并发，v1.2 用户决策——
//!   伪造是正式需求，以每账号稳定绑定控制风险，见 tasks::qoder_device）
//!
//! 凭证双源说明：wb 的双源为 token store × auth 文件；Qoder 客户端本地存储解密
//! （auth.v1.dat，DPAPI+AES-GCM）受 R-8 门控——M1 仅 token store 单源（PAT 导入落库），
//! 客户端存储通道在 R-8 闭合后以新增 `client_store_creds()` 接入本层。
//!
//! 红线：全程零 token 输出（凭证不入日志/事件/UI）。

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::fs_utils;
use crate::state::AppState;

/// OpenAPI 基址（CN 域；端点常量集中可改，R-7 主备域探测位预留）
pub const OPEN_API_BASE: &str = "https://openapi.qoder.com.cn";

/// 官网 Web 域（R-11 抓包 2026-10-04：账户页 `/api/v2/me/usages/big_model_credits`
/// 端点在此域，Bearer 鉴权与 openapi 同源；openapi 主机无该路由——实测 alb 503）
pub const WEB_BASE: &str = "https://qoder.cn";

/// `Cosy-ClientType` 真实值（R-4 已闭合：2026-09-27 抓包实测主进程恒带 `cosy-clienttype: 10`）
pub const COSY_CLIENT_TYPE: &str = "10";

/// 客户端 UA 对齐（R-4 抓包实测：主进程 API 请求 UA 为 "Qoder"；渲染进程为
/// Electron 完整 UA `...QoderCN/0.4.2 Chrome/150... Electron/43.1.1...`）
pub const CLIENT_USER_AGENT: &str = "Qoder";

// ── 凭证结构 ────────────────────────────────────────────────────────────────

#[derive(Serialize, Clone, Default)]
pub struct QoderCreds {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub expires_at_ms: Option<i64>,
    #[serde(default)]
    pub uid: String,
    #[serde(default)]
    pub nickname: String,
    /// 凭证种类：pat（pt-，官方认可）| client（客户端存储/抓包透传）
    #[serde(default)]
    pub kind: String,
    /// PAT 原始凭证（pt-）：access_token 为换取后的 24h 作业令牌时，此字段保存
    /// 原始 PAT 供到期重换（PAT 长期有效，绝不入日志/事件）
    #[serde(default)]
    pub pat: String,
    /// 刷新令牌过期时刻（ms；R-10 设备流 refresh_token_expires_in ≈360d、R-6 jobToken
    /// ≈48h）。审查 L-RT 过期持久化：落库留档，供后续 RT 生命周期判定消费
    #[serde(default)]
    pub refresh_expires_at_ms: Option<i64>,
    /// 设备指纹头来源：真实捕获值（client/mitm/cli）原样保存在 token store；
    /// effective_creds 合并时缺失则注入账号绑定 machine_id（§5.10）
    #[serde(default)]
    pub machine_id: String,
    #[serde(default)]
    pub machine_token: String,
}

// 手写脱敏 Debug（derive(Debug) 会把 access_token/refresh_token/pat/machine_token
// 全量打进日志——触犯「凭证不入日志」红线）：敏感字段仅显 **（空则空串便于排障）
impl std::fmt::Debug for QoderCreds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mask = |s: &str| if s.is_empty() { "" } else { "**" };
        f.debug_struct("QoderCreds")
            .field("access_token", &mask(&self.access_token))
            .field("refresh_token", &mask(&self.refresh_token))
            .field("expires_at_ms", &self.expires_at_ms)
            .field("uid", &self.uid)
            .field("nickname", &self.nickname)
            .field("kind", &self.kind)
            .field("pat", &mask(&self.pat))
            .field("refresh_expires_at_ms", &self.refresh_expires_at_ms)
            .field("machine_id", &self.machine_id)
            .field("machine_token", &mask(&self.machine_token))
            .finish()
    }
}

fn s_of(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or("").to_string()
}

fn i_of(v: Option<&Value>) -> Option<i64> {
    let v = v?;
    v.as_i64().or_else(|| v.as_str()?.trim().parse::<i64>().ok())
}

/// expires_at 域钳制（审查 L-溢出）：畸形大值（秒/毫秒单位错、服务端脏数据）会导致
/// 临期比较整数溢出、到期日历展示为千年后；超界一律视为「无过期信息」（保守走刷新路径）
pub fn clamp_expires_at(ms: i64) -> Option<i64> {
    let now = chrono::Utc::now().timestamp_millis();
    let max = now + 10 * 365 * 24 * 3_600_000;
    if ms <= 0 || ms > max {
        None
    } else {
        Some(ms)
    }
}

/// 从 token store 记录提取凭证（宽容解析；兼容 accessToken/access_token/token 键名）
pub fn creds_of(source: &Value) -> QoderCreds {
    let auth = source.get("auth").filter(|v| v.is_object()).unwrap_or(source);
    let account = source
        .get("account")
        .filter(|v| v.is_object())
        .unwrap_or(source);
    QoderCreds {
        access_token: s_of(fs_utils::dig(auth, &["accessToken", "access_token", "token", "pat"])),
        refresh_token: s_of(fs_utils::dig(auth, &["refreshToken", "refresh_token"])),
        expires_at_ms: i_of(fs_utils::dig(
            auth,
            &["expiresAtMs", "expires_at_ms", "expiresAt", "expires_at"],
        ))
        .and_then(clamp_expires_at),
        uid: s_of(fs_utils::dig(account, &["uid", "userId", "user_id", "id"])),
        nickname: s_of(fs_utils::dig(account, &["nickname", "name", "displayName"])),
        kind: s_of(fs_utils::dig(auth, &["kind", "token_kind", "credential_source"])),
        pat: s_of(fs_utils::dig(source, &["pat"])),
        refresh_expires_at_ms: i_of(fs_utils::dig(
            source,
            &["refresh_expires_at_ms", "refreshExpiresAtMs", "refresh_expires_at", "refreshExpiresAt"],
        ))
        .and_then(clamp_expires_at),
        machine_id: s_of(fs_utils::dig(source, &["machine_id", "machineId", "cosy_machine_id"])),
        machine_token: s_of(fs_utils::dig(source, &["machine_token", "machineToken", "cosy_machine_token"])),
    }
}

// ── token store（qoder_tokens 表；结构 {version, tokens: {id: rec}}）────────

/// token store 读改写互斥：load→merge→save 非原子，签到/积分/刷新/导入多通道
/// 并发写会互相覆盖丢更新（last-writer-wins 抹掉彼此的新 token）。进程内全局锁
/// 串行化表级读改写；仅持锁做本地 IO，不覆盖网络请求路径（无死锁面）。
static TOKEN_STORE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 每账号刷新互斥（审查 H-1）：签到/积分/定时兜底/401 自愈多通道并发触发同一账号
/// `ensure_fresh` 时，两个线程可能同时拿旧 refresh_token/PAT 换新 token——后落库者
/// 覆盖先落库者，被覆盖方刚拿到的令牌即刻失效（严重时 refresh_token 一并丢，被迫重登）。
/// 以账号 id 为键的进程内锁串行化「读凭证→网络刷新→落库」全程；持锁后重读 token store
/// 天然构成二次检查：他人已刷新落库则直接命中新凭证（fresh/复用），不再重复发网络请求。
/// 锁序约定：refresh 锁 → TOKEN_STORE_LOCK（save_token_store 单向获取，无环）。
/// 表本体提升到模块级（原为 refresh_lock_for 内静态）：供 refresh_lock_remove 在
/// 账号移除路径主动清理访问（P3-L——账号移除后条目永驻 HashMap，进程生命周期内
/// 每次增删账号泄漏一把锁）。
static REFRESH_LOCKS: std::sync::Mutex<
    Option<std::collections::HashMap<String, std::sync::Arc<std::sync::Mutex<()>>>>,
> = std::sync::Mutex::new(None);

fn refresh_lock_for(acct_id: &str) -> std::sync::Arc<std::sync::Mutex<()>> {
    let mut g = REFRESH_LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    g.get_or_insert_with(std::collections::HashMap::new)
        .entry(acct_id.to_string())
        .or_insert_with(|| std::sync::Arc::new(std::sync::Mutex::new(())))
        .clone()
}

/// P3-L 主动清理：账号移除路径（qoder_account_remove 等删账号后）调用，回收其全局
/// 刷新锁条目（仅摘表项，无 DB 读；并发持有者的 Arc 由引用计数自然释放，若移除与
/// 并发刷新竞争，新到的 ensure_fresh 会重建条目，语义不变）。
/// Q3：原方案在 ensure_fresh 内惰性 gc（每次调用 qoder_pool_load 全表 SELECT），
/// 经网关 qoder_identity 回调成为热路径每请求 +1 次 DB 读，且 refresh_lock_for 先
/// or_insert 建条目使 gc 永远命中无法短路——改为移除账号处主动调用本函数。
pub(crate) fn refresh_lock_remove(acct_id: &str) {
    let mut g = REFRESH_LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = g.as_mut() {
        map.remove(acct_id);
    }
}

/// 敏感字段键名（审查 P0-1 凭证收敛）：rec 中这些字段一律占位（空串）存 DB，
/// 明文进 vault（Stronghold + DPAPI，与 Trae/WB 同一 vault，ns="qoder"）。
/// machine_id 是设备标识符（非凭证），保留在 DB 供排障。
const TOKEN_SENSITIVE_KEYS: [&str; 4] = [
    "access_token",
    "refresh_token",
    "pat",
    "machine_token",
];

/// DB 读取 + vault 回填（仅内存）：敏感字段为占位空串时从 vault 回填。
/// vault 不可用 / 无记录 → 保持空串（上层按 no_credential 处理，fail-secure）。
pub fn token_store_load_secure(data_dir: &std::path::Path) -> Value {
    let mut store = crate::store::docs::qoder_token_store_load(&crate::store::db(data_dir));
    let Some(tokens) = store.get_mut("tokens").and_then(Value::as_object_mut) else {
        return store;
    };
    for (id, rec) in tokens.iter_mut() {
        let Some(rm) = rec.as_object_mut() else { continue };
        let Some(sec) = crate::vault::ns_get(data_dir, "qoder", id) else { continue };
        for k in TOKEN_SENSITIVE_KEYS {
            let Some(val) = sec.get(k).and_then(Value::as_str) else { continue };
            if val.is_empty() {
                continue;
            }
            // 仅填空值：DB 明文优先（更新鲜，如迁移残留，待下次写入收敛）
            if rm.get(k).and_then(Value::as_str).map_or(true, |s| s.is_empty()) {
                rm.insert(k.to_string(), serde_json::json!(val));
            }
        }
    }
    store
}

/// 整库敏感字段收敛：每个 rec 的非空敏感值字段级合并写入 vault，随后整库占位
///（明文只留 vault）。返回写入 vault 的账号数。
fn secure_store_for_save(
    data_dir: &std::path::Path,
    store: &mut Value,
) -> Result<usize, String> {
    let Some(tokens) = store.get_mut("tokens").and_then(Value::as_object_mut) else {
        return Ok(0);
    };
    let ids: Vec<String> = tokens.keys().cloned().collect();
    let mut wrote = 0usize;
    for id in &ids {
        let Some(rec) = tokens.get(id.as_str()) else { continue };
        let mut entry = crate::vault::ns_get(data_dir, "qoder", id)
            .unwrap_or_else(|| serde_json::json!({}));
        let mut dirty = false;
        for k in TOKEN_SENSITIVE_KEYS {
            if let Some(v) = rec.get(k).and_then(Value::as_str) {
                if !v.is_empty() {
                    entry[k] = serde_json::json!(v);
                    dirty = true;
                }
            }
        }
        if dirty {
            crate::vault::ns_set(data_dir, "qoder", id, &entry)?;
            wrote += 1;
        }
    }
    // 整库占位（整表替换写回：所有 rec 的敏感字段一律清空）
    for rec in tokens.values_mut() {
        if let Some(rm) = rec.as_object_mut() {
            for k in TOKEN_SENSITIVE_KEYS {
                if rm.contains_key(k) {
                    rm.insert(k.to_string(), serde_json::json!(""));
                }
            }
        }
    }
    Ok(wrote)
}

/// 启动迁移（P0-1）：存量明文凭证收敛进 vault + DB 占位化（幂等，无明文时零开销）
pub fn migrate_token_store(state: &AppState) -> Result<usize, String> {
    let _guard = TOKEN_STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut store = crate::store::docs::qoder_token_store_load(&crate::store::db(&state.data_dir));
    let has_plain = store
        .get("tokens")
        .and_then(Value::as_object)
        .map(|tokens| {
            tokens.values().any(|rec| {
                TOKEN_SENSITIVE_KEYS.iter().any(|k| {
                    rec.get(k)
                        .and_then(Value::as_str)
                        .map_or(false, |s| !s.is_empty())
                })
            })
        })
        .unwrap_or(false);
    if !has_plain {
        return Ok(0);
    }
    let wrote = secure_store_for_save(&state.data_dir, &mut store)?;
    crate::store::docs::qoder_token_store_save(&crate::store::db(&state.data_dir), &store)?;
    Ok(wrote)
}

pub fn load_token_store(state: &AppState) -> Value {
    token_store_load_secure(&state.data_dir)
}

/// 生效凭证（M1 单源：token store；客户端存储通道 R-8 闭合后接入双源比较）。
/// 设备指纹在此合并注入（§5.10 唯一出口：checkin/credits 均经此）——
/// 真实捕获优先，缺失时注入账号绑定 machine_id + 现场随机 machine_token。
pub fn effective_creds(state: &AppState, acct_id: &str) -> QoderCreds {
    let mut creds = load_token_store(state)
        .get("tokens")
        .and_then(|t| t.get(acct_id))
        .map(creds_of)
        .unwrap_or_default();
    let profile = load_device_profile(state, acct_id);
    super::qoder_device::merge_device_profile(&mut creds, profile.as_ref());
    creds
}

/// 从账号池读取账号绑定指纹（§5.10；缺失返回 None → 不注入）
fn load_device_profile(state: &AppState, acct_id: &str) -> Option<super::qoder_device::QoderDeviceProfile> {
    let pool = crate::store::docs::qoder_pool_load(&crate::store::db(&state.data_dir));
    pool.get("accounts")
        .and_then(Value::as_array)
        .and_then(|arr| {
            arr.iter()
                .find(|a| a.get("id").and_then(Value::as_str) == Some(acct_id))
        })
        .and_then(|a| a.get("device_profile").cloned())
        .and_then(|p| serde_json::from_value(p).ok())
}

/// 写工具侧凭证副本（version≠1 拒写；非空字段 merge + updated_at，对齐 wb_common 同语义）。
/// 凭证收敛（P0-1）：敏感字段进 vault、DB 占位；vault 写失败时仍落占位库并返回 Err
///（对齐 Trae 红线：宁可丢本次凭据更新，也不把 token/pat 明文写进 SQLite）。
pub fn save_token_store(state: &AppState, id: &str, creds: &QoderCreds) -> Result<(), String> {
    let _guard = TOKEN_STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut store = load_token_store(state);
    if !store.is_object() {
        store = serde_json::json!({});
    }
    let target = {
        let obj = store.as_object_mut().unwrap();
        match obj.get("version") {
            Some(v) if v.as_i64() != Some(1) => {
                return Err("token_store 版本不识别，拒绝写入".to_string());
            }
            _ => {}
        }
        obj.insert("version".into(), serde_json::json!(1));
        let tokens = obj.entry("tokens").or_insert_with(|| serde_json::json!({}));
        let mut rec = tokens
            .as_object_mut()
            .and_then(|t| t.get(id).cloned())
            .unwrap_or(serde_json::json!({}));
        if let Some(rm) = rec.as_object_mut() {
            let val = serde_json::to_value(creds).map_err(|e| e.to_string())?;
            for (k, v) in val.as_object().into_iter().flatten() {
                // 空串/null 一律不覆盖已有值（局部更新不抹掉设备头/种类等存量字段）
                if !v.is_null() && !(v.is_string() && v.as_str().unwrap_or("").is_empty()) {
                    rm.insert(k.clone(), v.clone());
                }
            }
            rm.insert("updated_at".into(), serde_json::json!(fs_utils::now_iso()));
        }
        if let Some(t) = store.get_mut("tokens").and_then(Value::as_object_mut) {
            t.insert(id.to_string(), rec);
        }
        store
            .get("tokens")
            .and_then(|t| t.get(id))
            .cloned()
            .ok_or_else(|| "token_store 内部错误：目标行缺失".to_string())?
    };
    // 敏感字段收敛进 vault（慢 IO）——期间其他进程可能已写库；故落库不使用上面的
    // 读时快照整表替换，而是重新 load DB 最新表做「仅目标行替换」的行级合并，
    // 把进程间 last-writer-wins 的覆盖窗口从「vault 全程」收窄到「load→replace 数毫秒」
    //（审查 H-1 第②步；进程内并发由 TOKEN_STORE_LOCK + 每账号刷新锁防护）。
    let vault_result = secure_store_for_save(&state.data_dir, &mut store).map(|_| ());
    let mut fresh = crate::store::docs::qoder_token_store_load(&crate::store::db(&state.data_dir));
    if !fresh.is_object() {
        fresh = serde_json::json!({});
    }
    {
        let obj = fresh.as_object_mut().unwrap();
        match obj.get("version") {
            Some(v) if v.as_i64() != Some(1) => {
                return Err("token_store 版本不识别，拒绝写入".to_string());
            }
            _ => {}
        }
        obj.insert("version".into(), serde_json::json!(1));
        if let Some(t) = obj.entry("tokens").or_insert_with(|| serde_json::json!({})).as_object_mut() {
            // 凭证红线（审查 P0 修复）：target 是 secure_store_for_save 占位化**之前**
            // 克隆的明文行，直接落库会把明文凭证写进 SQLite（直到下次启动迁移才收敛，
            // 且 token_store_load_secure「DB 明文优先」会持续旁路 vault 回填）。
            // 落库前必须与 secure_store_for_save 同口径占位——DB qoder_tokens 一律空串，
            // 明文唯一驻留地是 vault；vault 失败时同样落占位行（重新登录可恢复）
            let mut row = target;
            if let Some(rm) = row.as_object_mut() {
                for k in TOKEN_SENSITIVE_KEYS {
                    if rm.contains_key(k) {
                        rm.insert(k.to_string(), serde_json::json!(""));
                    }
                }
            }
            t.insert(id.to_string(), row);
        }
    }
    crate::store::docs::qoder_token_store_save(&crate::store::db(&state.data_dir), &fresh)?;
    vault_result.map_err(|e| {
        format!("Qoder 凭据加密存储失败（已仅保存占位信息，重新登录可恢复）: {e}")
    })
}

/// 删除 token store 记录（账号移除时同步清理 vault 凭证）。
/// 顺序（审查 L）：DB 落库成功后才清 vault——save 失败时保留 vault 凭证并返 Err，
/// 避免「库里还在、密钥已删」的悬挂态（重导同账号可复用既有凭证恢复）。
pub fn remove_token(state: &AppState, id: &str) -> Result<(), String> {
    let _guard = TOKEN_STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // 凭证红线（审查 P0 修复）：此处只需摘行，必须读 DB 原始表（占位形态）——
    // 不可走 load_token_store（vault 回填后的明文整表写回，会把其余账号的
    // 明文凭证重新落库）
    let mut store =
        crate::store::docs::qoder_token_store_load(&crate::store::db(&state.data_dir));
    if let Some(t) = store.get_mut("tokens").and_then(Value::as_object_mut) {
        t.remove(id);
    }
    crate::store::docs::qoder_token_store_save(&crate::store::db(&state.data_dir), &store)?;
    crate::vault::ns_remove(&state.data_dir, "qoder", id);
    Ok(())
}

/// 清除设备流凭证残留（疑点②审查修复）：PAT 导入成功后调用。
/// refresh_token / refresh_expires_at_ms 为设备流专属字段——PAT 通道（pat 字段
/// 非空即走 jobToken 重换）完全不消费；同账号此前经 OAuth/IDE 扫描入池时留下的
/// 旧设备流 RT 会在 vault 中残留，形成「PAT 优先但 refresh 残留」混合态（敏感
/// 凭证留驻且无任何使用方）。save_token_store 为非空字段合并，空值不覆盖，
/// 无法经由其清除——须显式移除。machine_id/machine_token 保留：有效设备指纹
/// 与凭证种类无关，PAT 通道请求头仍消费。
pub fn clear_device_flow_creds(state: &AppState, id: &str) -> Result<(), String> {
    let _guard = TOKEN_STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // vault 条目：字段级移除后整体回写（ns_get 拿到的即明文 entry）
    if let Some(mut entry) = crate::vault::ns_get(&state.data_dir, "qoder", id) {
        if let Some(rm) = entry.as_object_mut() {
            let had = rm.remove("refresh_token").is_some();
            let had2 = rm.remove("refresh_expires_at_ms").is_some();
            if had || had2 {
                crate::vault::ns_set(&state.data_dir, "qoder", id, &entry)?;
            }
        }
    }
    // DB 行：与 remove_token 同口径读原始占位表（防明文整表写回），目标行移除字段
    let mut store =
        crate::store::docs::qoder_token_store_load(&crate::store::db(&state.data_dir));
    if let Some(rec) = store
        .get_mut("tokens")
        .and_then(Value::as_object_mut)
        .and_then(|t| t.get_mut(id))
        .and_then(Value::as_object_mut)
    {
        rec.remove("refresh_token");
        rec.remove("refresh_expires_at_ms");
    }
    crate::store::docs::qoder_token_store_save(&crate::store::db(&state.data_dir), &store)?;
    Ok(())
}

// ── 统一请求头（§5.2：Cosy 头必带）─────────────────────────────────────────

/// Bearer + Cosy-ClientType（缺失 → 服务端静默空列表）+ 可选设备头透传 + UA。
pub fn build_auth_headers(creds: &QoderCreds) -> Vec<(String, String)> {
    let mut h = vec![
        (
            "Authorization".to_string(),
            format!("Bearer {}", creds.access_token),
        ),
        ("Cosy-ClientType".to_string(), COSY_CLIENT_TYPE.to_string()),
        ("User-Agent".to_string(), CLIENT_USER_AGENT.to_string()),
        ("Content-Type".to_string(), "application/json".to_string()),
    ];
    // 设备指纹头原样携带（注入发生在 effective_creds 合并层，§5.10）
    if !creds.machine_id.is_empty() {
        h.push(("Cosy-MachineId".to_string(), creds.machine_id.clone()));
    }
    if !creds.machine_token.is_empty() {
        h.push(("Cosy-MachineToken".to_string(), creds.machine_token.clone()));
    }
    h
}

/// POST JSON → (http_status, parsed)；status=0 网络不可达（复用 wb_common 同款实现）
pub fn post_json(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(String, String)],
    body: &Value,
) -> (u16, Option<Value>) {
    crate::tasks::wb_common::post_json(agent, url, headers, body)
}

pub fn get_json(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(String, String)],
) -> (u16, Option<Value>, String) {
    crate::tasks::wb_common::get_json(agent, url, headers)
}

// ── token 刷新（deviceToken/refresh；R-10 轮询参数侦察后补 poll 接入）───────

/// 刷新失败分类（P1，移植 wb_common::RefreshFail 语义）：区分「凭证永久失效」与
/// 「暂态故障」——原实现 status != 200 一律 None，401/403（refresh_token 永久吊销）
/// 与断网同报 refresh_failed，调度器视为暂态无限重试，账号永不标记 needs_relogin
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshFail {
    /// 网络不可达 / HTTP 5xx / 响应结构异常：暂态，保留 30 分钟冷却重试
    Transient,
    /// HTTP 4xx（含空 refresh_token 守卫）：refresh_token 被服务端永久拒绝
    ///（吊销/过期；一次性轮换下旧 RT 不可恢复），应标记 needs_relogin 停止重试
    AuthDead,
}

/// 非 200 的 HTTP 状态 → 刷新失败分类（Q1 抽纯函数便于单测；status=0/200 由调用方前置处理）。
/// 429（限流）/408（请求超时）归 Transient：二者源于服务端压力/超时等暂态因素，
/// 重试可恢复——若归 AuthDead 会经 mark_needs_relogin 永久停用本可自愈的账号；
/// 其余 4xx 视为 refresh_token 被服务端永久拒绝（吊销/过期），归 AuthDead；
/// 5xx 及其余状态为服务端故障，归 Transient。
pub(crate) fn classify_refresh_status(status: u16) -> RefreshFail {
    if status == 429 || status == 408 {
        RefreshFail::Transient
    } else if (400..500).contains(&status) {
        RefreshFail::AuthDead
    } else {
        RefreshFail::Transient
    }
}

/// 无签名刷新端点按**刷新令牌前缀**分派（agent2api refresh.rs 情报互证 2026-10-03）：
/// - `jrt-`（作业令牌的刷新令牌，PAT 通道产物）→ `/api/v1/jobToken/refresh`
/// - 其余（设备流刷新令牌）→ `/api/v1/deviceToken/refresh`
/// 两端点同在 openapi 主机、无 COSY 签名、body 均为 snake_case。
/// 按「前缀」而非「有无 PAT」分派：PAT 与设备刷新令牌可并存于同一账号，按存在性
/// 分派会让 PAT 劫持设备刷新，导致 COSY 身份与记录 machineId 错位（上游实测教训）。
pub(crate) fn refresh_endpoint_for(refresh_token: &str) -> &'static str {
    if refresh_token.starts_with("jrt-") {
        "/api/v1/jobToken/refresh"
    } else {
        "/api/v1/deviceToken/refresh"
    }
}

/// 调无签名刷新端点（body {"refresh_token":...}）并给出失败分类。
/// 成功返回新 Creds（expires 按 expiresIn 归一为毫秒回填——R-6 实测毫秒级 86400000，
/// 秒级值兼容 ×1000；设备头原样透传保留）。
///
/// agent2api 三条踩坑的本地对照（2026-10-03 情报核对）：
/// ①「按前缀分派」→ refresh_endpoint_for（此前恒打 deviceToken/refresh，
///   jrt- 刷新令牌会被设备端点拒绝）；
/// ②「拒绝 expires_in 相对值」→ 本地 normalize_expires_in 以 30 天秒数为阈值的
///   量级判别已覆盖其实测坑点（设备端点回 2591999994 毫秒，朴素秒解释会写出
///   82 年后的过期时刻；阈值判别归毫秒 = 30 天，正确）；
/// ③「身份只认 uid 字段」→ 本地刷新响应不解析身份（uid 唯一来源是 userinfo
///   独立通道），无身份错位面，不适用。
pub fn refresh_token_once_ex(
    agent: &ureq::Agent,
    creds: &QoderCreds,
) -> (Option<QoderCreds>, RefreshFail) {
    if creds.refresh_token.is_empty() {
        return (None, RefreshFail::AuthDead);
    }
    // 刷新端点不带 Authorization（下方 retain 移除；鉴权完全靠 body 中的 refresh_token）+ Cosy 头
    let mut h = build_auth_headers(creds);
    h.retain(|(k, _)| k != "Authorization");
    let url = format!("{OPEN_API_BASE}{}", refresh_endpoint_for(&creds.refresh_token));
    let (status, body) = post_json(
        agent,
        &url,
        &h,
        &serde_json::json!({ "refresh_token": creds.refresh_token }),
    );
    if status == 0 {
        return (None, RefreshFail::Transient);
    }
    if status != 200 {
        // Q1：429（限流）/408（超时）为可恢复暂态（Transient），其余 4xx 才是
        // 凭证被服务端永久拒绝（AuthDead）——分类理由见 classify_refresh_status
        return (None, classify_refresh_status(status));
    }
    let Some(body) = body else {
        return (None, RefreshFail::Transient);
    };
    let acc = s_of(fs_utils::dig(&body, &["accessToken", "access_token", "token"]));
    if acc.is_empty() {
        return (None, RefreshFail::Transient);
    }
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut out = creds.clone();
    out.access_token = acc;
    let ref_tok = s_of(fs_utils::dig(&body, &["refreshToken", "refresh_token"]));
    if !ref_tok.is_empty() {
        out.refresh_token = ref_tok;
    }
    if let Some(e) = i_of(fs_utils::dig(&body, &["expiresIn", "expires_in"])) {
        out.expires_at_ms = Some(now_ms + normalize_expires_in(e));
    } else {
        // P2：响应缺 expiresIn 时落保守本地过期基准（now+12h）——否则 expires_at 恒为
        // None，ensure_fresh 的临期判定永无基准，每次调用都重发刷新请求。12h 低于客户端
        // 典型 24h 有效期，宁多刷不误用；clamp_expires_at 仅钳非正/超界畸形值，
        // now+12h 在合法域内不受影响
        out.expires_at_ms = Some(now_ms + 12 * 3_600_000);
    }
    // 刷新令牌过期时刻随行解析（R-6 实测 refresh_token_expires_in≈48h ms；审查 L-RT）
    if let Some(e) = i_of(fs_utils::dig(
        &body,
        &["refresh_token_expires_in", "refreshTokenExpiresIn"],
    )) {
        out.refresh_expires_at_ms = Some(now_ms + normalize_expires_in(e));
    }
    (Some(out), RefreshFail::Transient)
}

// ── userinfo（PAT 导入时回填 uid/昵称；失败容错不阻塞导入）──────────────────

/// POST /api/v1/userinfo → (uid, nickname)；任何失败返回 (None, None)。
/// （R-13 侦察注：设计文档 L553 写 GET，但实现一直以 POST 在用且未被证伪——
/// 端点真实方法未实测，若后续发现 404/405 再切 get_json。）
pub fn fetch_userinfo(agent: &ureq::Agent, creds: &QoderCreds) -> (Option<String>, Option<String>) {
    let url = format!("{OPEN_API_BASE}/api/v1/userinfo");
    let (status, body) = post_json(agent, &url, &build_auth_headers(creds), &serde_json::json!({}));
    if status != 200 {
        return (None, None);
    }
    let Some(b) = body else { return (None, None) };
    // 信封穿透：{data:{uid,nickname}} 或平铺
    let uid = fs_utils::dig(&b, &["uid", "userId", "user_id", "id"]).and_then(Value::as_str).map(String::from);
    let nickname = fs_utils::dig(&b, &["nickname", "nickName", "name", "displayName"])
        .and_then(Value::as_str)
        .map(String::from);
    (uid, nickname)
}

// ── plan 查询（R-7 抓包固化：GET /api/v2/user/plan → plan_tier_name 等）─────

/// 套餐档位展示名映射（2026-10-02 实测两通道取值形态）：
/// - openapi `/api/v2/user/plan`：`plan_tier_name` 如 "Pro Trial"（R-7 抓包）
/// - qoder.cn Web `/api/v1/me/userplan`：`plan_tier`/`plan_tier_name` 为枚举，
///   如 `PLAN_TIER_FREE`（免费）、`PLAN_TIER_PRO`（专业版）、`PLAN_TIER_PRO_PLUS`
///   （高级版）、`PLAN_TIER_ULTRA`（旗舰版）；定价档位见 `/api/v1/products/pricing/all`
///   （Pro / Pro+ / Ultra）。
/// 匹配不分大小写、下划线归一为空格；未识别档位原样透传。
pub fn plan_display_name(raw: &str) -> String {
    let s = raw.trim();
    if s.is_empty() {
        return String::new();
    }
    let l = s.to_ascii_lowercase().replace('_', " ");
    // 顺序敏感：trial/free 必须先于 pro（"Pro Trial" 属试用档）；pro plus 先于 pro（子串包含）
    if l.contains("trial") || l.contains("free") {
        return "体验版".into();
    }
    if l.contains("ultra") || l.contains("ultimate") {
        return "旗舰版".into();
    }
    if l.contains("pro plus") || l.contains("pro+") || l.contains("premium") || l.contains("advanced") {
        return "高级版".into();
    }
    if l.contains("pro") || l.contains("professional") {
        return "专业版".into();
    }
    s.to_string()
}

/// 套餐档位回填门控（qoder_credits 余额刷新顺带拉取 /api/v2/user/plan 的决策函数，
/// 2026-10-02 审查优化）：仅当 ①本行余额拉取成功 且 ②池内无套餐 或 存量值为待归一
/// 的原始档位名（plan_display_name 对已归一展示名/Teams 等未知档位映射为自身）时
/// 才发起查询——已归一账号的余额刷新不再支付每次一次的额外 HTTP 往返。
pub fn need_plan_fetch(row_ok: bool, stored_plan: &str) -> bool {
    row_ok && (stored_plan.is_empty() || plan_display_name(stored_plan) != stored_plan)
}

/// GET /api/v2/user/plan → (plan_tier 展示名, user_type, end_date_ms)；失败全 None。
/// 实测响应：{"user_type":"personal_professional_trial","plan_tier_name":"Pro Trial",
///           "is_personal_version":true,"is_paid_plan":false,...,"end_date":1791673619906}
pub fn fetch_plan(agent: &ureq::Agent, creds: &QoderCreds) -> (Option<String>, Option<String>, Option<i64>) {
    let url = format!("{OPEN_API_BASE}/api/v2/user/plan");
    let (status, body, _raw) = get_json(agent, &url, &build_auth_headers(creds));
    if status != 200 {
        return (None, None, None);
    }
    let Some(b) = body else { return (None, None, None) };
    // 宽容解析：tier 可能落 plan_tier_name/planTierName/plan_tier/planTier（枚举）等键
    let tier = fs_utils::dig(
        &b,
        &["plan_tier_name", "planTierName", "plan_tier", "planTier"],
    )
    .and_then(Value::as_str)
    .map(plan_display_name);
    let user_type = fs_utils::dig(&b, &["user_type", "userType"])
        .and_then(Value::as_str)
        .map(String::from);
    let end_date = fs_utils::dig(&b, &["end_date", "endDate"]).and_then(|v| {
        v.as_i64().or_else(|| v.as_str()?.trim().parse::<i64>().ok())
    });
    (tier, user_type, end_date)
}

// ── PAT 作业令牌换取（R-6 闭合：2026-09-27 实测 pt- 直调 sash 端点 401，
//    客户端真实链路为 POST /api/v1/me/jobToken 换 24h 作业令牌后再调业务端点）──

/// 作业令牌 clientId：按 PAT 派生的稳定 UUID 格式串（客户端实测携带其安装级
/// 固定 clientId；服务端未见强校验，按账号稳定派生即可，避免每次随机）
fn job_client_id(pat: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"qoder-job-client:");
    h.update(pat.as_bytes());
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// expires_in 归一为毫秒（R-6 抓包实测 86400000 = 24h，即毫秒；秒级值兼容 ×1000）。
/// 阈值 2_592_000 = 30 天的秒数：秒级上限（30d=2.592e6）与毫秒级下限（1h=3.6e6）
/// 之间留出安全间隔，杜绝「秒级超长有效期被误当毫秒」的歧义
pub(crate) fn normalize_expires_in(e: i64) -> i64 {
    if e > 2_592_000 {
        e
    } else {
        e * 1000
    }
}

/// PAT → 作业令牌。多端点尝试（审查 M-2：已证实通道优先，未证实兜底在后）：
/// ① `POST /api/v1/me/jobToken` body `{"clientId": ...}`（R-6 抓包：客户端真实路径）
/// ② `POST /api/v1/jobToken/exchange` body `{"pat": ...}`（设计文档附录 A，社区逆向，未证实）
/// ③ `POST /api/v1/jobToken/exchange` 空 body（Bearer 鉴权，未证实）
/// 成功返回以作业令牌为 access_token 的 Creds（pat 字段保存原始 PAT 供到期重换）。
/// data_dir 用于状态码落日志（脱敏：只记端点与 HTTP 状态，不含 token）——全部失败时
/// 便于定位是 401（PAT 不被接受）还是 404（端点不存在）。
///
/// 失败分类（审查 P2 修复：网络失败与 PAT 被拒不再同报 None）：
/// 以**首通道（已证实路径）**的状态码为准——0（网络不可达）/5xx 归 `Transient`
///（可重试自愈），4xx 归 `Rejected`（PAT 被服务端明确拒绝，需人工）；兜底通道
/// 状态仅落日志不参与分类（②③ 未证实，恒 404 不代表 PAT 无效）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobExchangeFail {
    /// 网络/服务端暂态失败：可重试（调度冷却/401 自愈均有意义）
    Transient,
    /// PAT 被服务端拒绝（首通道 4xx）：重试无解，需重新创建并导入 PAT
    Rejected,
}

/// PAT→作业令牌探测通道（疑点① 单测锁定排序）：失败分类以首通道状态码为准，
/// 已实证的 R-6 抓包路径 `/api/v1/me/jobToken` 必须居首（减少无效 404 请求），
/// 未证实的 exchange 兜底通道仅落日志不参与分类。单测以假 pat 断言排序与
/// body 形态，不发起真实请求。
fn job_token_attempts(pat: &str) -> [(&'static str, Value, &'static str); 3] {
    [
        (
            "/api/v1/me/jobToken",
            serde_json::json!({ "clientId": job_client_id(pat) }),
            "me/jobToken",
        ),
        (
            "/api/v1/jobToken/exchange",
            serde_json::json!({ "pat": pat }),
            "exchange+pat",
        ),
        ("/api/v1/jobToken/exchange", serde_json::json!({}), "exchange"),
    ]
}

pub fn exchange_job_token(
    agent: &ureq::Agent,
    pat: &str,
    data_dir: &std::path::Path,
) -> Result<QoderCreds, JobExchangeFail> {
    let attempts = job_token_attempts(pat);
    // 首通道（已证实 R-6 路径）状态码决定失败分类；None = 首通道未产生 HTTP 状态
    //（理论不可达——attempts 非空），按暂态兜底保守处理
    let mut first_status: Option<u16> = None;
    for (i, (path, body, tag)) in attempts.iter().enumerate() {
        let path = *path;
        let tag = *tag;
        let url = format!("{OPEN_API_BASE}{path}");
        let headers = vec![
            ("Authorization".to_string(), format!("Bearer {pat}")),
            ("Cosy-ClientType".to_string(), COSY_CLIENT_TYPE.to_string()),
            ("User-Agent".to_string(), CLIENT_USER_AGENT.to_string()),
            ("Content-Type".to_string(), "application/json".to_string()),
        ];
        let (status, resp) = post_json(agent, &url, &headers, &body);
        if i == 0 {
            first_status = Some(status);
        }
        if status == 200 {
            if let Some(b) = resp {
                let token = s_of(fs_utils::dig(&b, &["token", "accessToken", "access_token", "job_token"]));
                if !token.is_empty() {
                    let now_ms = chrono::Utc::now().timestamp_millis();
                    let expires_at_ms = i_of(fs_utils::dig(&b, &["expires_in", "expiresIn"]))
                        .map(normalize_expires_in)
                        .map(|e| now_ms + e);
                    let refresh_token = s_of(fs_utils::dig(&b, &["refresh_token", "refreshToken"]));
                    fs_utils::app_log(
                        data_dir,
                        &format!("qoder PAT→作业令牌换取成功（通道 {tag}）"),
                    );
                    return Ok(QoderCreds {
                        access_token: token,
                        refresh_token,
                        expires_at_ms,
                        kind: "pat".into(),
                        pat: pat.to_string(),
                        ..Default::default()
                    });
                }
            }
            // 200 但无令牌字段：视为该形态不匹配，继续下一通道
            fs_utils::app_log(data_dir, &format!("qoder jobToken 通道 {tag} 返回 200 但缺少令牌字段"));
        } else if status != 0 {
            fs_utils::app_log(data_dir, &format!("qoder jobToken 通道 {tag} → HTTP {status}"));
        } else {
            fs_utils::app_log(data_dir, &format!("qoder jobToken 通道 {tag} 网络不可达"));
        }
    }
    fs_utils::app_log(
        data_dir,
        &format!("qoder PAT→作业令牌全部通道失败（首通道 HTTP {}）：PAT 可能无效/已吊销，或换取端点对 PAT 另有要求", first_status.unwrap_or(0)),
    );
    // 分类（见函数头）：首通道 0/5xx = 暂态；4xx = PAT 被拒；200-but-malformed
    // 走完全部通道（接口结构变化）重试同样无解，保守归 Rejected
    let st = first_status.unwrap_or(0);
    if st == 0 || st >= 500 {
        Err(JobExchangeFail::Transient)
    } else {
        Err(JobExchangeFail::Rejected)
    }
}

// ── 惰性刷新（对齐 wb_common::ensure_fresh 语义）────────────────────────────

/// PAT 通道判定（疑点① 单测锁定 jt- 换号链路）：access_token 为 pt- 前缀，或
/// pat 备份字段非空。jt- 导入因同时写 pat 字段（qoder_account_import_pat）而
/// 稳定落入 PAT 通道——否则 jt- 作业令牌 24h 过期后掉进客户端通道赌
/// deviceToken/refresh 兼容性，直接 refresh_failed 需手工重导。
fn is_pat_channel(creds: &QoderCreds) -> bool {
    creds.access_token.starts_with("pt-") || !creds.pat.is_empty()
}

/// 惰性刷新：距过期 < lazy_hours 才刷；一次调用最多一次刷新。
/// 返回 (creds, refreshed, note)，note ∈ no_credential/fresh/expired_needs_relogin/
/// refreshed/refreshed_unsaved/refresh_failed/auth_dead/pat_rejected。
///
/// PAT 通道（R-6）：pt- 不被 sash 业务端点接受（实测 401），先经 jobToken 换取
/// 24h 作业令牌；作业令牌临期（< lazy_hours，与客户端通道同语义）或已过期时用
/// 原始 PAT 重换（PAT 长期有效）；调用方传 i64::MAX（401 自愈/恒刷路径）即无条件重换。
pub fn ensure_fresh(
    state: &AppState,
    agent: &ureq::Agent,
    acct_id: &str,
    lazy_hours: i64,
) -> (QoderCreds, bool, &'static str) {
    // H-1 第①步：同一账号并发刷新串行化；持锁后 effective_creds 重读即最新状态——
    // 他人已刷新落库的新令牌直接命中 fresh/复用路径（二次检查），不再重复发起网络刷新
    let lock = refresh_lock_for(acct_id);
    let _refresh_guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    let creds = effective_creds(state, acct_id);
    if creds.access_token.is_empty() {
        return (creds, false, "no_credential");
    }
    let now_ms = chrono::Utc::now().timestamp_millis();
    let is_pat = creds.access_token.starts_with("pt-");
    // PAT 通道判定收编为 is_pat_channel（疑点① 单测锁定 jt- 换号链路）
    // PAT 通道覆盖全部携带 pat 备份的凭证（含作业令牌）：其生命周期由原始 PAT
    // 重换管理（可靠自愈路径），不得掉入客户端通道赌 deviceToken/refresh 对
    // 作业令牌 refresh_token 的兼容性（旧门控 e<=now 使临期窗口误入该通道报 refresh_failed）。
    if is_pat_channel(&creds) {
        // ── PAT 通道：确保作业令牌有效 ──
        let pat = if creds.pat.is_empty() {
            creds.access_token.clone()
        } else {
            creds.pat.clone()
        };
        // 临期窗口消费 lazy_hours（与客户端通道同语义，不再写死 1h）：调用方传
        // i64::MAX（401 自愈/恒刷路径）时 saturating_mul 封顶 → 无条件重换——原硬编码
        // 1h 窗口会把「服务端已吊销但本地未临期」的令牌挡回 fresh，401 自愈失效
        let need_exchange = is_pat
            || creds
                .expires_at_ms
                .is_none_or(|e| e - now_ms < lazy_hours.saturating_mul(3_600_000));
        if !need_exchange {
            return (creds, false, "fresh");
        }
        return match exchange_job_token(agent, &pat, &state.data_dir) {
            Ok(new) => {
                // 落库失败不能静默：否则新作业令牌只存活本轮，下轮仍走 PAT 重换
                if let Err(e) = save_token_store(state, acct_id, &new) {
                    fs_utils::app_log(&state.data_dir, &format!("[qoder] token 落库失败(id={acct_id}, PAT通道): {e}"));
                }
                // §5.10：换取产物为全新 Creds 不含指纹，返回前在内存层补注入账号绑定
                // 指纹（effective_creds 同款合并语义）——否则本轮后续请求丢
                // Cosy-MachineId/Cosy-MachineToken 头。随机 machine_token 不落库
                //（save_token_store 空串不覆盖），保持「每会话现场随机」模型。
                let mut out = new;
                let profile = load_device_profile(state, acct_id);
                super::qoder_device::merge_device_profile(&mut out, profile.as_ref());
                (out, true, "refreshed")
            }
            // 失败分类（审查 P2）：网络/5xx 暂态归 refresh_failed（调度冷却重试可自愈，
            // 此前误报 pat_rejected 会被当成「重试无解」放弃 + 误导用户重导 PAT）；
            // 首通道 4xx 才是 PAT 被拒（永久，需人工）
            Err(JobExchangeFail::Transient) => (creds, false, "refresh_failed"),
            Err(JobExchangeFail::Rejected) => (creds, false, "pat_rejected"),
        };
    }
    // ── 客户端 token 通道：惰性刷新（deviceToken/refresh）──
    // 空 refresh_token 检查统一前置：过期/临期/未知过期（expires_at 缺失）一律无法
    // 客户端刷新。原实现仅在「已过期且带 expires_at」时前置判定，其余空 refresh_token
    // 场景会掉进 refresh_token_once_ex 的空守卫被误报 refresh_failed（暂态），触发无效重试
    if creds.refresh_token.is_empty() {
        if let Some(exp) = creds.expires_at_ms {
            let remain_h = (exp - now_ms) as f64 / 3_600_000.0;
            if remain_h > lazy_hours as f64 {
                return (creds, false, "fresh");
            }
        } else {
            // 无 expires_at 也无 refresh_token：无法判定新鲜度也无法刷新，按需重登
            return (creds, false, "expired_needs_relogin");
        }
        return (creds, false, "expired_needs_relogin");
    }
    if let Some(exp) = creds.expires_at_ms {
        let remain_h = (exp - now_ms) as f64 / 3_600_000.0;
        if remain_h > lazy_hours as f64 {
            return (creds, false, "fresh");
        }
    }
    // P1（对照 wb 分类语义）：区分暂态与永久失败——HTTP 4xx 意味着 refresh_token 已被
    // 服务端永久拒绝（吊销/过期），与网络故障同报 refresh_failed 会被调度器当暂态
    // 无限重试；永久失败回写池 needs_relogin（对照 wb_common::mark_needs_relogin 落库）
    // 并返回 auth_dead，qoder_refresh 等调用方按「需重登」处理、不再重试
    let (new, fail) = refresh_token_once_ex(agent, &creds);
    if let Some(new) = new {
        // §5.10：指纹不落库。refresh_token_once_ex 原样保留注入指纹（返回值供本轮请求带
        // 设备头），但落库前须剥离设备字段——注入值/随机 machine_token 回写 store 后会被
        // creds_of 读作「真实捕获」，污染 merge_device_profile 的优先级①判定，且把
        // 「每会话现场随机」的 machine_token 固化。空串经 save_token_store 跳过 →
        // 不覆盖存量（真实捕获值如有则原样保留），与 PAT 通道落库语义一致。
        let mut persist = new.clone();
        persist.machine_id.clear();
        persist.machine_token.clear();
        // P2 降级标记：落库失败不能仅记日志——刷新端点一次性轮换 refresh_token，
        // 旧 RT 在服务端已作废，本轮新凭证只存活内存，进程重启后无法再刷新，
        // 须重新导入/重登恢复。note=refreshed_unsaved 供调用方与日志可感知
        //（本函数已记 error 日志；不改 NDJSON 事件契约，仅扩展 note 取值）
        let mut note: &'static str = "refreshed";
        if let Err(e) = save_token_store(state, acct_id, &persist) {
            fs_utils::app_log(
                &state.data_dir,
                &format!("[qoder] token 落库失败(id={acct_id}, 客户端通道): {e}"),
            );
            note = "refreshed_unsaved";
        }
        return (new, true, note);
    }
    if fail == RefreshFail::AuthDead {
        mark_needs_relogin(state, acct_id, "客户端通道刷新被拒（refresh_token 已失效）");
        return (creds, false, "auth_dead");
    }
    (creds, false, "refresh_failed")
}

// ── 网关 identity 失败分类（审查 P2：瞬时失败不再永久禁用账号）──────────────
// 网关 qoder_identity 回调（commands/api_server.rs）把 ensure_fresh 的 status 映射为
// Err 文本；暂态失败（refresh_failed）带此前缀，qoder_route 侧据此记 ErrKind::Server
//（熔断：连续 3 错 30m 起指数退避、成功重置，可自愈）而非 SessionDead（永久禁用，
// 此前一次网络抖动即把账号禁用到池热重载/重启）。
pub const TRANSIENT_ERR_TAG: &str = "[qoder-transient]";

/// identity Err 文本是否为暂态失败（前缀标记判定；两侧常量同源，无文案耦合面）
pub fn is_transient_identity_err(e: &str) -> bool {
    e.starts_with(TRANSIENT_ERR_TAG)
}

// ── 账号池回写 ─────────────────────────────────────────────────────────────

/// 刷新成功后回写账号池过期时间与登录态标记（调度/到期数据源）。
/// 供 qoder_checkin / qoder_credits（401 自愈）等通道共用。
pub fn sync_pool_expiry(state: &AppState, aid: &str, creds: &QoderCreds) {
    // I09：直操原始 JSON 保留未知字段（不可走 with_pool_mut），持池锁防并发整池覆盖丢写
    let _guard = state
        .qoder_pool_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut pool: Value = crate::store::docs::qoder_pool_load(&crate::store::db(&state.data_dir));
    let mut changed = false;
    if let Some(accounts) = pool.get_mut("accounts").and_then(Value::as_array_mut) {
        for a in accounts.iter_mut() {
            if a.get("id").and_then(Value::as_str) == Some(aid) {
                a["token_expires_at"] = serde_json::json!(creds.expires_at_ms.map(|ms| ms.div_euclid(1000)));
                a["needs_relogin"] = serde_json::json!(false);
                a["relogin_reason"] = serde_json::json!("");
                changed = true;
            }
        }
    }
    if changed {
        let _ = crate::store::docs::qoder_pool_save(&crate::store::db(&state.data_dir), &pool);
    }
}

/// 标记账号需重新登录（P1，对照 wb_common::mark_needs_relogin 同语义落库）：
/// 客户端通道刷新遇 HTTP 4xx 时 refresh_token 已被服务端永久拒绝（吊销/过期，
/// 一次性轮换下不可恢复），重试无解——回写池 needs_relogin 后，看板/调度跳过
/// 与 qoder_refresh 的「需重登」口径随之生效（reason 为静态描述，不含凭证）。
pub fn mark_needs_relogin(state: &AppState, acct_id: &str, reason: &str) {
    // I09：直操原始 JSON 保留未知字段，持池锁防并发整池覆盖丢写（sync_pool_expiry 同款）
    let _guard = state
        .qoder_pool_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut pool: Value = crate::store::docs::qoder_pool_load(&crate::store::db(&state.data_dir));
    let mut changed = false;
    if let Some(accounts) = pool.get_mut("accounts").and_then(Value::as_array_mut) {
        for a in accounts.iter_mut() {
            if a.get("id").and_then(Value::as_str) == Some(acct_id) {
                a["needs_relogin"] = serde_json::json!(true);
                a["relogin_reason"] = serde_json::json!(reason);
                changed = true;
            }
        }
    }
    if changed {
        let _ = crate::store::docs::qoder_pool_save(&crate::store::db(&state.data_dir), &pool);
        fs_utils::app_log(
            &state.data_dir,
            &format!("[qoder] 账号 {acct_id} 已标记需重新登录（{reason}）"),
        );
    }
}

// ── 账号 id（对齐 wb- 惯例：qd- + sha256[..12]）────────────────────────────

/// 稳定账号 id：同 token 稳定同 id（防换发 token 重复入池）
pub fn account_id_of(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    let digest = h.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("qd-{}", &hex[..12])
}

// ── 跨进程互斥（审查 P1：schtasks CLI 与应用内调度器同刻双进程）──────────────
//
// 进程内轮次锁（QODER_ROUND_LOCK）与每账号刷新锁（REFRESH_LOCKS）对
// 「schtasks CLI（--task-run，main.rs 注释明确绕开单实例插件）+ 应用内调度器
// 同刻（默认同为 10:15）双进程执行」无效：双进程对同账号并发 ensure_fresh →
// 以同一 refresh_token 发起刷新 → 服务端一次性轮换下后到者 4xx → AuthDead →
// 误标 needs_relogin。Windows 命名 Mutex 全局唯一，抢锁失败方幂等跳过
//（签到/刷新均幂等，无损失）；锁名含 data_dir 短哈希（便携版多数据目录互不误伤）。

/// 跨进程锁持有句柄（RAII：Drop 释放；进程崩溃由 OS 回收 Mutex）
#[cfg(windows)]
pub struct CrossProcLock {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

/// 跨进程锁获取失败原因（可观测：创建失败 ≠ 真被占用——2026-10-04 排查教训：
/// 锁名误用多段路径时 CreateMutexW 恒报 ERROR_PATH_NOT_FOUND(3)，旧实现一律
/// 误报「另一进程正在执行」，调度器每分钟空转 skip 且刷新永不执行）
/// 构造点全在 cfg(windows) 的 try_acquire——mac 构建未消费（mac 跨进程互斥
/// 待实装：flock/文件锁，见平台注释）
#[cfg_attr(target_os = "macos", allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrossProcLockFail {
    /// CreateMutexW 失败（内核错误码）——锁机制本身不可用，非他方占用
    CreateFailed(u32),
    /// 对方持有中，等待 wait_ms 超时——真占用，幂等跳过无损失
    Busy,
    /// WaitForSingleObject 系统级失败（内核错误码）
    WaitFailed(u32),
}

impl CrossProcLockFail {
    /// 人类可读描述（调用方拼进 skip 日志，便于一眼区分误报与真占用）
    pub fn describe(&self) -> String {
        match self {
            Self::CreateFailed(e) => format!("锁创建失败（Win32 err={e}，非他方占用）"),
            Self::Busy => "他方进程持有中（等待超时）".to_string(),
            Self::WaitFailed(e) => format!("锁等待系统失败（Win32 err={e}）"),
        }
    }
}

#[cfg(windows)]
impl CrossProcLock {
    /// 尝试在 wait_ms 内获取命名互斥体；成功返回 Some(guard)，失败返回
    /// None + 具体原因（调用方按原因落日志：Busy 幂等跳过，CreateFailed 需人工排查）
    pub fn try_acquire(
        data_dir: &std::path::Path,
        scope: &str,
        wait_ms: u32,
    ) -> (Option<Self>, Option<CrossProcLockFail>) {
        use windows_sys::Win32::Foundation::{
            CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
        };
        use windows_sys::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
        // 锁名含 data_dir 短哈希：AIWORKDATA_DIR 指向不同目录的实例互不干扰。
        // ⚠️ 必须单段名（点分隔）：内核对象命名空间下 `\BaseNamedObjects` 不存在
        // 中间对象目录，多段名 `Global\a\b\c` 直接 STATUS_OBJECT_PATH_NOT_FOUND
        //（Win32 err=3）——2026-10-04 实测复现，曾致锁自合入起从未获取成功
        let mut h = Sha256::new();
        h.update(data_dir.to_string_lossy().as_bytes());
        let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        let name: Vec<u16> = format!("Global\\AIWorkAssistant.qoder.{scope}.{}", &hex[..16])
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY：name 以 NUL 结尾；CreateMutexW 不拥有调用方内存
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        // HANDLE 为 *mut c_void：null 即创建失败
        if handle.is_null() {
            let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
            return (None, Some(CrossProcLockFail::CreateFailed(err)));
        }
        // SAFETY：handle 由 CreateMutexW 返回且非 null
        let waited = unsafe { WaitForSingleObject(handle, wait_ms) };
        if waited == WAIT_OBJECT_0 || waited == WAIT_ABANDONED {
            // WAIT_ABANDONED：前持有进程崩溃退出，所有权已转移给本进程——正常获取
            (Some(Self { handle }), None)
        } else {
            // WAIT_TIMEOUT（对方持有中）/WAIT_FAILED：释放句柄后放弃
            let fail = if waited == WAIT_TIMEOUT {
                CrossProcLockFail::Busy
            } else {
                CrossProcLockFail::WaitFailed(unsafe {
                    windows_sys::Win32::Foundation::GetLastError()
                })
            };
            unsafe { CloseHandle(handle) };
            (None, Some(fail))
        }
    }
}

#[cfg(windows)]
impl Drop for CrossProcLock {
    fn drop(&mut self) {
        // SAFETY：wait 成功（OBJECT_0/ABANDONED）即本线程拥有所有权，ReleaseMutex
        // 释放之。仅 CloseHandle 不 Release 时：若他方进程尚持打开句柄（等待中），
        // 互斥体对象不销毁且所有权仍挂在（可能长期存活的）本线程上——后续竞争者
        // 全部超时假 busy 直到本线程退出
        unsafe { windows_sys::Win32::System::Threading::ReleaseMutex(self.handle) };
        // SAFETY：handle 来自 CreateMutexW 且未被关闭；CloseHandle 释放所有权
        //（Mutex 对象在所有句柄关闭后销毁）
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.handle) };
    }
}

/// 非 Windows 平台占位（本项目仅 Windows 发布，保 CI 单测可编译）
#[cfg(not(windows))]
pub struct CrossProcLock;

#[cfg(not(windows))]
impl CrossProcLock {
    pub fn try_acquire(
        _data_dir: &std::path::Path,
        _scope: &str,
        _wait_ms: u32,
    ) -> (Option<Self>, Option<CrossProcLockFail>) {
        (Some(Self), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 跨进程锁名可用性回归（2026-10-04）：锁名误用多段路径 `Global\a\b\c` 时，
    /// 内核对象命名空间 `\BaseNamedObjects` 下无中间对象目录 → CreateMutexW 恒
    /// ERROR_PATH_NOT_FOUND(3) → try_acquire 永久返回假 busy（日志误报「另一进程
    /// 正在执行」54+ 分钟、刷新任务全天未执行）。修复后：单段名创建必成功；
    /// Windows 互斥体对同线程递归可重入（二次等待立即成功），Drop 释放后句柄清零
    #[test]
    #[cfg(windows)]
    fn cross_proc_lock_name_valid_and_acquirable() {
        let tmp = std::env::temp_dir().join(format!("aiwork-lock-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let (g1, fail1) = CrossProcLock::try_acquire(&tmp, "checkin", 0);
        assert!(g1.is_some(), "首次获取失败: {fail1:?}");
        // 同线程二次等待：Windows 互斥体递归可重入 → 不阻塞立即成功
        let (g2, fail2) = CrossProcLock::try_acquire(&tmp, "checkin", 0);
        assert!(g2.is_some(), "同线程重入失败: {fail2:?}");
        drop(g2);
        drop(g1);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 套餐档位映射：openapi 展示名 / Web 端枚举 / 定价档位名 → 体验/专业/高级/旗舰版
    #[test]
    fn plan_display_name_maps_known_tiers() {
        assert_eq!(plan_display_name("PLAN_TIER_FREE"), "体验版");
        assert_eq!(plan_display_name("Pro Trial"), "体验版");
        assert_eq!(plan_display_name("PLAN_TIER_PRO"), "专业版");
        assert_eq!(plan_display_name("Pro"), "专业版");
        assert_eq!(plan_display_name("PLAN_TIER_PRO_PLUS"), "高级版");
        assert_eq!(plan_display_name("Pro+"), "高级版");
        assert_eq!(plan_display_name("PLAN_TIER_ULTRA"), "旗舰版");
        assert_eq!(plan_display_name("Ultra"), "旗舰版");
        assert_eq!(plan_display_name(""), "");
        // 未识别档位原样透传（如 Teams / Enterprise）
        assert_eq!(plan_display_name("Teams"), "Teams");
        assert_eq!(plan_display_name("Enterprise"), "Enterprise");
    }

    /// 套餐回填门控（集成口径）：行失败恒不查；池内空/待归一原始名才查；
    /// 已归一展示名与未知档位（映射为自身）不查——保证归一只发生一次
    #[test]
    fn need_plan_fetch_门控矩阵() {
        assert!(need_plan_fetch(true, ""), "无套餐必查");
        assert!(need_plan_fetch(true, "Pro Trial"), "openapi 原始展示名待归一");
        assert!(need_plan_fetch(true, "PLAN_TIER_PRO"), "Web 枚举形态待归一");
        assert!(!need_plan_fetch(true, "体验版"), "已归一展示名不再查");
        assert!(!need_plan_fetch(true, "Teams"), "未知档位映射为自身，不重复查");
        assert!(!need_plan_fetch(false, ""), "行失败（401 自愈未果等）不查");
        assert!(!need_plan_fetch(false, "Pro Trial"));
    }

    #[test]
    fn creds_of_parses_flat_and_nested() {
        let flat = creds_of(&json!({"accessToken": "pt-abc", "uid": "u1", "kind": "pat"}));
        assert_eq!(flat.access_token, "pt-abc");
        assert_eq!(flat.uid, "u1");
        assert_eq!(flat.kind, "pat");
        let nested = creds_of(&json!({
            "account": {"uid": "u2", "nickname": "n"},
            "auth": {"access_token": "jt-x", "expiresAtMs": 123}
        }));
        assert_eq!(nested.access_token, "jt-x");
        assert_eq!(nested.uid, "u2");
        assert_eq!(nested.expires_at_ms, Some(123));
    }

    #[test]
    fn account_id_stable_and_prefixed() {
        let a = account_id_of("pt-abc");
        let b = account_id_of("pt-abc");
        assert_eq!(a, b);
        assert!(a.starts_with("qd-"));
        assert_eq!(a.len(), "qd-".len() + 12);
    }

    #[test]
    fn build_auth_headers_carries_cosy_and_device_passthrough() {
        let mut c = QoderCreds { access_token: "pt-x".into(), ..Default::default() };
        let h = build_auth_headers(&c);
        assert!(h.iter().any(|(k, v)| k == "Cosy-ClientType" && v == "10"));
        assert!(!h.iter().any(|(k, _)| k == "Cosy-MachineId"), "设备头缺失时不得伪造");
        c.machine_id = "mid".into();
        c.machine_token = "mtk".into();
        let h = build_auth_headers(&c);
        assert!(h.iter().any(|(k, v)| k == "Cosy-MachineId" && v == "mid"));
        assert!(h.iter().any(|(k, v)| k == "Cosy-MachineToken" && v == "mtk"));
    }

    /// R-6 抓包固化：clientId 为 PAT 派生的稳定 UUID 格式串
    #[test]
    fn job_client_id_stable_uuid_format() {
        let a = job_client_id("pt-abc");
        let b = job_client_id("pt-abc");
        assert_eq!(a, b, "同 PAT 稳定同 clientId");
        assert_ne!(job_client_id("pt-xyz"), a, "不同 PAT 不同 clientId");
        // 8-4-4-4-12 UUID 形态
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
    }

    /// R-6 抓包实测 expires_in=86400000（毫秒，24h）；秒级值兼容
    #[test]
    fn normalize_expires_in_handles_ms_and_seconds() {
        assert_eq!(normalize_expires_in(86_400_000), 86_400_000, "毫秒原样");
        assert_eq!(normalize_expires_in(86_400), 86_400_000, "秒级 ×1000");
        // 阈值边界：30 天秒级恰为分界（>2_592_000 判毫秒原样），其下判秒级 ×1000
        assert_eq!(normalize_expires_in(2_592_000), 2_592_000_000, "30 天秒级 ×1000");
        assert_eq!(normalize_expires_in(2_592_001), 2_592_001, "超阈值判毫秒原样");
        // 1h 秒级（3600）不得被误判为毫秒（3.6e6 ms 恰为 1h 毫秒级下限之下仍安全）
        assert_eq!(normalize_expires_in(3_600_000), 3_600_000, "1h 毫秒原样");
        assert_eq!(normalize_expires_in(3_600), 3_600_000, "1h 秒级 ×1000");
        let now = chrono::Utc::now().timestamp_millis();
        let exp = now + normalize_expires_in(86_400_000);
        let hours = (exp - now) as f64 / 3_600_000.0;
        assert!((hours - 24.0).abs() < 0.01, "24h 窗口");
    }

    /// Q1：刷新失败状态分类——429（限流）/408（超时）为可恢复暂态归 Transient，
    /// 不得触发 mark_needs_relogin 永久停用账号；401/403 及其余 4xx 为凭证被
    /// 服务端永久拒绝归 AuthDead；5xx 服务端故障归 Transient
    #[test]
    fn classify_refresh_status_429_408_transient_rest_4xx_auth_dead() {
        assert_eq!(classify_refresh_status(429), RefreshFail::Transient, "限流暂态");
        assert_eq!(classify_refresh_status(408), RefreshFail::Transient, "超时暂态");
        assert_eq!(classify_refresh_status(401), RefreshFail::AuthDead, "凭证被拒");
        assert_eq!(classify_refresh_status(403), RefreshFail::AuthDead, "凭证被拒");
        assert_eq!(classify_refresh_status(400), RefreshFail::AuthDead, "其余 4xx 永久拒绝");
        assert_eq!(classify_refresh_status(422), RefreshFail::AuthDead, "其余 4xx 永久拒绝");
        assert_eq!(classify_refresh_status(500), RefreshFail::Transient, "5xx 服务端故障");
        assert_eq!(classify_refresh_status(503), RefreshFail::Transient, "5xx 服务端故障");
    }

    /// 无签名刷新端点按刷新令牌前缀分派（agent2api 情报互证）：
    /// jrt- → jobToken/refresh，其余（dt- 系/空）→ deviceToken/refresh；
    /// 分派依据是前缀本身而非「有无 PAT」（PAT 与设备刷新令牌可并存）
    #[test]
    fn refresh_endpoint_dispatch_by_prefix() {
        assert_eq!(
            refresh_endpoint_for("jrt-abc123"),
            "/api/v1/jobToken/refresh",
            "作业令牌刷新令牌走 jobToken 通道"
        );
        assert_eq!(refresh_endpoint_for("dt-xyz"), "/api/v1/deviceToken/refresh");
        assert_eq!(refresh_endpoint_for(""), "/api/v1/deviceToken/refresh", "空值走设备通道缺省");
        assert_eq!(
            refresh_endpoint_for("rt-plain"),
            "/api/v1/deviceToken/refresh",
            "非 jrt- 前缀一律设备通道"
        );
    }

    /// 疑点①（jt- 换号链路，无法实测真实 token 故单测锁定代码行为）：
    /// jt- 导入落库形态 = access_token 与 pat 双写（qoder_account_import_pat），
    /// ensure_fresh 据此落入 PAT 通道走 jobToken 重换——「jt- 仅落 access_token」
    /// 的旧形态必须被判为客户端通道（反例锁定，防止回归）
    #[test]
    fn pat_channel_covers_jt_via_pat_backup_field() {
        // jt- + pat 双写（现行导入形态）→ PAT 通道（jobToken 重换自愈）
        let imported = QoderCreds {
            access_token: "jt-abc".into(),
            pat: "jt-abc".into(),
            kind: "pat".into(),
            ..Default::default()
        };
        assert!(is_pat_channel(&imported), "jt- 双写形态必须走 PAT 通道");
        // 反例锁定：jt- 无 pat 备份 → 客户端通道（正是双写修复前的坏行为）
        let legacy = QoderCreds {
            access_token: "jt-abc".into(),
            ..Default::default()
        };
        assert!(
            !is_pat_channel(&legacy),
            "无 pat 备份的 jt- 不得误入 PAT 通道（需先经 jobToken 换取）"
        );
        // pt- 直接命中 PAT 通道；设备流凭证走客户端通道
        assert!(is_pat_channel(&QoderCreds { access_token: "pt-x".into(), ..Default::default() }));
        assert!(!is_pat_channel(&QoderCreds { access_token: "dt-x".into(), ..Default::default() }));
        assert!(!is_pat_channel(&QoderCreds::default()));
    }

    /// 疑点①：PAT→作业令牌探测端点排序——已实证的 R-6 抓包路径必须居首
    ///（失败分类以首通道状态码为准，且减少无效 404 请求）
    #[test]
    fn job_token_probe_orders_verified_endpoint_first() {
        let attempts = job_token_attempts("pt-test");
        assert_eq!(
            attempts[0].0, "/api/v1/me/jobToken",
            "已实证端点必须居首（失败分类以其状态码为准）"
        );
        assert_eq!(attempts[0].2, "me/jobToken");
        // 首通道 body 携带 PAT 派生的稳定 clientId（R-6 抓包固化）
        assert!(attempts[0].1.get("clientId").and_then(Value::as_str).is_some());
        // 兜底通道仅两条且均指向 exchange（未证实路径，不参与失败分类）
        assert_eq!(attempts[1].0, "/api/v1/jobToken/exchange");
        assert_eq!(attempts[2].0, "/api/v1/jobToken/exchange");
    }

    // ── p3-3 gateway 可行性探针（#[ignore]：cargo test probe_gateway -- --ignored --nocapture）──
    // 背景：gateway.qoder.com.cn 对无/假 Cosy-Key 已实测 403 code=101 "Signature invalid"
    //（model/list，curl 无凭证与假签名两组）。本探针用真实 jobToken（vault）+ 仿 SOLO 抓包头
    // 探测 ①model/list ②chat Encode=0 明文 body 的服务端反应，区分：
    //   101 Signature invalid → 签名强校验（chat 上游死路，转 catalog 观察器方案）
    //   401/403 非签名码     → token 形态被拒（jobToken 不被 gateway 接受）
    //   400/422 参数类       → 签名已过！body schema 问题（可迭代，重大利好）
    //   200/SSE              → 全通；读流前 5 行即主动断开（免费模型 qfmodel 零消耗）
    // 注意：应用运行时 vault 快照可能被锁，凭证读取降级为空（探针报错无害）。

    fn probe_agent() -> ureq::Agent {
        ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(10))
            .timeout_read(std::time::Duration::from_secs(20))
            .build()
    }

    /// 仿 SOLO chat 抓包头（Cosy-Clienttype: 0 通道，抓包 2026-09-30 21:17:40），
    /// Cosy-Key 为占位假值（探测服务端是否真校验签名内容）
    fn probe_cosy_headers(token: &str) -> Vec<(String, String)> {
        let now = chrono::Utc::now().timestamp();
        vec![
            ("Authorization".into(), format!("Bearer {token}")),
            ("Content-Type".into(), "application/json".into()),
            ("Accept".into(), "text/event-stream".into()),
            ("User-Agent".into(), "Go-http-client/1.1".into()),
            ("Cosy-Clienttype".into(), "0".into()),
            ("Cosy-Data-Policy".into(), "AGREE".into()),
            ("Cosy-Date".into(), now.to_string()),
            ("Cosy-Clientip".into(), "169.254.0.70".into()),
            ("Cosy-Key".into(), "cHJvYmVmYWtlZGtleV9ub3Rfc2lnbmF0dXJlX3Rlc3Q=".into()),
            ("Cosy-Machineid".into(), "34303537-3736-432d-a130-30773a35302d".into()),
            ("Cosy-Machineos".into(), "x86_64_windows".into()),
            ("Cosy-Version".into(), "1.32.0".into()),
            ("Login-Version".into(), "v2".into()),
        ]
    }

    /// 池内第一个可用账号 id（探针用，非生产路径）
    fn probe_first_account(state: &AppState) -> String {
        let pool = crate::store::docs::qoder_pool_load(&crate::store::db(&state.data_dir));
        pool.get("accounts")
            .and_then(Value::as_array)
            .and_then(|arr| arr.first())
            .and_then(|a| a.get("id"))
            .and_then(Value::as_str)
            .expect("Qoder 账号池为空或缺 id")
            .to_string()
    }

    fn print_probe_result(tag: &str, result: Result<ureq::Response, ureq::Error>) {
        match result {
            Ok(resp) => {
                println!("[{tag}] HTTP {} content-type={:?}", resp.status(), resp.content_type());
                // 成功情形（SSE 流）：只读前 5 行即主动断开，把推理消耗压到最低
                use std::io::BufRead;
                let reader = std::io::BufReader::new(resp.into_reader());
                for (i, line) in reader.lines().enumerate() {
                    match line {
                        Ok(l) if l.is_empty() => continue,
                        Ok(l) => println!("  << {}", &l[..l.len().min(400)]),
                        Err(e) => { println!("  <read-err> {e}"); break; }
                    }
                    if i >= 4 { println!("  …(探针截断，主动断开)"); break; }
                }
            }
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                println!("[{tag}] HTTP {code}: {}", &body[..body.len().min(500)]);
            }
            Err(e) => println!("[{tag}] NET-ERR: {e}"),
        }
    }

    /// 探针 ①：model/list 带 jobToken + 假 Cosy-Key（对照 curl 无凭证 403/101）
    #[test]
    #[ignore]
    fn probe_gateway_model_list_with_jobtoken() {
        let state = crate::state::AppState::new().expect("构造 AppState 失败");
        let acct_id = probe_first_account(&state);
        let agent = probe_agent();
        let (creds, _r, note) = ensure_fresh(&state, &agent, &acct_id, 24);
        let kind = if creds.access_token.starts_with("pt-") { "pt-" } else { "非pt(作业令牌)" };
        println!("acct = {acct_id}, ensure_fresh = {note}, token 形态 = {kind}");
        assert!(!creds.access_token.is_empty(), "无可用凭证（vault 被锁或池空）");
        let url = "https://gateway.qoder.com.cn/algo/api/v2/model/list";
        let req = probe_cosy_headers(&creds.access_token)
            .into_iter()
            .fold(agent.get(url), |r, (k, v)| r.set(&k, &v));
        print_probe_result("model/list+jobToken", req.call());
    }

    /// 探针 ②：chat Encode=0 明文 OpenAI 风格 body + 假 Cosy-Key + 免费模型 qfmodel
    #[test]
    #[ignore]
    fn probe_gateway_chat_encode0() {
        let state = crate::state::AppState::new().expect("构造 AppState 失败");
        let acct_id = probe_first_account(&state);
        let agent = probe_agent();
        let (creds, _r, note) = ensure_fresh(&state, &agent, &acct_id, 24);
        println!("acct = {acct_id}, ensure_fresh = {note}");
        assert!(!creds.access_token.is_empty(), "无可用凭证（vault 被锁或池空）");
        let url = "https://gateway.qoder.com.cn/algo/api/v2/service/pro/sse/agent_chat_generation?FetchKeys=llm_model_result&AgentId=agent_common&Encode=0";
        let body = json!({
            "model": "qfmodel",
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}]
        });
        let req = probe_cosy_headers(&creds.access_token)
            .into_iter()
            .fold(agent.post(url), |r, (k, v)| r.set(&k, &v));
        print_probe_result("chat+encode0", req.send_string(&body.to_string()));
    }

    /// 探针 ③（p3-2d）：model/list 真 COSY 签名全量重抓。
    /// 背景：抓包日志预览此前截断 8KB，model/list（解压 67856B）只拿到前 12%，
    /// MiniMax-M2.7 与 chat 数组外的其余配置段缺失；256KB 上限修复后需重抓，
    /// 但代理通道要求手动开 GUI——本探针直接用 token store 真凭证 +
    /// build_cosy_headers 真签名（GET 空体）打 gateway，全量 JSON 落盘
    /// `<仓库根>/temp/qoder_model_list_full.json` 供 qoder_upstream 目录核对。
    /// 运行：cargo test probe_gateway_model_list_signed_full -- --ignored --nocapture
    #[test]
    #[ignore]
    fn probe_gateway_model_list_signed_full() {
        let state = crate::state::AppState::new().expect("构造 AppState 失败");
        let acct_id = probe_first_account(&state);
        let agent = probe_agent();
        let (creds, _r, note) = ensure_fresh(&state, &agent, &acct_id, 24);
        println!("acct = {acct_id}, ensure_fresh = {note}");
        assert!(!creds.access_token.is_empty(), "无可用凭证（vault 被锁或池空）");
        let identity = super::super::qoder_sign::CosyIdentity {
            user_id: &creds.uid,
            auth_token: &creds.access_token,
            name: &creds.nickname,
            email: "",
            machine_id: &creds.machine_id,
        };
        let url = "https://gateway.qoder.com.cn/algo/api/v2/model/list";
        let headers = super::super::qoder_sign::build_cosy_headers(None, url, &identity)
            .expect("构造 COSY 签名头失败");
        let req = headers
            .into_iter()
            .fold(agent.get(url), |r, (k, v)| r.set(&k, &v));
        match req.call() {
            Ok(resp) => {
                let status = resp.status();
                let ctype = resp.content_type().to_string();
                let body = resp.into_string().unwrap_or_default();
                println!("[model/list+cosy签名] HTTP {status} content-type={ctype} bytes={}", body.len());
                let out_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../temp/qoder_model_list_full.json");
                if let Some(parent) = out_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::write(&out_path, body.as_bytes()).expect("写盘失败");
                println!("全量 JSON 已落盘: {}", out_path.display());
                // 粗验：chat 数组 + MiniMax 关键字
                let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                let chat_len = v.get("chat").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0);
                println!("chat 数组条目 = {chat_len}; 含 MiniMax = {}", body.contains("MiniMax"));
                assert_eq!(status, 200);
                assert!(chat_len > 0, "响应无 chat 数组");
            }
            Err(ureq::Error::Status(code, resp)) => {
                let body_text = resp.into_string().unwrap_or_default();
                println!("[model/list+cosy签名] HTTP {code}: {}", &body_text[..body_text.len().min(600)]);
                panic!("签名请求失败 HTTP {code}");
            }
            Err(e) => panic!("NET-ERR: {e}"),
        }
    }
}
