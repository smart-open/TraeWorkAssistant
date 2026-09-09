use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Sha256, Digest};

use crate::models::{CooldownEntry, DeviceMap, PoolStatus};

use super::ErrKind;

/// 账号池调度策略（T10）
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum PoolStrategy {
    /// 积分先过期优先（默认，保持既有行为）
    #[default]
    ExpireFirst,
    /// 剩余通用积分多优先
    CreditFirst,
    /// 随机取号
    Random,
}

impl PoolStrategy {
    /// 解析配置字符串（空/未知值回退 expire_first）
    pub fn parse(s: &str) -> Self {
        match s {
            "credit_first" => Self::CreditFirst,
            "random" => Self::Random,
            _ => Self::ExpireFirst,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExpireFirst => "expire_first",
            Self::CreditFirst => "credit_first",
            Self::Random => "random",
        }
    }
}

/// 池中单个账号的运行时状态
pub struct PoolEntry {
    pub uid: String,
    pub name: String,
    pub jwt: String,
    pub credits: Option<f64>,
    pub credits_expire_at: Option<i64>,
    pub disabled: bool,
    pub err_count: i32,
    /// 冷却截止时间（Unix 秒），0 表示无冷却
    pub until: i64,
    pub reason: String,
    pub device_id: String,
    pub machine_id: String,
}

impl PoolEntry {
    fn healthy(&self, now_ts: i64) -> bool {
        if self.disabled {
            return false;
        }
        if self.until > 0 && now_ts < self.until {
            return false;
        }
        true
    }
}

/// 账号池：内存索引 + 冷却/禁用状态机
pub struct ApiPool {
    entries: Mutex<HashMap<String, PoolEntry>>,
    strategy: Mutex<PoolStrategy>,
}

/// 安全获取 Mutex 锁：若锁被毒化（panic 导致），仍恢复内部数据继续运行
fn safe_lock<'a, T>(m: &'a Mutex<T>) -> std::sync::MutexGuard<'a, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl ApiPool {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            strategy: Mutex::new(PoolStrategy::ExpireFirst),
        }
    }

    /// 设置调度策略（启动时由 api_pool.json 决定）
    pub fn set_strategy(&self, s: PoolStrategy) {
        *safe_lock(&self.strategy) = s;
    }

    /// 从已有账号文件同步池：只加入 enabled_uids 中的账号；
    /// group_ids 非空时仅纳入所选分组的账号（未分组账号不参与，T10）
    #[allow(clippy::too_many_arguments)]
    pub fn sync_from_accounts(
        &self,
        accounts: &[crate::models::RawAccount],
        enabled_uids: &[String],
        group_ids: &[String],
        membership: &HashMap<String, String>,
        cooldowns: &HashMap<String, CooldownEntry>,
        remaining_credits: &HashMap<String, f64>,
        expire_times: &HashMap<String, i64>,
        device_map: &DeviceMap,
    ) {
        let mut entries = safe_lock(&self.entries);
        entries.clear();
        let enabled: HashSet<&str> = enabled_uids.iter().map(|s| s.as_str()).collect();
        let group_filter: Option<HashSet<&str>> = if group_ids.is_empty() {
            None
        } else {
            Some(group_ids.iter().map(|s| s.as_str()).collect())
        };
        for a in accounts {
            if let Some(uid) = &a.user_id {
                if !enabled.contains(uid.as_str()) {
                    continue;
                }
                if let Some(filter) = &group_filter {
                    let in_group = membership
                        .get(uid)
                        .map_or(false, |g| filter.contains(g.as_str()));
                    if !in_group {
                        continue;
                    }
                }
                let cd = cooldowns.get(uid).cloned().unwrap_or_default();
                let disabled = cd.error_type == "SessionDead";
                let (device_id, machine_id) = device_map
                    .get(uid)
                    .map(|d| (d.device_id.clone(), seeded_hex(64, uid, "mach")))
                    .unwrap_or_else(|| (String::new(), seeded_hex(64, uid, "mach")));
                let jwt_raw = a.jwt.clone();
                let jwt_clean = jwt_raw
                    .strip_prefix("Cloud-IDE-JWT ")
                    .unwrap_or(&jwt_raw)
                    .trim()
                    .to_string();
                entries.insert(
                    uid.clone(),
                    PoolEntry {
                        uid: uid.clone(),
                        name: a.name.clone(),
                        jwt: jwt_clean,
                        credits: remaining_credits.get(uid).copied(),
                        credits_expire_at: expire_times.get(uid).copied(),
                        disabled,
                        err_count: cd.error_count,
                        until: cd.until,
                        reason: cd.reason,
                        device_id,
                        machine_id,
                    },
                );
            }
        }
    }

    /// 按当前策略挑选 healthy 账号；跳过 tried
    /// llm_utils_chat 消耗通用积分(product_id 208)
    /// 零积分账号会被跳过，避免无效请求
    pub fn pick_excluding(&self, tried: &HashSet<String>) -> Option<PickedAccount> {
        let entries = safe_lock(&self.entries);
        let strategy = *safe_lock(&self.strategy);
        let now = now_ts();
        let cands: Vec<&PoolEntry> = entries
            .values()
            .filter(|e| selectable(e, tried, now))
            .collect();
        // Random 用纳秒级时间做种子（无需密码学随机，仅打散取号顺序）
        let rand_seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() ^ (d.subsec_nanos() as u64).wrapping_mul(0x9e3779b97f4a7c15))
            .unwrap_or(0);
        pick_by_strategy(&cands, strategy, rand_seed).map(|e| PickedAccount {
            uid: e.uid.clone(),
            jwt: e.jwt.clone(),
            device_id: e.device_id.clone(),
            machine_id: e.machine_id.clone(),
        })
    }

    /// 记录错误并冷却
    pub fn note_error(&self, uid: &str, kind: ErrKind) {
        let dur = kind.cooldown_duration();
        let mut entries = safe_lock(&self.entries);
        if let Some(e) = entries.get_mut(uid) {
            if kind == ErrKind::SessionDead {
                e.disabled = true;
            } else if kind == ErrKind::PlanLimit || kind == ErrKind::SoftRate || kind == ErrKind::NotFound {
                e.until = now_ts() + dur.as_secs() as i64;
                e.reason = kind.as_str().to_string();
                e.err_count = 0;
            } else {
                e.err_count += 1;
                if e.err_count >= 3 {
                    e.until = now_ts() + dur.as_secs() as i64;
                    e.reason = "consecutive_errors".to_string();
                    e.err_count = 0;
                }
            }
        }
    }

    /// 记录成功
    pub fn note_success(&self, uid: &str) {
        let mut entries = safe_lock(&self.entries);
        if let Some(e) = entries.get_mut(uid) {
            e.err_count = 0;
        }
    }

    /// 清除所有账号的内存冷却状态（不影响 disabled/SessionDead）
    pub fn clear_cooldowns(&self) -> usize {
        let mut entries = safe_lock(&self.entries);
        let now = now_ts();
        let count = entries
            .values()
            .filter(|e| e.until > 0 && now < e.until)
            .count();
        for e in entries.values_mut() {
            e.until = 0;
            e.reason.clear();
            e.err_count = 0;
        }
        count
    }

    /// 返回池状态列表
    pub fn status_list(&self) -> Vec<PoolStatus> {
        let entries = safe_lock(&self.entries);
        let now = now_ts();
        let mut out: Vec<PoolStatus> = entries
            .values()
            .map(|e| PoolStatus {
                uid: e.uid.clone(),
                name: e.name.clone(),
                credits: e.credits,
                credits_expire_at: e.credits_expire_at,
                cooling: e.until > 0 && now < e.until,
                cooldown_until: if e.until > 0 { Some(e.until) } else { None },
                cooldown_reason: if e.reason.is_empty() { None } else { Some(e.reason.clone()) },
                disabled: e.disabled,
                err_count: e.err_count,
            })
            .collect();
        out.sort_by(|a, b| a.uid.cmp(&b.uid));
        out
    }

    pub fn count(&self) -> usize {
        safe_lock(&self.entries).len()
    }

    /// 诊断：返回所有账号被过滤的原因（用于 "no healthy account" 排查）
    pub fn diagnose(&self) -> Vec<PoolDiagnosis> {
        let entries = safe_lock(&self.entries);
        let now = now_ts();
        entries
            .values()
            .map(|e| {
                let reason = if e.disabled {
                    "disabled(SessionDead)".to_string()
                } else if e.until > 0 && now < e.until {
                    format!("cooldown(until={} remaining={}s)", e.until, e.until - now)
                } else if let Some(exp) = e.credits_expire_at {
                    if exp > 0 && exp < now {
                        "credits_expired".to_string()
                    } else if exp > 0 {
                        if let Some(c) = e.credits {
                            if c <= 0.0 {
                                "zero_credits".to_string()
                            } else {
                                "healthy".to_string()
                            }
                        } else {
                            "healthy(no_credits_info)".to_string()
                        }
                    } else {
                        "healthy(no_expiry)".to_string()
                    }
                } else {
                    "healthy(no_expiry_info)".to_string()
                };
                PoolDiagnosis {
                    uid: e.uid.clone(),
                    name: e.name.clone(),
                    disabled: e.disabled,
                    until: e.until,
                    credits: e.credits,
                    credits_expire_at: e.credits_expire_at,
                    reason,
                }
            })
            .collect()
    }
}

/// 账号池诊断信息
pub struct PoolDiagnosis {
    pub uid: String,
    pub name: String,
    pub disabled: bool,
    pub until: i64,
    pub credits: Option<f64>,
    pub credits_expire_at: Option<i64>,
    pub reason: String,
}

pub struct PickedAccount {
    pub uid: String,
    pub jwt: String,
    pub device_id: String,
    pub machine_id: String,
}

/// 候选过滤：healthy + 未 tried + 积分未过期 + 非零积分
fn selectable(e: &PoolEntry, tried: &HashSet<String>, now: i64) -> bool {
    if tried.contains(&e.uid) || !e.healthy(now) {
        return false;
    }
    // 跳过积分已过期的（expire_time=0 视为无过期时间，不跳过）
    if let Some(exp) = e.credits_expire_at {
        if exp > 0 && exp < now {
            return false;
        }
    }
    // 跳过零通用积分账号（通用积分耗尽，llm_utils_chat 无法使用）
    if let Some(c) = e.credits {
        if c <= 0.0 {
            return false;
        }
    }
    true
}

/// 按策略从候选集中挑选（纯函数，便于单测）
fn pick_by_strategy<'a>(
    cands: &[&'a PoolEntry],
    strategy: PoolStrategy,
    rand_seed: u64,
) -> Option<&'a PoolEntry> {
    if cands.is_empty() {
        return None;
    }
    match strategy {
        PoolStrategy::Random => Some(cands[(rand_seed as usize) % cands.len()]),
        PoolStrategy::CreditFirst => cands.iter().copied().max_by(|a, b| {
            a.credits
                .unwrap_or(0.0)
                .partial_cmp(&b.credits.unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        PoolStrategy::ExpireFirst => cands.iter().copied().min_by(|a, b| {
            // 有过期时间者优先（现状语义）→ 更早过期优先 → 平手积分多者优先
            let ha = a.credits_expire_at.map_or(false, |t| t > 0);
            let hb = b.credits_expire_at.map_or(false, |t| t > 0);
            hb.cmp(&ha)
                .then_with(|| {
                    a.credits_expire_at
                        .unwrap_or(0)
                        .cmp(&b.credits_expire_at.unwrap_or(0))
                })
                .then_with(|| {
                    b.credits
                        .unwrap_or(0.0)
                        .partial_cmp(&a.credits.unwrap_or(0.0))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
        }),
    }
}

fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 确定性派生 hex 字符串（与 device_proxy.py 的 _seeded_stream 算法一致）
/// 用于从 uid 生成 machine_id，保证同一账号始终得到同一设备标识
pub(crate) fn seeded_hex(n: usize, seed: &str, salt: &str) -> String {
    let data = format!("{}:{}", salt, seed);
    let mut out = Vec::new();
    let mut i: u32 = 0;
    while out.len() < (n + 1) / 2 {
        let mut hasher = Sha256::new();
        hasher.update(data.as_bytes());
        hasher.update(i.to_be_bytes());
        out.extend_from_slice(&hasher.finalize());
        i += 1;
    }
    out.truncate((n + 1) / 2);
    out.iter().map(|b| format!("{:02x}", b)).collect::<String>()
        .chars().take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct(name: &str, uid: &str) -> crate::models::RawAccount {
        crate::models::RawAccount {
            name: name.to_string(),
            user_id: Some(uid.to_string()),
            jwt: String::new(),
            refresh_token: None,
            added_at: None,
            updated_at: None,
        }
    }

    /// 建池：accounts 为 (uid, credits, expire) 三元组，全部 enabled
    fn build_pool(entries: &[(&str, f64, i64)]) -> ApiPool {
        let pool = ApiPool::new();
        let accounts: Vec<crate::models::RawAccount> = entries
            .iter()
            .map(|(uid, _, _)| acct(uid, uid))
            .collect();
        let enabled: Vec<String> = entries.iter().map(|(uid, _, _)| uid.to_string()).collect();
        let mut credits = HashMap::new();
        let mut expires = HashMap::new();
        for (uid, c, e) in entries {
            credits.insert(uid.to_string(), *c);
            expires.insert(uid.to_string(), *e);
        }
        pool.sync_from_accounts(
            &accounts,
            &enabled,
            &[],
            &HashMap::new(),
            &HashMap::new(),
            &credits,
            &expires,
            &HashMap::new(),
        );
        pool
    }

    #[test]
    fn strategy_parse_defaults_to_expire_first() {
        assert_eq!(PoolStrategy::parse(""), PoolStrategy::ExpireFirst);
        assert_eq!(PoolStrategy::parse("unknown"), PoolStrategy::ExpireFirst);
        assert_eq!(PoolStrategy::parse("credit_first"), PoolStrategy::CreditFirst);
        assert_eq!(PoolStrategy::parse("random"), PoolStrategy::Random);
        assert_eq!(PoolStrategy::default(), PoolStrategy::ExpireFirst);
    }

    #[test]
    fn expire_first_prefers_soonest_expiry() {
        // 两个都有过期时间（均在未来）：更早过期者胜
        let pool = build_pool(&[
            ("uid_a", 100.0, 4_000_001_000),
            ("uid_b", 100.0, 3_900_000_000),
        ]);
        pool.set_strategy(PoolStrategy::ExpireFirst);
        let picked = pool.pick_excluding(&HashSet::new()).unwrap();
        assert_eq!(picked.uid, "uid_b");
        // 排除后取另一个
        let mut tried = HashSet::new();
        tried.insert("uid_b".to_string());
        let picked = pool.pick_excluding(&tried).unwrap();
        assert_eq!(picked.uid, "uid_a");
    }

    #[test]
    fn expire_first_prefers_has_expiry_and_more_credits_on_tie() {
        // 有过期时间者优先于无过期时间
        let pool = build_pool(&[("uid_a", 999.0, 0), ("uid_b", 10.0, 4_000_000_000)]);
        pool.set_strategy(PoolStrategy::ExpireFirst);
        assert_eq!(pool.pick_excluding(&HashSet::new()).unwrap().uid, "uid_b");
        // 过期时间相同：积分多者胜
        let pool = build_pool(&[
            ("uid_a", 50.0, 4_000_000_000),
            ("uid_b", 200.0, 4_000_000_000),
        ]);
        assert_eq!(pool.pick_excluding(&HashSet::new()).unwrap().uid, "uid_b");
    }

    #[test]
    fn credit_first_prefers_more_credits() {
        let pool = build_pool(&[
            ("uid_a", 50.0, 3_900_000_000),
            ("uid_b", 300.0, 4_000_000_000),
        ]);
        pool.set_strategy(PoolStrategy::CreditFirst);
        let picked = pool.pick_excluding(&HashSet::new()).unwrap();
        assert_eq!(picked.uid, "uid_b");
    }

    #[test]
    fn random_strategy_only_picks_candidates() {
        let pool = build_pool(&[
            ("uid_a", 50.0, 3_900_000_000),
            ("uid_b", 300.0, 4_000_000_000),
        ]);
        pool.set_strategy(PoolStrategy::Random);
        // 连续取号（排除已选）能遍历完所有候选后枯竭
        let mut tried = HashSet::new();
        for _ in 0..2 {
            let p = pool.pick_excluding(&tried).unwrap();
            tried.insert(p.uid);
        }
        assert!(pool.pick_excluding(&tried).is_none());
    }

    #[test]
    fn zero_credit_and_expired_are_skipped() {
        // 零积分账号不参与取号
        let pool = build_pool(&[("uid_a", 0.0, 0), ("uid_b", 10.0, 0)]);
        assert_eq!(pool.pick_excluding(&HashSet::new()).unwrap().uid, "uid_b");
        // 积分已过期账号不参与取号（now 之后才会过期的不受影响）
        let pool = build_pool(&[("uid_a", 10.0, 1), ("uid_b", 10.0, 0)]);
        assert_eq!(pool.pick_excluding(&HashSet::new()).unwrap().uid, "uid_b");
    }

    #[test]
    fn group_filter_excludes_unselected_groups() {
        let pool = ApiPool::new();
        let accounts = vec![acct("A", "uid_a"), acct("B", "uid_b"), acct("C", "uid_c")];
        let enabled: Vec<String> = vec!["uid_a".into(), "uid_b".into(), "uid_c".into()];
        let mut membership = HashMap::new();
        membership.insert("uid_a".to_string(), "g1".to_string());
        membership.insert("uid_b".to_string(), "g2".to_string());
        // uid_c 未分组
        pool.sync_from_accounts(
            &accounts,
            &enabled,
            &["g2".to_string()],
            &membership,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(pool.count(), 1);
        let picked = pool.pick_excluding(&HashSet::new()).unwrap();
        assert_eq!(picked.uid, "uid_b");
    }

    #[test]
    fn empty_group_filter_keeps_all() {
        let pool = ApiPool::new();
        let accounts = vec![acct("A", "uid_a"), acct("B", "uid_b")];
        let enabled: Vec<String> = vec!["uid_a".into(), "uid_b".into()];
        let mut membership = HashMap::new();
        membership.insert("uid_a".to_string(), "g1".to_string());
        pool.sync_from_accounts(
            &accounts,
            &enabled,
            &[], // 空 = 不限分组（旧配置默认行为）
            &membership,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(pool.count(), 2);
    }
}
