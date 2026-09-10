//! 多 API Key 管理与每日配额。
//!
//! 数据落盘 `data/api_keys.json`；`daily_limit = 0` 表示不限。
//! 所有 Key 统一在列表中维护（无主/子之分）；未配置任何启用的 Key 时拒绝所有业务请求（fail-closed）。
//! 每次鉴权命中 Key 即累加当日用量并原子写盘（与 usage.rs 同策略：个人频率低）。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fs_utils;

/// 数据文件名（位于 data/ 目录）
pub const KEYS_FILE: &str = "api_keys.json";

/// 鉴权结果
pub enum KeyCheck {
    /// 命中且已记账，携带 Key 条目 id
    Ok(String),
    /// Key 无效或已禁用
    Invalid,
    /// 超出当日配额
    QuotaExceeded { limit: u64 },
}

/// API Key 条目
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ApiKeyEntry {
    /// 唯一标识（前端生成）
    pub id: String,
    /// 展示名
    pub name: String,
    /// 实际 Key 值（sk-...）
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

impl ApiKeysFile {
    /// 按呈现的 Key 校验并记账（命中即 +1）。调用方负责把结果写盘。
    pub fn verify_and_consume(&mut self, presented: &str, today: &str) -> KeyCheck {
        let Some(e) = self.keys.iter_mut().find(|k| k.enabled && k.key == presented) else {
            return KeyCheck::Invalid;
        };
        if let Err(limit) = quota_left(e, today) {
            return KeyCheck::QuotaExceeded { limit };
        }
        e.used_today += 1;
        KeyCheck::Ok(e.id.clone())
    }

    /// 是否存在启用的子 Key（用于判断是否需要鉴权）
    pub fn has_enabled(&self) -> bool {
        self.keys.iter().any(|k| k.enabled)
    }
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
        }
    }

    #[test]
    fn verify_matches_enabled_key_and_counts() {
        let mut f = ApiKeysFile {
            keys: vec![entry("k1", "sk-a", true, 0)],
        };
        assert!(matches!(f.verify_and_consume("sk-a", "2026-09-09"), KeyCheck::Ok(id) if id == "k1"));
        assert_eq!(f.keys[0].used_today, 1);
        assert_eq!(f.keys[0].used_date, "2026-09-09");
    }

    #[test]
    fn verify_rejects_disabled_or_unknown() {
        let mut f = ApiKeysFile {
            keys: vec![entry("k1", "sk-a", false, 0), entry("k2", "sk-b", true, 0)],
        };
        assert!(matches!(f.verify_and_consume("sk-a", "d"), KeyCheck::Invalid));
        assert!(matches!(f.verify_and_consume("sk-c", "d"), KeyCheck::Invalid));
    }

    #[test]
    fn quota_blocks_at_limit_and_resets_next_day() {
        let mut f = ApiKeysFile {
            keys: vec![entry("k1", "sk-a", true, 2)],
        };
        assert!(matches!(f.verify_and_consume("sk-a", "d1"), KeyCheck::Ok(_)));
        assert!(matches!(f.verify_and_consume("sk-a", "d1"), KeyCheck::Ok(_)));
        assert!(matches!(
            f.verify_and_consume("sk-a", "d1"),
            KeyCheck::QuotaExceeded { limit: 2 }
        ));
        // 跨天重置
        assert!(matches!(f.verify_and_consume("sk-a", "d2"), KeyCheck::Ok(_)));
        assert_eq!(f.keys[0].used_today, 1);
    }

    #[test]
    fn roundtrip_with_defaults() {
        let dir = std::env::temp_dir().join(format!("twa_keys_test_{}", std::process::id()));
        let mut f = ApiKeysFile::default();
        f.keys.push(entry("k1", "sk-x", true, 5));
        save(&dir, &f);
        let loaded = load(&dir);
        // 非空文件原样读盘
        assert_eq!(loaded.keys.len(), 1);
        assert_eq!(loaded.keys[0].daily_limit, 5);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
