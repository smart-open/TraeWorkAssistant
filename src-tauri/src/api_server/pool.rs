use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Sha256, Digest};

use crate::models::{CooldownEntry, DeviceMap, PoolStatus};

use super::ErrKind;

/// 账号池调度策略（T10 + T2.2 扩展）
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum PoolStrategy {
    /// 积分先过期优先（默认，保持既有行为）
    #[default]
    ExpireFirst,
    /// 剩余通用积分多优先
    CreditFirst,
    /// 随机取号
    Random,
    /// 三因子加权随机（T2.2/F-29 v1.2）：积分占比×10 + 闲置补偿（每小时+0.5
    /// 封顶 5.0）+ 成功率×3 → Top5 短名单内二次加权随机——防热点 + 防惊群
    Weighted,
    /// P2C（Power-of-Two-Choices，T2.2 v1.2）：随机选二取优——antigravity-tools
    /// 实证延迟优于轮询/加权随机；与三因子加权并存，实测对比后取默认
    P2C,
}

impl PoolStrategy {
    /// 解析配置字符串（空/未知值回退 expire_first）
    pub fn parse(s: &str) -> Self {
        match s {
            "credit_first" => Self::CreditFirst,
            "random" => Self::Random,
            "weighted" => Self::Weighted,
            "p2c" => Self::P2C,
            _ => Self::ExpireFirst,
        }
    }

    /// Buddy 池生效策略：wb_strategy 独立配置优先；空 = 跟随 Trae 池策略。
    /// 启动（api_server_start）与热应用（pool_set）共用此语义，保证两处行为一致。
    pub fn resolve_wb(strategy: &str, wb_strategy: &str) -> Self {
        Self::parse(if wb_strategy.is_empty() {
            strategy
        } else {
            wb_strategy
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExpireFirst => "expire_first",
            Self::CreditFirst => "credit_first",
            Self::Random => "random",
            Self::Weighted => "weighted",
            Self::P2C => "p2c",
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
    // ── T2.2 五态机与调度因子 ──
    /// hard_credit（积分耗尽）冷却截止：次日 04:00 自动恢复探测（F-29 v1.2）
    pub hard_until: i64,
    /// 最近一次被选中时间（Unix 秒；三因子加权闲置补偿因子）
    pub last_used: i64,
    /// 成功/失败计数（三因子加权成功率因子）
    pub successes: u64,
    pub failures: u64,
    /// 连续熔断次数（Server 错误 30m 起指数递增至 6h）
    pub cb_trips: u32,
    // ── WorkBuddy 上游凭证头字段（T2.1；SOLO 账号为空串/false）──
    pub domain: String,
    pub enterprise_id: String,
    pub global_region: bool,
}

impl PoolEntry {
    fn healthy(&self, now_ts: i64) -> bool {
        if self.disabled {
            return false;
        }
        if self.until > 0 && now_ts < self.until {
            return false;
        }
        // hard_credit 冷却：次日 04:00 前不参与（04:00 恢复探测由到期自然放开）
        if self.hard_until > 0 && now_ts < self.hard_until {
            return false;
        }
        true
    }

    /// 账号五态机（F-29 v1.2）：Available / QuotaProtection / RateLimited /
    /// Forbidden / ProxyDisabled（供 /status 画像与状态迁移观测）
    pub fn state_str(&self, now_ts: i64) -> &'static str {
        if self.disabled {
            "Forbidden"
        } else if self.hard_until > 0 && now_ts < self.hard_until {
            "QuotaProtection"
        } else if self.until > 0 && now_ts < self.until {
            "RateLimited"
        } else {
            "Available"
        }
    }

    /// 三因子得分（T2.2）：积分占比×10 + 闲置补偿×0.5/h（封顶 5.0）+ 成功率×3
    /// 纯函数；`total_credits` 为候选集积分总和（0 时积分因子取 0）
    fn weighted_score(&self, total_credits: f64, now_ts: i64) -> f64 {
        let credit_factor = if total_credits > 0.0 {
            (self.credits.unwrap_or(0.0).max(0.0) / total_credits) * 10.0
        } else {
            0.0
        };
        let idle_hours = if self.last_used > 0 {
            ((now_ts - self.last_used).max(0) as f64) / 3600.0
        } else {
            // 从未被选用：按满闲置补偿（鼓励冷启动账号）
            10.0
        };
        let idle_factor = (idle_hours * 0.5).min(5.0);
        let total = self.successes + self.failures;
        let success_rate = if total == 0 { 0.5 } else { self.successes as f64 / total as f64 };
        credit_factor + idle_factor + success_rate * 3.0
    }
}

/// 账号池：内存索引 + 冷却/禁用状态机
pub struct ApiPool {
    entries: Mutex<HashMap<String, PoolEntry>>,
    strategy: Mutex<PoolStrategy>,
    /// 防惊群（T2.2）：100ms 内重复选中同一 uid 且存在其他候选时让位
    recent_pick: Mutex<(String, i64)>, // (uid, 毫秒时间戳)
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
            recent_pick: Mutex::new((String::new(), 0)),
        }
    }

    /// 设置调度策略（启动时由 api_pool.json 决定）
    pub fn set_strategy(&self, s: PoolStrategy) {
        *safe_lock(&self.strategy) = s;
    }

    /// 健康账号池画像（智能调度因子，dispatch::smart_pool_order 数据源）：
    /// (最早积分到期时间, 健康账号剩余积分总和)；无到期数据 → None
    pub fn stats(&self) -> (Option<i64>, f64) {
        let entries = safe_lock(&self.entries);
        let now = now_ts();
        let mut earliest: Option<i64> = None;
        let mut total = 0.0;
        for e in entries.values() {
            if !e.healthy(now) {
                continue;
            }
            if let Some(c) = e.credits {
                if c > 0.0 {
                    total += c;
                }
            }
            if let Some(exp) = e.credits_expire_at {
                if exp > 0 && earliest.map_or(true, |cur| exp < cur) {
                    earliest = Some(exp);
                }
            }
        }
        (earliest, total)
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
                        hard_until: 0,
                        last_used: 0,
                        successes: 0,
                        failures: 0,
                        cb_trips: 0,
                        domain: String::new(),
                        enterprise_id: String::new(),
                        global_region: false,
                    },
                );
            }
        }
    }

    /// 从 WorkBuddy 账号同步池（T2.1/F-28）：WB 上游账号进同一调度引擎，
    /// 携带区域/企业域信息供上游 headers 使用
    pub fn sync_from_wb(&self, accounts: &[WbSyncAccount], enabled: &[String]) {
        let mut entries = safe_lock(&self.entries);
        // 仅替换 WB 形态的条目：以 domain/global_region 任一非空/true 识别。
        // 实际部署中 SOLO 与 WB 不同时入池（api 服务单实例二选一上游），
        // 但这里保持防御性：不清空非 WB 条目。
        entries.retain(|_, e| e.domain.is_empty() && !e.global_region);
        let enabled_set: HashSet<&str> = enabled.iter().map(|s| s.as_str()).collect();
        for a in accounts {
            if !enabled_set.contains(a.uid.as_str()) || a.token.is_empty() {
                continue;
            }
            entries.insert(
                a.uid.clone(),
                PoolEntry {
                    uid: a.uid.clone(),
                    name: a.name.clone(),
                    jwt: a.token.clone(),
                    credits: a.credits,
                    credits_expire_at: None,
                    disabled: a.needs_relogin,
                    err_count: 0,
                    until: 0,
                    reason: String::new(),
                    device_id: String::new(),
                    machine_id: String::new(),
                    hard_until: 0,
                    last_used: 0,
                    successes: 0,
                    failures: 0,
                    cb_trips: 0,
                    domain: a.domain.clone(),
                    enterprise_id: a.enterprise_id.clone(),
                    global_region: a.global_region,
                },
            );
        }
    }

    /// 按当前策略挑选 healthy 账号；跳过 tried
    /// llm_utils_chat 消耗通用积分(product_id 208)
    /// 零积分账号会被跳过，避免无效请求
    pub fn pick_excluding(&self, tried: &HashSet<String>) -> Option<PickedAccount> {
        let mut entries = safe_lock(&self.entries);
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
        let mut picked = pick_by_strategy(&cands, strategy, rand_seed, now).map(|e| PickedAccount {
            uid: e.uid.clone(),
            jwt: e.jwt.clone(),
            device_id: e.device_id.clone(),
            machine_id: e.machine_id.clone(),
            domain: e.domain.clone(),
            enterprise_id: e.enterprise_id.clone(),
            global_region: e.global_region,
        })?;

        // 防惊群：100ms 内重复选中同一 uid 且还有其他候选 → 让位（T2.2）
        if cands.len() > 1 {
            let mut recent = safe_lock(&self.recent_pick);
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            if recent.0 == picked.uid && now_ms - recent.1 < 100 {
                if let Some(alt) = pick_by_strategy(
                    &cands
                        .iter()
                        .copied()
                        .filter(|e| e.uid != picked.uid)
                        .collect::<Vec<_>>(),
                    strategy,
                    rand_seed.wrapping_add(1),
                    now,
                ) {
                    picked = PickedAccount {
                        uid: alt.uid.clone(),
                        jwt: alt.jwt.clone(),
                        device_id: alt.device_id.clone(),
                        machine_id: alt.machine_id.clone(),
                        domain: alt.domain.clone(),
                        enterprise_id: alt.enterprise_id.clone(),
                        global_region: alt.global_region,
                    };
                }
            }
            *recent = (picked.uid.clone(), now_ms);
        }

        // 记录取号时间（三因子加权闲置补偿因子）
        if let Some(e) = entries.get_mut(&picked.uid) {
            e.last_used = now;
        }
        Some(picked)
    }

    /// 指定 uid 取号（T2.4 会话粘性）：账号 healthy 时返回其凭证，否则 None
    pub fn pick_by_uid(&self, uid: &str) -> Option<PickedAccount> {
        let entries = safe_lock(&self.entries);
        let now = now_ts();
        let tried = HashSet::new();
        entries.get(uid).filter(|e| selectable(e, &tried, now)).map(|e| PickedAccount {
            uid: e.uid.clone(),
            jwt: e.jwt.clone(),
            device_id: e.device_id.clone(),
            machine_id: e.machine_id.clone(),
            domain: e.domain.clone(),
            enterprise_id: e.enterprise_id.clone(),
            global_region: e.global_region,
        })
    }

    /// 带 Key 约束取号（F-35 子 Key 体系，批次3）：
    /// - `dedicated`：专一模式绑定 uid（healthy 即直接锁定，绕过策略）
    /// - `allowed`：上游白名单过滤（None/空 = 不限）
    /// - 调度策略沿用池当前策略（子 Key「临期优先」= 池默认 expire_first，
    ///   池策略本身即用户可选的临期/积分/加权等模式；约束仅做过滤与锁定）
    pub fn pick_excluding_constrained(
        &self,
        tried: &HashSet<String>,
        allowed: Option<&HashSet<String>>,
        dedicated: Option<&str>,
    ) -> Option<PickedAccount> {
        // 专一模式：绑定账号 healthy 且未试错过 → 直接锁定
        if let Some(uid) = dedicated {
            if !tried.contains(uid) {
                if let Some(p) = self.pick_by_uid(uid) {
                    let mut entries = safe_lock(&self.entries);
                    if let Some(e) = entries.get_mut(uid) {
                        e.last_used = now_ts();
                    }
                    return Some(p);
                }
            }
        }
        let mut entries = safe_lock(&self.entries);
        let strategy = *safe_lock(&self.strategy);
        let now = now_ts();
        let cands: Vec<&PoolEntry> = entries
            .values()
            .filter(|e| selectable(e, tried, now))
            .filter(|e| allowed.map_or(true, |a| a.contains(&e.uid)))
            .collect();
        let rand_seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() ^ (d.subsec_nanos() as u64).wrapping_mul(0x9e3779b97f4a7c15))
            .unwrap_or(0);
        let picked = pick_by_strategy(&cands, strategy, rand_seed, now)?;
        let account = PickedAccount {
            uid: picked.uid.clone(),
            jwt: picked.jwt.clone(),
            device_id: picked.device_id.clone(),
            machine_id: picked.machine_id.clone(),
            domain: picked.domain.clone(),
            enterprise_id: picked.enterprise_id.clone(),
            global_region: picked.global_region,
        };
        drop(cands); // 释放 entries 不可变借用后再更新 last_used
        if let Some(e) = entries.get_mut(&account.uid) {
            e.last_used = now;
        }
        Some(account)
    }

    /// 更新账号凭证（T2.6：网关 401 刷新后回填，后续取号即用新 token）
    pub fn update_jwt(&self, uid: &str, jwt: &str) {
        let mut entries = safe_lock(&self.entries);
        if let Some(e) = entries.get_mut(uid) {
            e.jwt = jwt.to_string();
        }
    }

    /// 记录错误并冷却（T2.2 错误三态 + 分级冷却）
    pub fn note_error(&self, uid: &str, kind: ErrKind) {
        let mut entries = safe_lock(&self.entries);
        if let Some(e) = entries.get_mut(uid) {
            e.failures = e.failures.saturating_add(1);
            match kind {
                ErrKind::SessionDead | ErrKind::Forbidden => {
                    e.disabled = true;
                    e.reason = kind.as_str().to_string();
                }
                ErrKind::HardCredit => {
                    // 积分耗尽：冷却到次日 04:00，到期自动恢复探测（F-29 v1.2）
                    e.hard_until = next_0400(now_ts());
                    e.reason = "hard_credit".to_string();
                    e.err_count = 0;
                }
                ErrKind::PlanLimit | ErrKind::SoftRate | ErrKind::NotFound => {
                    e.until = now_ts() + kind.cooldown_duration().as_secs() as i64;
                    e.reason = kind.as_str().to_string();
                    e.err_count = 0;
                    // P2 修复10：不重置 cb_trips——熔断记忆只应在 note_success 重置，
                    // 否则 Server 熔断记忆被非 Server 错误意外清零
                }
                _ => {
                    e.err_count += 1;
                    if e.err_count >= 3 {
                        // 熔断器：30m 起指数递增（×2/次），封顶 6h
                        let exp = 30 * 60u64 << e.cb_trips.min(4);
                        e.until = now_ts() + exp.min(6 * 3600) as i64;
                        e.reason = "consecutive_errors".to_string();
                        e.err_count = 0;
                        e.cb_trips = e.cb_trips.saturating_add(1);
                    }
                }
            }
        }
    }

    /// 记录成功（重置熔断与错误计数）
    pub fn note_success(&self, uid: &str) {
        let mut entries = safe_lock(&self.entries);
        if let Some(e) = entries.get_mut(uid) {
            e.err_count = 0;
            e.cb_trips = 0;
            e.successes = e.successes.saturating_add(1);
        }
    }

    /// 清除所有账号的内存冷却状态（不影响 disabled/SessionDead）
    pub fn clear_cooldowns(&self) -> usize {
        let mut entries = safe_lock(&self.entries);
        let now = now_ts();
        let count = entries
            .values()
            .filter(|e| (e.until > 0 && now < e.until) || (e.hard_until > 0 && now < e.hard_until))
            .count();
        for e in entries.values_mut() {
            e.until = 0;
            e.hard_until = 0;
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
                state: e.state_str(now).to_string(),
            })
            .collect();
        out.sort_by(|a, b| a.uid.cmp(&b.uid));
        out
    }

    pub fn count(&self) -> usize {
        safe_lock(&self.entries).len()
    }

    /// 池内是否存在可选账号（healthy + 非零积分 + 未过期）。
    /// 统一调度选池健康预检用（§4.1 ④）；不含 Key 级白名单/专一约束——
    /// 那由各执行路径取号时自理，预检仅覆盖"池整体耗尽"场景
    pub fn has_selectable(&self) -> bool {
        let entries = safe_lock(&self.entries);
        let now = now_ts();
        let tried = HashSet::new();
        entries.values().any(|e| selectable(e, &tried, now))
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
                } else if e.hard_until > 0 && now < e.hard_until {
                    format!("hard_credit(until_0400={}s)", e.hard_until - now)
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
    // WorkBuddy 上游凭证头字段（T2.1；SOLO 账号为空串/false）
    pub domain: String,
    pub enterprise_id: String,
    pub global_region: bool,
}

/// WorkBuddy 账号入池同步结构（T2.1）
pub struct WbSyncAccount {
    pub uid: String,
    pub name: String,
    pub token: String,
    pub domain: String,
    pub enterprise_id: String,
    pub global_region: bool,
    pub credits: Option<f64>,
    pub needs_relogin: bool,
}

/// 候选过滤：healthy + 未 tried + 积分未过期 + 非零积分 + 非 hard_credit 冷却
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
    now: i64,
) -> Option<&'a PoolEntry> {
    if cands.is_empty() {
        return None;
    }
    match strategy {
        PoolStrategy::Random => Some(cands[(rand_seed as usize) % cands.len()]),
        PoolStrategy::Weighted => pick_weighted(cands, rand_seed, now),
        PoolStrategy::P2C => pick_p2c(cands, rand_seed, now),
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

/// 三因子加权随机（T2.2）：积分占比×10 + 闲置补偿（每小时+0.5 封顶 5.0，
/// 从未使用按满额）+ 成功率×3 → Top5 短名单内按得分二次加权随机
fn pick_weighted<'a>(cands: &[&'a PoolEntry], rand_seed: u64, now: i64) -> Option<&'a PoolEntry> {
    let total_credits: f64 = cands.iter().filter_map(|e| e.credits).sum();
    let mut scored: Vec<(&'a PoolEntry, f64)> = cands
        .iter()
        .map(|e| (*e, e.weighted_score(total_credits, now)))
        .collect();
    // 得分降序，取 Top5 短名单
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(5);
    // 短名单内按得分加权随机（最低权重 0.1 防零分账号永不出场）
    let weights: Vec<f64> = scored.iter().map(|(_, s)| s.max(0.1)).collect();
    let sum: f64 = weights.iter().sum();
    let mut point = (rand_seed % 10_000) as f64 / 10_000.0 * sum;
    for (e, w) in scored.iter().zip(weights.iter()) {
        point -= w;
        if point <= 0.0 {
            return Some(e.0);
        }
    }
    scored.last().map(|(e, _)| *e)
}

/// P2C：随机选二取优（T2.2 v1.2，antigravity-tools 实证延迟优于轮询/加权随机）
/// 「优」= 三因子得分高者；仅一名候选时直接返回
fn pick_p2c<'a>(cands: &[&'a PoolEntry], rand_seed: u64, now: i64) -> Option<&'a PoolEntry> {
    if cands.len() == 1 {
        return Some(cands[0]);
    }
    let total_credits: f64 = cands.iter().filter_map(|e| e.credits).sum();
    let a = cands[(rand_seed as usize) % cands.len()];
    let b = cands[((rand_seed >> 32) as usize) % cands.len()];
    if a.uid == b.uid {
        // 撞号：退化为随机一个
        return Some(cands[(rand_seed as usize) % cands.len()]);
    }
    let sa = a.weighted_score(total_credits, now);
    let sb = b.weighted_score(total_credits, now);
    Some(if sa >= sb { a } else { b })
}

/// 次日 04:00（本地东八区，与代码库其它处 local_ts 约定一致）的 Unix 秒
pub fn next_0400(now: i64) -> i64 {
    let local = now + 8 * 3600;
    let day_start = local - (local % 86400);
    let today_0400 = day_start + 4 * 3600;
    let target = if local < today_0400 { today_0400 } else { today_0400 + 86400 };
    target - 8 * 3600
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
            dc_id: None,
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
        assert_eq!(PoolStrategy::parse("weighted"), PoolStrategy::Weighted);
        assert_eq!(PoolStrategy::parse("p2c"), PoolStrategy::P2C);
        assert_eq!(PoolStrategy::default(), PoolStrategy::ExpireFirst);
    }

    #[test]
    fn resolve_wb_follows_trae_when_wb_empty() {
        // Buddy 池 wb_strategy 空 = 跟随 Trae 池策略（启动/热应用共用语义）
        assert_eq!(PoolStrategy::resolve_wb("weighted", ""), PoolStrategy::Weighted);
        assert_eq!(PoolStrategy::resolve_wb("p2c", ""), PoolStrategy::P2C);
        // Trae 池也为空/未知 → 默认 expire_first
        assert_eq!(PoolStrategy::resolve_wb("", ""), PoolStrategy::ExpireFirst);
        assert_eq!(PoolStrategy::resolve_wb("unknown", ""), PoolStrategy::ExpireFirst);
    }

    #[test]
    fn resolve_wb_prefers_explicit_wb() {
        // Buddy 池显式配置优先于 Trae 池（含 Trae 为空串时仍生效）
        assert_eq!(PoolStrategy::resolve_wb("expire_first", "p2c"), PoolStrategy::P2C);
        assert_eq!(PoolStrategy::resolve_wb("", "weighted"), PoolStrategy::Weighted);
        // 显式未知值回退默认（与 parse 语义一致）
        assert_eq!(PoolStrategy::resolve_wb("weighted", "unknown"), PoolStrategy::ExpireFirst);
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

    // ==================== T2.2 新增 ====================

    #[test]
    fn weighted_prefers_more_credits_and_idle() {
        // 同成功率基线（无历史 0.5）+ 无闲置差（last_used=0 满额）→ 积分多者得分高
        let pool = build_pool(&[
            ("uid_a", 10.0, 0),
            ("uid_b", 900.0, 0),
        ]);
        pool.set_strategy(PoolStrategy::Weighted);
        // Top5 短名单只有两名，uid_b 得分显著更高；多次取样必然覆盖 uid_b
        let mut seen_b = false;
        for i in 0..32u64 {
            let _ = i;
            if pool.pick_excluding(&HashSet::new()).unwrap().uid == "uid_b" {
                seen_b = true;
                break;
            }
        }
        assert!(seen_b, "weighted 策略应能取到高分账号 uid_b");
        // 闲置补偿：uid_a 刚被用过、uid_b 闲置 3h → uid_b 得分更高
        // 直接对 pick_weighted 纯函数做断言（不依赖随机落点）
        let e_a = crate::api_server::pool::PoolEntry {
            uid: "a".into(), name: String::new(), jwt: String::new(),
            credits: Some(100.0), credits_expire_at: None, disabled: false,
            err_count: 0, until: 0, reason: String::new(),
            device_id: String::new(), machine_id: String::new(),
            hard_until: 0, last_used: 1000, successes: 0, failures: 0, cb_trips: 0,
            domain: String::new(), enterprise_id: String::new(), global_region: false,
        };
        let e_b = crate::api_server::pool::PoolEntry {
            last_used: 1000 - 3 * 3600, // 闲置 3 小时
            ..crate::api_server::pool::PoolEntry {
                uid: "b".into(), name: String::new(), jwt: String::new(),
                credits: Some(100.0), credits_expire_at: None, disabled: false,
                err_count: 0, until: 0, reason: String::new(),
                device_id: String::new(), machine_id: String::new(),
                hard_until: 0, last_used: 0, successes: 0, failures: 0, cb_trips: 0,
                domain: String::new(), enterprise_id: String::new(), global_region: false,
            }
        };
        let now = 1000 + 60;
        let sa = e_a.weighted_score(200.0, now);
        let sb = e_b.weighted_score(200.0, now);
        assert!(sb > sa, "闲置 3h 的账号得分应高于刚用过的账号（{} vs {}）", sb, sa);
    }

    #[test]
    fn p2c_picks_better_of_two() {
        // 纯函数语义：随机选二取优（得分高者胜）；两候选相同时退化为随机
        let e_a = test_entry("a", 10.0, 1000);
        let e_b = test_entry("b", 900.0, 1000);
        let cands = vec![&e_a, &e_b];
        let now = 2000;
        // 两候选不同时（奇数种子 → 索引 (1,0)），恒选得分更高的 b
        for seed in (1..200u64).step_by(2) {
            assert_eq!(pick_p2c(&cands, seed, now).unwrap().uid, "b");
        }
        // 撞号（两索引相同）→ 不 panic，返回任一候选
        for seed in (0..200u64).step_by(2) {
            let picked = pick_p2c(&cands, seed, now).unwrap();
            assert!(picked.uid == "a" || picked.uid == "b");
        }
        // 单候选：直接返回
        let single = vec![&e_b];
        assert_eq!(pick_p2c(&single, 7, now).unwrap().uid, "b");
    }

    /// 测试用 PoolEntry 快速构造
    fn test_entry(uid: &str, credits: f64, last_used: i64) -> PoolEntry {
        PoolEntry {
            uid: uid.to_string(),
            name: String::new(),
            jwt: String::new(),
            credits: Some(credits),
            credits_expire_at: None,
            disabled: false,
            err_count: 0,
            until: 0,
            reason: String::new(),
            device_id: String::new(),
            machine_id: String::new(),
            hard_until: 0,
            last_used,
            successes: 0,
            failures: 0,
            cb_trips: 0,
            domain: String::new(),
            enterprise_id: String::new(),
            global_region: false,
        }
    }

    #[test]
    fn hard_credit_cools_until_next_0400() {
        let pool = build_pool(&[("uid_a", 0.0, 0), ("uid_b", 5.0, 0)]);
        // 强制注入：模拟积分耗尽（hard_credit 由上游错误触发，不走 selectable 的零积分捷径）
        pool.note_error("uid_b", ErrKind::HardCredit);
        // uid_b 进入 QuotaProtection
        assert_eq!(pool.status_list()[1].state, "QuotaProtection");
        // diagnose 给出 hard_credit 原因
        let d = pool.diagnose().into_iter().find(|x| x.uid == "uid_b").unwrap();
        assert!(d.reason.starts_with("hard_credit"));
        // 恢复探测：把 hard_until 手动置为过去 → 重新可选
        {
            let mut entries = safe_lock(&pool.entries);
            if let Some(e) = entries.get_mut("uid_b") {
                e.hard_until = now_ts() - 1;
            }
        }
        assert_eq!(pool.status_list()[1].state, "Available");
    }

    #[test]
    fn forbidden_disables_account() {
        let pool = build_pool(&[("uid_a", 10.0, 0), ("uid_b", 10.0, 0)]);
        pool.note_error("uid_b", ErrKind::Forbidden);
        assert_eq!(pool.status_list()[1].state, "Forbidden");
        assert!(pool.pick_excluding(&HashSet::new()).unwrap().uid != "uid_b");
    }

    #[test]
    fn server_circuit_breaker_backs_off_exponentially() {
        let pool = build_pool(&[("uid_a", 10.0, 0)]);
        // 三次连续 Server 错误触发第一次熔断（30m）
        for _ in 0..3 {
            pool.note_error("uid_a", ErrKind::Server);
        }
        let s1 = pool.status_list()[0].cooldown_until.unwrap() - now_ts();
        assert!((1799..=1801).contains(&s1), "首次熔断应约 30m，实际 {}s", s1);
        // 手动解除后再触发第二次 → 指数递增到 1h
        pool.clear_cooldowns();
        // clear_cooldowns 重置 until 但 cb_trips 保留（指数递增的记忆）
        {
            let mut entries = safe_lock(&pool.entries);
            entries.get_mut("uid_a").unwrap().cb_trips = 1;
        }
        for _ in 0..3 {
            pool.note_error("uid_a", ErrKind::Server);
        }
        let s2 = pool.status_list()[0].cooldown_until.unwrap() - now_ts();
        assert!((3599..=3601).contains(&s2), "第二次熔断应约 1h，实际 {}s", s2);
        // 成功重置熔断
        pool.clear_cooldowns();
        pool.note_success("uid_a");
        {
            let entries = safe_lock(&pool.entries);
            assert_eq!(entries.get("uid_a").unwrap().cb_trips, 0);
        }
    }

    #[test]
    fn next_0400_is_between_1s_and_24h_away() {
        let now = now_ts();
        for offset in [0i64, 3600, 61_200, 86_399] {
            let t = next_0400(now + offset);
            let delta = t - (now + offset);
            assert!(delta > 0, "必须在未来");
            assert!(delta <= 24 * 3600);
            // 目标时刻的本地时钟（东八区）恰好落在 04:00
            let local = t + 8 * 3600;
            assert_eq!(local % 86400, 4 * 3600);
        }
    }

    // ==================== P2 修复10：cb_trips 不被非 Server 错误重置 ====================

    #[test]
    fn plan_limit_cooldown_keeps_circuit_memory() {
        let pool = build_pool(&[("uid_a", 10.0, 0)]);
        // 预置熔断记忆（此前被 PlanLimit/SoftRate/NotFound 分支误重置）
        {
            let mut entries = safe_lock(&pool.entries);
            entries.get_mut("uid_a").unwrap().cb_trips = 2;
        }
        // 三类非 Server 错误均不应清掉 cb_trips（各自冷却时长照常生效）
        pool.note_error("uid_a", ErrKind::PlanLimit);
        assert!(pool.status_list()[0].cooling, "PlanLimit 应进入 12h 冷却");
        {
            let entries = safe_lock(&pool.entries);
            assert_eq!(
                entries.get("uid_a").unwrap().cb_trips,
                2,
                "PlanLimit 不得重置熔断记忆"
            );
        }
        // SoftRate 冷却结束后再验证
        {
            let mut entries = safe_lock(&pool.entries);
            entries.get_mut("uid_a").unwrap().until = 0;
        }
        pool.note_error("uid_a", ErrKind::SoftRate);
        assert!(pool.status_list()[0].cooling, "SoftRate 应进入 60s 冷却");
        {
            let entries = safe_lock(&pool.entries);
            assert_eq!(entries.get("uid_a").unwrap().cb_trips, 2);
        }
        // note_success 仍是唯一重置点
        pool.note_success("uid_a");
        {
            let entries = safe_lock(&pool.entries);
            assert_eq!(entries.get("uid_a").unwrap().cb_trips, 0);
        }
    }

    #[test]
    fn wb_sync_and_pick_carries_region_fields() {
        let pool = ApiPool::new();
        pool.sync_from_wb(
            &[crate::api_server::pool::WbSyncAccount {
                uid: "wb-abc".into(),
                name: "WB账号".into(),
                token: "tk".into(),
                domain: "workbuddy.ai".into(),
                enterprise_id: "e1".into(),
                global_region: true,
                credits: Some(50.0),
                needs_relogin: false,
            }],
            &["wb-abc".to_string()],
        );
        pool.set_strategy(PoolStrategy::CreditFirst);
        let picked = pool.pick_excluding(&HashSet::new()).unwrap();
        assert_eq!(picked.uid, "wb-abc");
        assert!(picked.global_region);
        assert_eq!(picked.domain, "workbuddy.ai");
        assert_eq!(picked.enterprise_id, "e1");
        // needs_relogin → Forbidden，不参与取号
        let pool2 = ApiPool::new();
        pool2.sync_from_wb(
            &[crate::api_server::pool::WbSyncAccount {
                uid: "wb-x".into(), name: String::new(), token: "tk".into(),
                domain: String::new(), enterprise_id: String::new(),
                global_region: false, credits: None, needs_relogin: true,
            }],
            &["wb-x".to_string()],
        );
        assert!(pool2.pick_excluding(&HashSet::new()).is_none());
    }
}
