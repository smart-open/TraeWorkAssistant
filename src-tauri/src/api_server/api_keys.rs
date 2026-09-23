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
//! - `allowed_accounts`：限定上游账号白名单，空 = 不限。
//!   issue #30 混合白名单：bind=""（跟随全局调度）时条目可带 `trae:`/`buddy:`
//!   池前缀（按前缀作用域分池，某池零条目 = 该池排除）；旧数据全裸 uid 保持
//!   旧语义（仅约束 WB 池）；绑定池时为对应池裸 uid
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

/// 鉴权通过后的 Key 约束快照（F-35 + 资源池绑定 + issue #30 混合白名单）
#[derive(Clone, Debug)]
pub struct ResolvedKey {
    pub id: String,
    /// 限定上游账号 uid 白名单；空 = 不限。
    /// bind=""（跟随全局调度）时条目可带池前缀 `trae:`/`buddy:`（issue #30 混合
    /// 白名单）；绑定池时为对应池裸 uid；旧数据全裸 uid = 仅约束 buddy 池
    pub allowed_accounts: Vec<String>,
    /// expire_first | dedicated
    pub schedule_mode: String,
    /// 专一模式绑定的上游账号 uid（空 = allowed_accounts 首个）；混合白名单时
    /// 同样可带池前缀（专一锁定其归属池，排除另一池）
    pub dedicated_account: String,
    /// 资源池绑定："" = 跟随全局调度 | "trae" | "buddy"
    pub bind_pool: String,
}

/// 解析池前缀标记（issue #30）：`"trae:x"` → (Some("trae"), "x")；裸 uid → (None, 原串)。
/// 大小写不敏感；非法前缀（如 "openai:x"）不识别，整体视为裸 uid。
/// 池标识为 &'static（归一化字面量），第二个引用绑定输入串生命周期
pub fn split_pool_tagged(uid: &str) -> (Option<&'static str>, &str) {
    if let Some((tag, rest)) = uid.split_once(':') {
        match parse_bind_pool(tag) {
            Some(p) => return (Some(p), rest),
            None => return (None, uid),
        }
    }
    (None, uid)
}

/// 单池生效约束（pool_constraints 返回值）
#[derive(Clone, Debug)]
pub struct PoolConstraint {
    /// 白名单集合：None = 不限；Some(空集) = 该池被该 Key 排除
    pub allowed: Option<std::collections::HashSet<String>>,
    /// 该池专一账号 uid（已去前缀；取号侧优先锁定不让位）
    pub dedicated: Option<String>,
}

impl ResolvedKey {
    /// 返回绑定的池标识（归一化后 Some("trae")/Some("buddy")；空或非法 = None）
    pub fn bind_pool(&self) -> Option<&'static str> {
        parse_bind_pool(&self.bind_pool)
    }

    /// 混合白名单判定（issue #30）：allowed_accounts 中任一条目带池前缀。
    /// 新前端在 bind="" 下保存的列表必带前缀；旧数据全裸 uid = 旧语义
    pub fn is_mixed_whitelist(&self) -> bool {
        self.allowed_accounts.iter().any(|u| split_pool_tagged(u).0.is_some())
    }

    /// 该 Key 的 F-35 约束（allowed_accounts 白名单 + dedicated）是否作用于指定池。
    /// 作用域规则（bind_pool 决定白名单所指的账号体系）：
    /// - bind=""（未绑定）→ 仅约束 buddy 池（F-35 旧语义兼容：白名单历来只作用于
    ///   WB 上游；绝不能波及 trae，否则旧 Key 的 Trae 侧调度会被误过滤）
    /// - bind="trae" → 仅约束 trae 池（白名单/专一指 Trae 账号 uid）
    /// - bind="buddy" → 仅约束 buddy 池
    /// - 混合白名单（issue #30）→ 各池按前缀条目分别约束（见 pool_constraints）
    pub fn constrains_pool(&self, pool: &str) -> bool {
        match self.bind_pool() {
            None => pool == "buddy",
            Some(p) => p == pool,
        }
    }

    /// 指定池的生效约束（issue #30 统一入口）。
    /// 返回 None = 该 Key 不约束此池；Some(PoolConstraint)：
    /// - `allowed` None = 本池不限；Some(空集) = 该池被该 Key 排除（预检判不健康
    ///   走 fallback，取号必失败兜底）；Some(非空) = 白名单过滤
    /// - `dedicated` = 该池专一账号 uid（已去前缀，取号侧优先锁定不让位）
    ///
    /// 作用域规则：
    /// - 白名单与专一账号全空 → None（不限）
    /// - 混合白名单（任一条目带 `trae:`/`buddy:` 前缀）→ 按前缀作用域分池：
    ///   本池条目（裸 uid 兜底视为 buddy，防御旧数据混入）构成白名单；
    ///   专一模式锁定归属池（归属池白名单并入专一账号，保证预检健康集与取号
    ///   dedicated 优先语义一致；另一池整体排除）
    /// - 否则旧语义 → `constrains_pool` 为 false 返回 None；true 返回约束，
    ///   其中白名单可空（空白名单 + 专一 = 不限账号但锁定专一，F-35 原语义）
    pub fn pool_constraints(&self, pool: &str) -> Option<PoolConstraint> {
        use std::collections::HashSet;
        if self.allowed_accounts.is_empty() && self.dedicated_account.is_empty() {
            return None;
        }
        // 专一模式生效的专一账号（空 → allowed 首个），解析归属池与裸 uid
        let dedicated = if self.schedule_mode == MODE_DEDICATED {
            let raw = if self.dedicated_account.is_empty() {
                self.allowed_accounts.first().cloned().unwrap_or_default()
            } else {
                self.dedicated_account.clone()
            };
            let (p, u) = split_pool_tagged(&raw);
            let u = u.to_string();
            (!u.is_empty()).then(|| (p.unwrap_or("buddy"), u))
        } else {
            None
        };
        if self.is_mixed_whitelist() {
            // 混合白名单：按前缀作用域到指定池（裸 uid 防御性归属 buddy）
            let scoped: HashSet<String> = self
                .allowed_accounts
                .iter()
                .filter(|u| match split_pool_tagged(u) {
                    (Some(p), _) => p == pool,
                    (None, _) => pool == "buddy",
                })
                .map(|u| split_pool_tagged(u).1.to_string())
                .collect();
            match dedicated {
                // 归属池：白名单并入专一账号——预检健康集与取号「dedicated 优先
                // 锁定」一致，避免「专一账号不在勾选集 + 其余全不健康」时被误杀
                Some((dpool, duid)) if dpool == pool => {
                    let mut allowed = scoped;
                    allowed.insert(duid.clone());
                    Some(PoolConstraint { allowed: Some(allowed), dedicated: Some(duid) })
                }
                // 专一锁池：非归属池整体排除（预检判不健康 → 走归属池）
                Some(_) => {
                    Some(PoolConstraint { allowed: Some(HashSet::new()), dedicated: None })
                }
                None => Some(PoolConstraint { allowed: Some(scoped), dedicated: None }),
            }
        } else if self.constrains_pool(pool) {
            // 旧语义：constrains_pool 已判定本池受约束；白名单可空（issue #30
            // 回归修复：空白名单 + 专一时 allowed=None 不限、dedicated 仍锁定，
            // 与重构前 wb_route/trae_pool_constraints 的独立提取行为一致）
            let allowed = (!self.allowed_accounts.is_empty())
                .then(|| self.allowed_accounts.iter().cloned().collect());
            Some(PoolConstraint { allowed, dedicated: dedicated.map(|(_, u)| u) })
        } else {
            None
        }
    }
}

/// 解析 bind_pool 字段为静态池标识（空/非法→None）
pub fn parse_bind_pool(s: &str) -> Option<&'static str> {
    match s.trim().to_lowercase().as_str() {
        "trae" => Some("trae"),
        "buddy" => Some("buddy"),
        _ => None,
    }
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
    /// 资源池绑定（issue #25）："" = 跟随全局调度 | "trae" | "buddy"。
    /// 绑定后选池优先级最高（覆盖会话粘性与全局策略），池内约束（白名单/专一）
    /// 随绑定池切换账号体系
    #[serde(default)]
    pub bind_pool: String,
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
            bind_pool: e.bind_pool.clone(),
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
            bind_pool: e.bind_pool.clone(),
        })
}

/// 按 Key 条目 id 取展示名（请求日志用）：走同一内存权威副本，零磁盘 IO；
/// Key 不存在或匿名返回空串（日志侧显示 "-"）
pub fn key_name_for(data_dir: &Path, key_id: &str) -> String {
    let mut reg = KEYS_STATE.lock().unwrap_or_else(|e| e.into_inner());
    entry_or_load(&mut reg, data_dir)
        .file
        .keys
        .iter()
        .find(|k| k.id == key_id)
        .map(|e| e.name.clone())
        .unwrap_or_default()
}

/// 按 Key 条目 id 一次性取（展示名, 约束快照）：单锁单遍历合并查询，
/// 供诊断路径（no_healthy_detail）替代 constraints_for + key_name_for 的两次加锁；
/// Key 不存在返回 None（匿名/未知 Key 由调用方兜底仅报池健康数）
pub fn key_snapshot_for(data_dir: &Path, key_id: &str) -> Option<(String, ResolvedKey)> {
    let mut reg = KEYS_STATE.lock().unwrap_or_else(|e| e.into_inner());
    entry_or_load(&mut reg, data_dir)
        .file
        .keys
        .iter()
        .find(|k| k.id == key_id)
        .map(|e| {
            (
                e.name.clone(),
                ResolvedKey {
                    id: e.id.clone(),
                    allowed_accounts: e.allowed_accounts.clone(),
                    schedule_mode: e.schedule_mode().to_string(),
                    dedicated_account: e.dedicated_account.clone(),
                    bind_pool: e.bind_pool.clone(),
                },
            )
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
            bind_pool: String::new(),
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
    fn bind_pool_parse_and_pool_scope() {
        // 未绑定：跟随全局调度，F-35 约束仅作用于 buddy 池（旧语义兼容）
        let mut e = entry("k1", "ck-a", true, 0);
        assert_eq!(parse_bind_pool(&e.bind_pool), None);
        let mut rk = constraints_of(&e);
        assert_eq!(rk.bind_pool(), None);
        assert!(rk.constrains_pool("buddy"), "旧 Key 白名单仍仅作用 buddy");
        assert!(!rk.constrains_pool("trae"), "旧 Key 白名单绝不能波及 trae");

        // 绑定 trae：约束切换到 trae 池
        e.bind_pool = "Trae".into(); // 大小写归一
        rk = constraints_of(&e);
        assert_eq!(rk.bind_pool(), Some("trae"));
        assert!(rk.constrains_pool("trae"));
        assert!(!rk.constrains_pool("buddy"));

        // 绑定 buddy
        e.bind_pool = "buddy".into();
        rk = constraints_of(&e);
        assert_eq!(rk.bind_pool(), Some("buddy"));
        assert!(rk.constrains_pool("buddy"));
        assert!(!rk.constrains_pool("trae"));

        // 非法值视为未绑定
        e.bind_pool = "openai".into();
        assert_eq!(parse_bind_pool(&e.bind_pool), None);
    }

    /// 从条目构造约束快照（模拟 verify_and_consume / constraints_for 的字段拷贝）
    fn constraints_of(e: &ApiKeyEntry) -> ResolvedKey {
        ResolvedKey {
            id: e.id.clone(),
            allowed_accounts: e.allowed_accounts.clone(),
            schedule_mode: e.schedule_mode().to_string(),
            dedicated_account: e.dedicated_account.clone(),
            bind_pool: e.bind_pool.clone(),
        }
    }

    #[test]
    fn roundtrip_preserves_bind_pool() {
        let dir = std::env::temp_dir().join(format!("twa_keys_bind_{}", std::process::id()));
        let mut f = ApiKeysFile::default();
        let mut e = entry("k1", "ck-x", true, 0);
        e.bind_pool = "trae".into();
        f.keys.push(e);
        save(&dir, &f);
        let loaded = load(&dir);
        assert_eq!(loaded.keys[0].bind_pool, "trae");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn serde_compat_old_entry_without_bind_pool() {
        // 旧版本落盘 JSON 无 bind_pool 字段：serde default 反序列化为空串（跟随全局调度）
        let old = r#"{"id":"k1","name":"旧 Key","key":"ck-x","enabled":true,
                      "daily_limit":5,"created_at":1,"used_date":"2026-01-01","used_today":2}"#;
        let e: ApiKeyEntry = serde_json::from_str(old).unwrap();
        assert_eq!(e.bind_pool, "");
        assert_eq!(parse_bind_pool(&e.bind_pool), None);
        assert_eq!(e.schedule_mode(), MODE_EXPIRE_FIRST);
        // 未来版本新增未知字段：serde 默认忽略，不报错
        let fut = r#"{"id":"k2","name":"n","key":"ck-y","enabled":true,"future_field":1}"#;
        let e2: ApiKeyEntry = serde_json::from_str(fut).unwrap();
        assert_eq!(e2.bind_pool, "");
    }

    #[test]
    fn verify_and_consume_returns_bind_pool() {
        // 鉴权记账返回的约束快照必须携带 bind_pool（供 dispatch/wb_route 读取）
        let mut f = ApiKeysFile::default();
        let mut e = entry("k1", "ck-a", true, 0);
        e.bind_pool = "buddy".into();
        e.allowed_accounts = vec!["b1".into()];
        f.keys.push(e);
        match f.verify_and_consume("ck-a", "2026-09-22") {
            KeyCheck::Ok(rk) => {
                assert_eq!(rk.bind_pool(), Some("buddy"));
                assert!(rk.constrains_pool("buddy"));
                assert!(!rk.constrains_pool("trae"));
            }
            _ => panic!("命中 Key 应返回 Ok"),
        }
        // 未绑定 Key：快照 bind_pool 为空串，constrains_pool 维持旧语义（仅 WB 池）
        let mut f2 = ApiKeysFile::default();
        f2.keys.push(entry("k2", "ck-b", true, 0));
        match f2.verify_and_consume("ck-b", "2026-09-22") {
            KeyCheck::Ok(rk) => {
                assert_eq!(rk.bind_pool(), None);
                assert!(rk.constrains_pool("buddy"));
                assert!(!rk.constrains_pool("trae"));
            }
            _ => panic!("命中 Key 应返回 Ok"),
        }
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

    // ==================== issue #30 混合白名单 ====================

    #[test]
    fn split_pool_tagged_parses_prefix() {
        assert_eq!(split_pool_tagged("trae:t1"), (Some("trae"), "t1"));
        assert_eq!(split_pool_tagged("Trae:t1"), (Some("trae"), "t1"));
        assert_eq!(split_pool_tagged("buddy:b1"), (Some("buddy"), "b1"));
        // 裸 uid / 非法前缀（"openai:x" 中 openai 非法 → 整串视为裸 uid）
        assert_eq!(split_pool_tagged("t1"), (None, "t1"));
        assert_eq!(split_pool_tagged("openai:x"), (None, "openai:x"));
    }

    #[test]
    fn pool_constraints_empty_whitelist_is_none() {
        let mut e = entry("k1", "ck-a", true, 0);
        e.allowed_accounts = vec![];
        e.dedicated_account = String::new();
        let rk = constraints_of(&e);
        assert!(rk.pool_constraints("trae").is_none());
        assert!(rk.pool_constraints("buddy").is_none());
    }

    #[test]
    fn pool_constraints_empty_whitelist_keeps_dedicated() {
        // 回归修复（审查 #1）：空白名单 + 专一 → allowed=None 不限、dedicated 仍锁定
        // （旧 wb_route/trae_pool_constraints 独立提取语义），不得被「白名单空 → None」吞掉
        let mut e = entry("k1", "ck-a", true, 0);
        e.schedule_mode = MODE_DEDICATED.into();
        e.allowed_accounts = vec![];
        e.dedicated_account = "b9".into();
        let rk = constraints_of(&e);
        // bind=""：白名单仅作用 buddy 池（F-35），buddy=(不限, b9)、trae 不受限
        let c = rk.pool_constraints("buddy").expect("buddy 专一约束应保留");
        assert!(c.allowed.is_none(), "空白名单 = 不限，不得变成空集排除");
        assert_eq!(c.dedicated.as_deref(), Some("b9"));
        assert!(rk.pool_constraints("trae").is_none());

        // bind="trae"：约束转到 trae 池
        e.bind_pool = "trae".into();
        e.dedicated_account = "t9".into();
        let rk = constraints_of(&e);
        let c = rk.pool_constraints("trae").expect("trae 专一约束应保留");
        assert!(c.allowed.is_none());
        assert_eq!(c.dedicated.as_deref(), Some("t9"));
        assert!(rk.pool_constraints("buddy").is_none());
    }

    #[test]
    fn pool_constraints_legacy_unbound_only_buddy() {
        // 旧数据全裸 uid + bind=""：仅约束 buddy 池，trae 不受限（F-35 红线）
        let mut e = entry("k1", "ck-a", true, 0);
        e.allowed_accounts = vec!["b1".into()];
        let rk = constraints_of(&e);
        let c = rk.pool_constraints("buddy").expect("buddy 应被约束");
        assert!(c.allowed.is_some_and(|a| a.contains("b1") && a.len() == 1));
        assert!(c.dedicated.is_none());
        assert!(rk.pool_constraints("trae").is_none(), "旧 Key 白名单不得波及 trae");
    }

    #[test]
    fn pool_constraints_bound_pools_unchanged() {
        // 绑定 trae：裸 uid 约束 trae；buddy 不受限
        let mut e = entry("k1", "ck-a", true, 0);
        e.bind_pool = "trae".into();
        e.allowed_accounts = vec!["t1".into(), "t2".into()];
        e.schedule_mode = MODE_DEDICATED.into();
        e.dedicated_account = "t2".into();
        let rk = constraints_of(&e);
        let c = rk.pool_constraints("trae").expect("trae 应被约束");
        let a = c.allowed.expect("白名单应为 Some");
        assert!(a.contains("t1") && a.contains("t2"));
        assert_eq!(c.dedicated.as_deref(), Some("t2"));
        assert!(rk.pool_constraints("buddy").is_none());

        // 绑定 buddy：对称
        e.bind_pool = "buddy".into();
        e.allowed_accounts = vec!["b1".into()];
        e.dedicated_account = String::new();
        let rk = constraints_of(&e);
        let c = rk.pool_constraints("buddy").expect("buddy 应被约束");
        assert!(c.allowed.is_some_and(|a| a.contains("b1")));
        assert_eq!(c.dedicated.as_deref(), Some("b1"), "专一空 dedicated 回退 allowed 首个");
        assert!(rk.pool_constraints("trae").is_none());
    }

    #[test]
    fn pool_constraints_mixed_scopes_per_pool() {
        // 混合白名单：trae/buddy 前缀条目各自作用域
        let mut e = entry("k1", "ck-a", true, 0);
        e.allowed_accounts = vec!["trae:t1".into(), "buddy:b1".into(), "buddy:b2".into()];
        let rk = constraints_of(&e);
        assert!(rk.is_mixed_whitelist());

        let ct = rk.pool_constraints("trae").expect("trae 应被约束");
        assert!(ct.allowed.is_some_and(|a| a.contains("t1") && a.len() == 1));
        assert!(ct.dedicated.is_none());

        let cb = rk.pool_constraints("buddy").expect("buddy 应被约束");
        assert!(cb.allowed.is_some_and(|a| a.contains("b1") && a.contains("b2") && a.len() == 2));
        assert!(cb.dedicated.is_none());
    }

    #[test]
    fn pool_constraints_mixed_missing_pool_excluded() {
        // 混合白名单只勾 trae：buddy 零条目 = 排除（Some(空集)，非 None 不限）
        let mut e = entry("k1", "ck-a", true, 0);
        e.allowed_accounts = vec!["trae:t1".into()];
        let rk = constraints_of(&e);
        let ct = rk.pool_constraints("trae").expect("trae 应被约束");
        assert!(ct.allowed.is_some_and(|a| a.contains("t1")));
        let cb = rk.pool_constraints("buddy").expect("buddy 应返回空集排除");
        assert!(cb.allowed.is_some_and(|a| a.is_empty()), "零条目池应为空集（排除），而非不限");

        // 对称：只勾 buddy
        e.allowed_accounts = vec!["buddy:b1".into()];
        let rk = constraints_of(&e);
        let cb = rk.pool_constraints("buddy").expect("buddy 应被约束");
        assert!(cb.allowed.is_some_and(|a| a.contains("b1")));
        let ct = rk.pool_constraints("trae").expect("trae 应返回空集排除");
        assert!(ct.allowed.is_some_and(|a| a.is_empty()));
    }

    #[test]
    fn pool_constraints_mixed_legacy_uid_falls_back_to_buddy() {
        // 防御：混合列表中混入裸 uid → 归属 buddy（F-35 旧语义兜底）
        let mut e = entry("k1", "ck-a", true, 0);
        e.allowed_accounts = vec!["trae:t1".into(), "b9".into()];
        let rk = constraints_of(&e);
        let cb = rk.pool_constraints("buddy").expect("buddy 应被约束");
        assert!(cb.allowed.is_some_and(|a| a.contains("b9")));
        let ct = rk.pool_constraints("trae").expect("trae 应被约束");
        assert!(ct.allowed.is_some_and(|a| a.contains("t1") && a.len() == 1));
    }

    #[test]
    fn pool_constraints_mixed_dedicated_pins_pool() {
        // 混合 + 专一（trae:t1）：归属池 trae 提取 dedicated 并入白名单；buddy 排除（锁池）
        let mut e = entry("k1", "ck-a", true, 0);
        e.schedule_mode = MODE_DEDICATED.into();
        e.allowed_accounts = vec!["trae:t1".into(), "buddy:b1".into()];
        e.dedicated_account = "trae:t1".into();
        let rk = constraints_of(&e);
        let ct = rk.pool_constraints("trae").expect("归属池应被约束");
        assert!(ct.allowed.is_some_and(|a| a.contains("t1")));
        assert_eq!(ct.dedicated.as_deref(), Some("t1"), "dedicated 应去前缀");
        let cb = rk.pool_constraints("buddy").expect("非归属池应返回空集排除");
        assert!(cb.allowed.is_some_and(|a| a.is_empty()));
        assert!(cb.dedicated.is_none());

        // 专一空 dedicated 回退 allowed 首个（trae:t1）→ 仍锁 trae 排除 buddy
        e.dedicated_account = String::new();
        let rk = constraints_of(&e);
        let ct = rk.pool_constraints("trae").expect("归属池应被约束");
        assert!(ct.allowed.is_some_and(|a| a.contains("t1")));
        assert_eq!(ct.dedicated.as_deref(), Some("t1"));
        let cb = rk.pool_constraints("buddy").expect("非归属池应返回空集排除");
        assert!(cb.allowed.is_some_and(|a| a.is_empty()));
    }

    #[test]
    fn pool_constraints_mixed_dedicated_outside_whitelist() {
        // 审查 #2 修复：专一账号不在勾选集 → 归属池白名单并入专一账号（预检不误杀）；
        // 另一池锁池排除。场景：只勾 buddy:b1，专一手选 trae:t9
        let mut e = entry("k1", "ck-a", true, 0);
        e.schedule_mode = MODE_DEDICATED.into();
        e.allowed_accounts = vec!["buddy:b1".into()];
        e.dedicated_account = "trae:t9".into();
        let rk = constraints_of(&e);
        let ct = rk.pool_constraints("trae").expect("归属池应被约束");
        assert!(ct.allowed.is_some_and(|a| a.contains("t9") && !a.contains("b1")));
        assert_eq!(ct.dedicated.as_deref(), Some("t9"));
        let cb = rk.pool_constraints("buddy").expect("非归属池应返回空集排除");
        assert!(cb.allowed.is_some_and(|a| a.is_empty()), "专一锁池：勾选集 b1 也不得放行");
    }
}
