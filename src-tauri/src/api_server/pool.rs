use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::models::{CooldownEntry, PoolStatus};

use super::ErrKind;

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
}

/// 安全获取 Mutex 锁：若锁被毒化（panic 导致），仍恢复内部数据继续运行
fn safe_lock<'a, T>(m: &'a Mutex<T>) -> std::sync::MutexGuard<'a, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl ApiPool {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// 从已有账号文件同步池：只加入 enabled_uids 中的账号
    pub fn sync_from_accounts(
        &self,
        accounts: &[crate::models::RawAccount],
        enabled_uids: &[String],
        cooldowns: &HashMap<String, CooldownEntry>,
        remaining_credits: &HashMap<String, f64>,
        expire_times: &HashMap<String, i64>,
    ) {
        let mut entries = safe_lock(&self.entries);
        entries.clear();
        let enabled: HashSet<&str> = enabled_uids.iter().map(|s| s.as_str()).collect();
        for a in accounts {
            if let Some(uid) = &a.user_id {
                if !enabled.contains(uid.as_str()) {
                    continue;
                }
                let cd = cooldowns.get(uid).cloned().unwrap_or_default();
                let disabled = cd.error_type == "SessionDead";
                entries.insert(
                    uid.clone(),
                    PoolEntry {
                        uid: uid.clone(),
                        name: a.name.clone(),
                        jwt: a.jwt.clone(),
                        credits: remaining_credits.get(uid).copied(),
                        credits_expire_at: expire_times.get(uid).copied(),
                        disabled,
                        err_count: cd.error_count,
                        until: cd.until,
                        reason: cd.reason,
                    },
                );
            }
        }
    }

    /// 挑选 healthy 账号中积分过期时间最近者；跳过 tried
    pub fn pick_excluding(&self, tried: &HashSet<String>) -> Option<PickedAccount> {
        let entries = safe_lock(&self.entries);
        let now = now_ts();
        let mut best: Option<&PoolEntry> = None;
        for (uid, e) in entries.iter() {
            if tried.contains(uid) || !e.healthy(now) {
                continue;
            }
            // 跳过积分已过期的
            if let Some(exp) = e.credits_expire_at {
                if exp < now {
                    continue;
                }
            }
            // 跳过零积分且有过期时间的
            if e.credits_expire_at.is_some() {
                if let Some(c) = e.credits {
                    if c <= 0.0 {
                        continue;
                    }
                }
            }
            match best {
                None => best = Some(e),
                Some(b) => {
                    let be = b.credits_expire_at.is_some();
                    let ee = e.credits_expire_at.is_some();
                    if ee && !be {
                        best = Some(e);
                    } else if ee && be {
                        if e.credits_expire_at < b.credits_expire_at {
                            best = Some(e);
                        } else if e.credits_expire_at == b.credits_expire_at {
                            if e.credits.unwrap_or(0.0) > b.credits.unwrap_or(0.0) {
                                best = Some(e);
                            }
                        }
                    }
                }
            }
        }
        best.map(|e| PickedAccount {
            uid: e.uid.clone(),
            jwt: e.jwt.clone(),
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
}

pub struct PickedAccount {
    pub uid: String,
    pub jwt: String,
}

fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
