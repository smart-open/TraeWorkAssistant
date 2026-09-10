//! 多 API Key 管理与每日配额（T2）+ ck_xxx 子 Key 体系（F-35，批次3）。
//!
//! 数据落盘 `data/api_keys.json`；`daily_limit = 0` 表示不限。
//! 所有 Key 统一在列表中维护；未配置任何启用的 Key 时不鉴权。
//! 每次鉴权命中 Key 即累加当日用量并原子写盘（与 usage.rs 同策略：个人频率低）。
//!
//! F-35 子 Key 体系（对外子 Key 与上游真实凭证分离）：
//! - `ck_` 前缀子 Key（`generate_sub_key` 生成；旧 `sk-` Key 继续兼容）
//! - `allowed_accounts`：限定上游（WB 上游账号 uid 白名单，空 = 不限）
//! - `schedule_mode`：`expire_first`（默认，临期优先）| `dedicated`（专一，固定
//!   `dedicated_account` 或 allowed_accounts 首个）
//! - `daily_stats`：按日请求统计（保留最近 90 天）

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::fs_utils;

/// 数据文件名（位于 data/ 目录）
pub const KEYS_FILE: &str = "api_keys.json";

/// 子 Key 前缀（F-35）
pub const SUB_KEY_PREFIX: &str = "ck_";

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
#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct ApiKeysFile {
    #[serde(default)]
    pub keys: Vec<ApiKeyEntry>,
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
        let Some(e) = self.keys.iter_mut().find(|k| k.enabled && k.key == presented) else {
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

/// 生成子 Key（`ck_` + 32 hex；sha256(纳秒 + 计数器 + pid)，与 pseudo_uuid_v4 同源思路）
pub fn generate_sub_key() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let mut h = Sha256::new();
    h.update(now.as_nanos().to_le_bytes());
    h.update(n.to_le_bytes());
    h.update(std::process::id().to_le_bytes());
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    format!("{SUB_KEY_PREFIX}{}", &hex[..32])
}

/// 按 Key 条目 id 解析约束快照（WB 路由层每流程调用一次；Key 不存在返回 None）
pub fn constraints_for(data_dir: &Path, key_id: &str) -> Option<ResolvedKey> {
    let f: ApiKeysFile = load(data_dir);
    f.keys.iter().find(|k| k.id == key_id).map(|e| ResolvedKey {
        id: e.id.clone(),
        allowed_accounts: e.allowed_accounts.clone(),
        schedule_mode: e.schedule_mode().to_string(),
        dedicated_account: e.dedicated_account.clone(),
    })
}

/// 数据文件路径：data_dir/data/api_keys.json
pub fn keys_path(data_dir: &Path) -> PathBuf {
    let dir = data_dir.join("data");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(KEYS_FILE)
}

/// 读盘
pub fn load(data_dir: &Path) -> ApiKeysFile {
    fs_utils::read_json(&keys_path(data_dir))
}

/// 原子写盘
pub fn save(data_dir: &Path, f: &ApiKeysFile) {
    let _ = fs_utils::write_json(&keys_path(data_dir), f);
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
        };
        assert!(matches!(f.verify_and_consume("ck-a", "d"), KeyCheck::Invalid));
        assert!(matches!(f.verify_and_consume("ck-c", "d"), KeyCheck::Invalid));
    }

    #[test]
    fn quota_blocks_at_limit_and_resets_next_day() {
        let mut f = ApiKeysFile {
            keys: vec![entry("k1", "ck-a", true, 2)],
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
    fn sub_key_prefix_and_uniqueness() {
        let a = generate_sub_key();
        let b = generate_sub_key();
        assert!(a.starts_with("ck_"));
        assert_ne!(a, b);
        assert_eq!(a.len(), 3 + 32);
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
}
