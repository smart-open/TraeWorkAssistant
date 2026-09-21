//! 多 API Key 管理与每日配额（T2）+ ck_xxx 子 Key 体系（F-35，批次3）。
//!
//! 数据落盘 `data/api_keys.json`；`daily_limit = 0` 表示不限。
//! 所有 Key 统一在列表中维护（无主/子之分）；未配置任何启用的 Key 时：
//! `auth_disabled = true`（显式关闭鉴权）放行并记为 anonymous，否则拒绝请求（默认）。
//!
//! 记账削峰（网关性能批次 E）：鉴权命中只更新**内存权威副本**并标脏，
//! SQLite 写事务由 flusher（2s）与 stop 时统一落盘——消除「每请求一次整表替换
//! 事务」的写放大与 store 层连接锁争抢。UI 编辑走 `save`（持久化 + 缓存同步刷新），
//! 改动即时生效；进程崩溃最多丢最近 2s 的 used_today 计数（个人场景可接受）。
//!
//! F-35 子 Key 体系（对外子 Key 与上游真实凭证分离）：
//! - `ck_` 前缀子 Key（前端 crypto 随机源生成；旧 `sk-` Key 继续兼容）
//! - `allowed_accounts`：限定上游（WB 上游账号 uid 白名单，空 = 不限）
//! - `schedule_mode`：`expire_first`（默认，临期优先）| `dedicated`（专一，固定
//!   `dedicated_account` 或 allowed_accounts 首个）
//! - `daily_stats`：按日请求统计（保留最近 90 天）

use std::path::Path;

use serde::{Deserialize, Serialize};


/// 数据文件名（位于 data/ 目录）
/// 按日统计保留天数
const DAILY_STATS_CAP: usize = 90;

/// 调度模式：临期优先（默认）
pub const MODE_EXPIRE_FIRST: &str = "expire_first";
/// 调度模式：专一（固定上游账号）
pub const MODE_DEDICATED: &str = "dedicated";

/// 鉴权结果
pub enum KeyCheck {
    /// 命中且已记账，携带约束快照（供 WB 路由层读取上游限定与调度模式）
    Ok(ResolvedKey),
    /// Key 无效或已禁用
    Invalid,
    /// 超出当日配额
    QuotaExceeded { limit: u64 },
}

/// 鉴权通过后的 Key 约束快照（F-35）
#[derive(Clone, Debug)]
pub struct ResolvedKey {
    pub id: String,
    /// 限定上游账号 uid 白名单；空 = 不限
    pub allowed_accounts: Vec<String>,
    /// expire_first | dedicated
    pub schedule_mode: String,
    /// 专一模式绑定的上游账号 uid（空 = allowed_accounts 首个）
    pub dedicated_account: String,
}

/// 子 Key 按日统计项
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct KeyDailyStat {
    pub date: String,
    pub requests: u64,
}

/// API Key 条目
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ApiKeyEntry {
    /// 唯一标识（前端生成）
    pub id: String,
    /// 展示名
    pub name: String,
    /// 实际 Key 值（ck_xxx / sk-...）
    pub key: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 每日请求配额，0 = 不限
    #[serde(default)]
    pub daily_limit: u64,
    /// 创建时间（unix 秒）
    #[serde(default)]
    pub created_at: u64,
    /// 当日用量记账日期（YYYY-MM-DD）
    #[serde(default)]
    pub used_date: String,
    /// 当日已用请求数
    #[serde(default)]
    pub used_today: u64,
    // ── F-35 子 Key 体系（批次3；serde default 兼容旧文件）──
    /// 限定上游账号 uid 白名单（WB 上游 uid；空 = 不限）
    #[serde(default)]
    pub allowed_accounts: Vec<String>,
    /// 调度模式：expire_first（默认）| dedicated
    #[serde(default)]
    pub schedule_mode: String,
    /// 专一模式绑定的上游账号 uid（空 = allowed_accounts 首个）
    #[serde(default)]
    pub dedicated_account: String,
    /// 按日请求统计（升序，保留最近 90 天）
    #[serde(default)]
    pub daily_stats: Vec<KeyDailyStat>,
}

fn default_true() -> bool {
    true
}

/// 数据文件根结构
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ApiKeysFile {
    #[serde(default)]
    pub keys: Vec<ApiKeyEntry>,
    /// 显式关闭鉴权：仅当没有任何启用 Key 时生效（true = 无 Key 放行；默认 false = 无 Key 拒绝）
    #[serde(default)]
    pub auth_disabled: bool,
}

impl Default for ApiKeysFile {
    fn default() -> Self {
        Self { keys: Vec::new(), auth_disabled: false }
    }
}

impl ApiKeyEntry {
    pub fn schedule_mode(&self) -> &str {
        if self.schedule_mode.is_empty() {
            MODE_EXPIRE_FIRST
        } else {
            &self.schedule_mode
        }
    }
}

/// 当日是否仍有配额；跨天自动重置计数
fn quota_left(e: &mut ApiKeyEntry, today: &str) -> Result<(), u64> {
    if e.used_date != today {
        e.used_date = today.to_string();
        e.used_today = 0;
    }
    if e.daily_limit > 0 && e.used_today >= e.daily_limit {
        return Err(e.daily_limit);
    }
    Ok(())
}

/// 按日统计记账：当日项 find-or-insert +1，cap 90 天
fn bump_daily_stats(e: &mut ApiKeyEntry, today: &str) {
    let need_new = e
        .daily_stats
        .last()
        .map(|s| s.date.as_str() != today)
        .unwrap_or(true);
    if need_new {
        e.daily_stats.push(KeyDailyStat {
            date: today.to_string(),
            requests: 0,
        });
        if e.daily_stats.len() > DAILY_STATS_CAP {
            let drop = e.daily_stats.len() - DAILY_STATS_CAP;
            e.daily_stats.drain(0..drop);
        }
    }
    if let Some(s) = e.daily_stats.last_mut() {
        s.requests = s.requests.saturating_add(1);
    }
}

impl ApiKeysFile {
    /// 按呈现的 Key 校验并记账（命中即 +1 + 按日统计）。调用方负责把结果写盘。
    pub fn verify_and_consume(&mut self, presented: &str, today: &str) -> KeyCheck {
        // 常量时间比较（审查 P2-3）：对两侧求 sha256 再比对，避免逐字节提前返回泄露前缀匹配长度
        let digest = |s: &str| {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(s.as_bytes());
            h.finalize()
        };
        let pd = digest(presented);
        let Some(e) = self
            .keys
            .iter_mut()
            .find(|k| k.enabled && digest(&k.key) == pd)
        else {
            return KeyCheck::Invalid;
        };
        if let Err(limit) = quota_left(e, today) {
            return KeyCheck::QuotaExceeded { limit };
        }
        e.used_today += 1;
        bump_daily_stats(e, today);
        KeyCheck::Ok(ResolvedKey {
            id: e.id.clone(),
            allowed_accounts: e.allowed_accounts.clone(),
            schedule_mode: e.schedule_mode().to_string(),
            dedicated_account: e.dedicated_account.clone(),
        })
    }

    /// 是否存在启用的子 Key（用于判断是否需要鉴权）
    pub fn has_enabled(&self) -> bool {
        self.keys.iter().any(|k| k.enabled)
    }
}

/// 按 Key 条目 id 解析约束快照（WB 路由层每流程调用一次；Key 不存在返回 None）。
/// 锁内直查内存权威副本，仅克隆命中的约束字段（避免整表 clone 的锁持有与内存开销）
pub fn constraints_for(data_dir: &Path, key_id: &str) -> Option<ResolvedKey> {
    let mut reg = KEYS_STATE.lock().unwrap_or_else(|e| e.into_inner());
    entry_or_load(&mut reg, data_dir)
        .file
        .keys
        .iter()
        .find(|k| k.id == key_id)
        .map(|e| ResolvedKey {
            id: e.id.clone(),
            allowed_accounts: e.allowed_accounts.clone(),
            schedule_mode: e.schedule_mode().to_string(),
            dedicated_account: e.dedicated_account.clone(),
        })
}

/// 进程级状态锁 + 内存权威副本（批次 E）：api_keys 的「读-改-写」（verify 记账 +
/// save + flush）全部在同一把锁内完成，保证并发请求计数原子，且替代原
/// 「每请求 load 全表 + 整表替换事务落盘」的写放大（P1-2 / 批次 E）。
static KEYS_STATE: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, KeysEntry>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// 单个 data_dir 的缓存条目：内存权威副本 + 待落盘脏标记
struct KeysEntry {
    file: ApiKeysFile,
    dirty: bool,
}

/// 锁内取条目：缓存未命中时从 SQLite 加载并插入（首读即缓存）
fn entry_or_load<'a>(
    reg: &'a mut std::collections::HashMap<std::path::PathBuf, KeysEntry>,
    data_dir: &'a Path,
) -> &'a mut KeysEntry {
    reg.entry(data_dir.to_path_buf()).or_insert_with(|| KeysEntry {
        file: crate::store::docs::api_keys_load(&crate::store::db(data_dir)),
        dirty: false,
    })
}

/// 读（缓存优先；命令层展示与 constraints_for 均走此处，不再每调用一次 SQLite 读）
pub fn load(data_dir: &Path) -> ApiKeysFile {
    let mut reg = KEYS_STATE.lock().unwrap_or_else(|e| e.into_inner());
    entry_or_load(&mut reg, data_dir).file.clone()
}

/// 持久化并同步刷新内存权威副本（UI 编辑路径：改动即时生效）。
/// 计数字段（used_today/used_date/daily_stats）以内存权威副本为准合并——前端快照
/// 常携带打开设置页时的旧计数，直接覆盖会系统性回退当日记账（R2；同时消除
/// save 与 flusher 锁外写库交错的窄竞态）。合并后标脏，flusher 兜底再落一次盘
/// 收敛交错窗口；落盘失败保留脏标记由 flusher 重试（不静默丢编辑）。
pub fn save(data_dir: &Path, f: &ApiKeysFile) {
    let merged = {
        let mut reg = KEYS_STATE.lock().unwrap_or_else(|e| e.into_inner());
        let entry = entry_or_load(&mut reg, data_dir);
        let mut nf = f.clone();
        for k in &mut nf.keys {
            if let Some(cur) = entry.file.keys.iter().find(|c| c.id == k.id) {
                k.used_today = cur.used_today;
                k.used_date = cur.used_date.clone();
                k.daily_stats = cur.daily_stats.clone();
            }
        }
        entry.file = nf.clone();
        entry.dirty = true;
        nf
    };
    if crate::store::docs::api_keys_save(&crate::store::db(data_dir), &merged).is_err() {
        eprintln!("[api_keys] save 落盘失败（内存已生效，flusher 将重试）: {}", data_dir.display());
    }
}

/// 鉴权记账原子操作：锁内 verify_and_consume（内存副本），命中即标脏；
/// SQLite 落盘由 flusher（2s）/ stop 时 flush_dirty 统一执行。
/// auth 中间件每请求调用本函数，禁止绕开 KEYS_STATE 直接 load+save。
pub fn verify_and_consume_locked(data_dir: &Path, presented: &str, today: &str) -> KeyCheck {
    let mut reg = KEYS_STATE.lock().unwrap_or_else(|e| e.into_inner());
    let entry = entry_or_load(&mut reg, data_dir);
    let r = entry.file.verify_and_consume(presented, today);
    // 仅记账命中（Ok）产生持久化需求；Invalid / QuotaExceeded（提前返回不改计数）无写盘
    if matches!(r, KeyCheck::Ok(_)) {
        entry.dirty = true;
    }
    r
}

/// 落盘脏副本（flusher 线程 2s 一次 + stop / 退出时调用）；成功落盘返回 true，
/// 无脏副本或落盘失败返回 false。锁内只取快照，SQLite 写事务在锁外执行；
/// 写失败恢复脏标记由 flusher 下轮重试（R3，不静默丢当批计数）
pub fn flush_dirty(data_dir: &Path) -> bool {
    let snapshot = {
        let mut reg = KEYS_STATE.lock().unwrap_or_else(|e| e.into_inner());
        match reg.get_mut(&data_dir.to_path_buf()) {
            Some(e) if e.dirty => {
                e.dirty = false;
                Some(e.file.clone())
            }
            _ => None,
        }
    };
    match snapshot {
        Some(f) => {
            let ok = crate::store::docs::api_keys_save(&crate::store::db(data_dir), &f).is_ok();
            if !ok {
                let mut reg = KEYS_STATE.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(e) = reg.get_mut(&data_dir.to_path_buf()) {
                    e.dirty = true;
                }
                eprintln!("[api_keys] flush 落盘失败（已恢复脏标记待重试）: {}", data_dir.display());
            }
            ok
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, key: &str, enabled: bool, limit: u64) -> ApiKeyEntry {
        ApiKeyEntry {
            id: id.into(),
            name: id.into(),
            key: key.into(),
            enabled,
            daily_limit: limit,
            created_at: 0,
            used_date: String::new(),
            used_today: 0,
            allowed_accounts: vec![],
            schedule_mode: String::new(),
            dedicated_account: String::new(),
            daily_stats: vec![],
        }
    }

    #[test]
    fn verify_matches_enabled_key_and_counts() {
        let mut f = ApiKeysFile {
            keys: vec![entry("k1", "ck-a", true, 0)],
            auth_disabled: false,
        };
        assert!(matches!(f.verify_and_consume("ck-a", "2026-09-09"), KeyCheck::Ok(r) if r.id == "k1"));
        assert_eq!(f.keys[0].used_today, 1);
        assert_eq!(f.keys[0].used_date, "2026-09-09");
        assert_eq!(f.keys[0].daily_stats.len(), 1);
        assert_eq!(f.keys[0].daily_stats[0].requests, 1);
    }

    #[test]
    fn verify_rejects_disabled_or_unknown() {
        let mut f = ApiKeysFile {
            keys: vec![entry("k1", "ck-a", false, 0), entry("k2", "ck-b", true, 0)],
            auth_disabled: false,
        };
        assert!(matches!(f.verify_and_consume("ck-a", "d"), KeyCheck::Invalid));
        assert!(matches!(f.verify_and_consume("ck-c", "d"), KeyCheck::Invalid));
    }

    #[test]
    fn quota_blocks_at_limit_and_resets_next_day() {
        let mut f = ApiKeysFile {
            keys: vec![entry("k1", "ck-a", true, 2)],
            auth_disabled: false,
        };
        assert!(matches!(f.verify_and_consume("ck-a", "d1"), KeyCheck::Ok(_)));
        assert!(matches!(f.verify_and_consume("ck-a", "d1"), KeyCheck::Ok(_)));
        assert!(matches!(
            f.verify_and_consume("ck-a", "d1"),
            KeyCheck::QuotaExceeded { limit: 2 }
        ));
        // 跨天重置
        assert!(matches!(f.verify_and_consume("ck-a", "d2"), KeyCheck::Ok(_)));
        assert_eq!(f.keys[0].used_today, 1);
        // 按日统计：两天各一项
        assert_eq!(f.keys[0].daily_stats.len(), 2);
        assert_eq!(f.keys[0].daily_stats[1].requests, 1);
    }

    #[test]
    fn daily_stats_capped_at_90() {
        let mut f = ApiKeysFile {
            keys: vec![entry("k1", "ck-a", true, 0)],
            auth_disabled: false,
        };
        for i in 0..120 {
            let date = format!("d{i}");
            let _ = f.verify_and_consume("ck-a", &date);
        }
        assert_eq!(f.keys[0].daily_stats.len(), 90);
        assert_eq!(f.keys[0].daily_stats[0].date, "d30");
    }

    #[test]
    fn resolved_key_defaults_and_constraints() {
        let mut e = entry("k1", "ck-a", true, 0);
        assert_eq!(e.schedule_mode(), MODE_EXPIRE_FIRST);
        e.schedule_mode = MODE_DEDICATED.into();
        e.allowed_accounts = vec!["wb-1".into()];
        assert_eq!(e.schedule_mode(), MODE_DEDICATED);
    }

    #[test]
    fn roundtrip_with_defaults() {
        let dir = std::env::temp_dir().join(format!("twa_keys_test_{}", std::process::id()));
        let mut f = ApiKeysFile::default();
        let mut e = entry("k1", "ck-x", true, 5);
        e.allowed_accounts = vec!["wb-9".into()];
        e.schedule_mode = MODE_DEDICATED.into();
        e.dedicated_account = "wb-9".into();
        f.keys.push(e);
        save(&dir, &f);
        let loaded = load(&dir);
        // 非空文件原样读盘
        assert_eq!(loaded.keys.len(), 1);
        assert_eq!(loaded.keys[0].daily_limit, 5);
        assert_eq!(loaded.keys[0].allowed_accounts, vec!["wb-9"]);
        assert_eq!(loaded.keys[0].schedule_mode(), MODE_DEDICATED);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ==================== 批次 E：内存权威副本 + 延迟落盘 ====================

    #[test]
    fn flush_persists_only_after_consume() {
        // Invalid 路径不标脏；命中记账标脏，flush_dirty 落库后存储可见
        let dir = std::env::temp_dir().join(format!("twa_keys_flush_{}", std::process::id()));
        let f = ApiKeysFile {
            keys: vec![entry("k1", "ck-a", true, 0)],
            auth_disabled: false,
        };
        save(&dir, &f);
        // R2：save 合并后标脏，由 flusher 兜底收敛一次
        assert!(flush_dirty(&dir), "save 标脏由 flusher 收敛");
        assert!(!flush_dirty(&dir), "收敛后无脏标记不应落盘");
        assert!(matches!(
            verify_and_consume_locked(&dir, "ck-wrong", "d1"),
            KeyCheck::Invalid
        ));
        assert!(!flush_dirty(&dir), "Invalid 路径不应标脏");
        assert!(matches!(
            verify_and_consume_locked(&dir, "ck-a", "d1"),
            KeyCheck::Ok(_)
        ));
        // 落盘前存储仍是旧值、内存副本已是新值（缓存权威语义）
        // 注意 store 用 rows_replace 写 SQLite，不能用 kv_get_raw 验证；
        // 直接调 api_keys_load（从 rows 读）验证存储层是否可见
        let mid_store = crate::store::docs::api_keys_load(&crate::store::db(&dir));
        assert_eq!(mid_store.keys[0].used_today, 0, "flush 前不应写库");
        assert_eq!(load(&dir).keys[0].used_today, 1, "内存副本已记账");
        // flush 后存储与内存一致
        assert!(flush_dirty(&dir), "记账后应标脏并落盘");
        let updated = crate::store::docs::api_keys_load(&crate::store::db(&dir));
        assert_eq!(updated.keys[0].used_today, 1, "flush 后应写库");
        assert!(!flush_dirty(&dir), "flush 后脏标记清除");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_refreshes_cache_immediately() {
        // UI 编辑路径：save 持久化 + 缓存同步刷新，load 立即读到新值
        let dir = std::env::temp_dir().join(format!("twa_keys_save_cache_{}", std::process::id()));
        let mut f = ApiKeysFile {
            keys: vec![entry("k1", "ck-a", true, 0)],
            auth_disabled: false,
        };
        save(&dir, &f);
        assert_eq!(load(&dir).keys.len(), 1);
        // R2 计数合并：save 携带旧快照（used_today=0）不得回退内存已记账的计数
        let _ = verify_and_consume_locked(&dir, "ck-a", "d1");
        f.keys[0].name = "renamed".into();
        save(&dir, &f);
        let loaded = load(&dir);
        assert_eq!(loaded.keys[0].name, "renamed", "编辑字段生效");
        assert_eq!(loaded.keys[0].used_today, 1, "旧快照不得回退内存计数");
        assert_eq!(loaded.keys[0].used_date, "d1");
        // save 合并后标脏 → flusher 兜底落一次盘收敛（再 flush 应无脏）
        assert!(flush_dirty(&dir), "save 后应标脏由 flusher 收敛");
        let stored = crate::store::docs::api_keys_load(&crate::store::db(&dir));
        assert_eq!(stored.keys[0].name, "renamed");
        assert_eq!(stored.keys[0].used_today, 1, "落盘含合并后的计数");
        assert!(!flush_dirty(&dir), "收敛后脏标记清除");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
