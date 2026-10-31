use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
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

    /// Qoder 池生效策略：qoder_strategy 独立配置优先；空 = 跟随 Trae 池策略
    ///（与 Buddy 同语义，复用同一跟随规则）
    pub fn resolve_qoder(strategy: &str, qoder_strategy: &str) -> Self {
        Self::resolve_wb(strategy, qoder_strategy)
    }

    /// Buddy 国际版（buddy-ai）池生效策略：buddy_ai_strategy 独立配置优先；
    /// 空 = 跟随 Trae 池策略（与 Buddy/Qoder 同语义；启动与 pool_set 热应用共用）
    pub fn resolve_buddy_ai(strategy: &str, buddy_ai_strategy: &str) -> Self {
        Self::resolve_wb(strategy, buddy_ai_strategy)
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

/// reserve 过滤新鲜度门槛（issue #91 缓存问题）：积分快照采集时间距今超过
/// 该值视为陈旧，reserve 过滤失效（fail-open）——陈旧「高积分」快照照样会
/// 调度秃号，fail-closed 反而额外误杀；恢复靠启动保鲜（do_start 后台刷新）
/// 与运行期 update_credits 拨新。24h 与 trae-credits-snapshot daily 模式对齐
const RESERVE_STALE_SECS: i64 = 24 * 3600;

/// 池中单个账号的运行时状态
pub struct PoolEntry {
    pub uid: String,
    pub name: String,
    pub jwt: String,
    pub credits: Option<f64>,
    pub credits_expire_at: Option<i64>,
    /// 积分快照采集时间（Unix 秒，issue #91 缓存保鲜）：
    /// None = 无时间信息（升级前旧数据/无快照），reserve 过滤同样不生效
    pub credits_fetched_at: Option<i64>,
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
    /// refresh_token 已判定失效（F-78 批次 3）：同步自 RawAccount.refresh_token_invalid，
    /// 与 disabled 联动（重新 OAuth 登录后 sync 自动恢复）
    pub refresh_invalid: bool,
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

    /// reserve 过滤是否生效（issue #91 缓存问题）：积分快照存在且新鲜
    /// （≤ RESERVE_STALE_SECS）才生效；缺失（None）/陈旧/未来时间戳均视为
    /// 陈旧 → fail-open。零积分（c<=0）过滤不经过此门槛，保持既有无条件行为
    fn reserve_filter_active(&self, now: i64) -> bool {
        match self.credits_fetched_at {
            Some(t) => t <= now && now - t <= RESERVE_STALE_SECS,
            None => false,
        }
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
    /// 账号级在途并发计数（F-77）：uid → 计数器共享句柄；
    /// InflightGuard 取号后 bind，Drop（流结束/断连/panic 展开）时 -1。
    /// 计数器经 Arc 与 guard 共享，池重建（sync_*）不中断在途请求的配对释放
    inflight: Mutex<HashMap<String, Arc<std::sync::atomic::AtomicU32>>>,
    /// 账号并发上限（F-77，api_pool.json 热应用）：0 = 不限；默认 1。
    /// inflight ≥ 上限的账号视为 busy 不参与候选；全部 busy 时降级取
    /// inflight 最小者（不过载拒绝，保证请求不失败）
    concurrency_limit: std::sync::atomic::AtomicU32,
    /// 账号保留剩余积分阈值（issue #91，dispatch_policy_set 热应用）：
    /// f64 bits 存储；剩余积分 ≤ 阈值的账号不参与调度（0 = 不过滤）。
    /// 防止账号积分被调度耗尽后连免费模型也不可用
    reserve_credits: std::sync::atomic::AtomicU64,
    /// 冷却/禁用标记的 app.log 观测上下文：(data_dir, 池标签)。
    /// do_start 装配三池（trae/buddy/qoder）时注入；None（测试/未启动）不落日志
    log_ctx: Mutex<Option<(std::path::PathBuf, String)>>,
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
            inflight: Mutex::new(HashMap::new()),
            concurrency_limit: std::sync::atomic::AtomicU32::new(1),
            reserve_credits: std::sync::atomic::AtomicU64::new(0.0f64.to_bits()),
            log_ctx: Mutex::new(None),
        }
    }

    /// 装配冷却/禁用标记的日志上下文：池标签 + 数据目录（do_start 启动时调用）；
    /// 未装配（测试/未启动网关）时 note_error 不落 app.log，无副作用
    pub fn set_log_ctx(&self, data_dir: &std::path::Path, label: &str) {
        *safe_lock(&self.log_ctx) = Some((data_dir.to_path_buf(), label.to_string()));
    }

    /// 设置账号并发上限（F-77，pool_set 热应用；0 = 不限）
    pub fn set_concurrency_limit(&self, limit: u32) {
        self.concurrency_limit
            .store(limit, std::sync::atomic::Ordering::Relaxed);
    }

    /// 账号并发上限当前值
    pub fn concurrency_limit(&self) -> u32 {
        self.concurrency_limit.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 设置账号保留剩余积分阈值（issue #91，dispatch_policy_set 热应用）：
    /// 负值防御归零（0 = 关闭过滤）
    pub fn set_reserve_credits(&self, v: f64) {
        self.reserve_credits
            .store(v.max(0.0).to_bits(), std::sync::atomic::Ordering::Relaxed);
    }

    /// 账号保留剩余积分阈值当前值
    pub fn reserve_credits(&self) -> f64 {
        f64::from_bits(self.reserve_credits.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// 是否存在积分快照陈旧/缺时间戳的账号（issue #91 启动保鲜判定）：
    /// 此类账号 reserve 过滤 fail-open 不生效，由 do_start 后台刷新拨新恢复。
    /// reserve 阈值关闭（0）时新鲜度判定无意义（reserve 过滤恒不生效），短路返回
    /// false——避免每次启动都触发全量余额刷新的网络副作用（B2，审查 2026-10-13）
    pub fn has_stale_credits(&self) -> bool {
        if self.reserve_credits() <= 0.0 {
            return false;
        }
        let entries = safe_lock(&self.entries);
        let now = now_ts();
        entries
            .values()
            .any(|e| e.credits.is_some() && !e.reserve_filter_active(now))
    }

    /// Qoder 池启动保鲜判定（issue #102）：是否存在会被 selectable 持续排除的
    /// 钉死快照——零/负余额（零积分过滤无新鲜度门槛，充值后不拨新不恢复）或
    /// 积分已到期（续期后同理）。与 has_stale_credits 的 reserve 新鲜度口径互补：
    /// 后者 reserve=0 时短路 false，覆盖不到本判定的两种场景。
    /// disabled（needs_relogin）账号不参与判定——刷新注定 401 永久失败，
    /// 计入只会让每次启动都触发注定失败的全池刷新（噪音 + 无谓网络请求）
    pub fn has_zero_or_expired_credits(&self) -> bool {
        let entries = safe_lock(&self.entries);
        let now = now_ts();
        entries.values().any(|e| {
            !e.disabled
                && (e.credits.is_some_and(|c| c <= 0.0)
                    || e.credits_expire_at.is_some_and(|exp| exp > 0 && exp < now))
        })
    }

    /// 账号在途计数共享句柄（F-77）：InflightGuard 取号后 bind_account 使用；
    /// 不存在则创建（计数从 0 开始，bind 时 +1）
    pub fn inflight_handle(&self, uid: &str) -> Arc<std::sync::atomic::AtomicU32> {
        let mut m = safe_lock(&self.inflight);
        m.entry(uid.to_string())
            .or_default()
            .clone()
    }

    /// 在途计数快照（调度 busy 过滤与 P2C/Weighted 负载因子数据源）
    fn inflight_snapshot(&self) -> HashMap<String, u32> {
        let m = safe_lock(&self.inflight);
        m.iter()
            .map(|(uid, c)| (uid.clone(), c.load(std::sync::atomic::Ordering::Relaxed)))
            .collect()
    }

    /// 设置调度策略（启动时由 api_pool.json 决定）
    pub fn set_strategy(&self, s: PoolStrategy) {
        *safe_lock(&self.strategy) = s;
    }

    /// 健康账号池画像（智能调度因子，dispatch::smart_pool_order 数据源）：
    /// (最早积分到期时间, 健康账号剩余积分总和)；无到期数据 → None。
    /// 积分总和不计「新鲜快照且低于保留阈值」的账号（issue #91）——被 reserve
    /// 过滤的账号实际不可调度；陈旧快照 fail-open 与 selectable 口径一致（计入）
    pub fn stats(&self) -> (Option<i64>, f64) {
        let entries = safe_lock(&self.entries);
        let now = now_ts();
        let reserve = self.reserve_credits();
        let mut earliest: Option<i64> = None;
        let mut total = 0.0;
        for e in entries.values() {
            if !e.healthy(now) {
                continue;
            }
            if let Some(c) = e.credits {
                if c > reserve || !e.reserve_filter_active(now) {
                    // max(0)：负积分（对账异常快照）不应拉低/拉负总分影响 smart 排序
                    total += c.max(0.0);
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
        credits_fetched_at: Option<i64>,
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
                // F-78 批次 3：refresh_token 判定失效的账号同步禁用（需重新 OAuth 登录后恢复）
                let refresh_invalid = a.refresh_token_invalid;
                let disabled = cd.error_type == "SessionDead" || refresh_invalid;
                // 聊天设备身份策略：
                // - machine_id 始终按 uid 确定性派生（seeded_hex(64, uid, "mach")），跨机器一致，
                //   天然满足"一账号一设备指纹"，不与 OAuth 登录层的机器级共享身份（icube 凭证）混用。
                // - device_id 优先取 device_map 中用户显式登记的设备；缺条目时回落 derive_device(uid)
                //   按 uid 派生（与签到伪设备同一派生体系）。绝不发送空 device_id：
                //   "x-device-id: ''" 是显性指纹异常，容易在上游形成聚集特征触发影子风控。
                let (device_id, machine_id) = match device_map.get(uid) {
                    // 空/空白 device_id（手工登记残缺）视同缺条目回落派生，对齐
                    // trae_checkin get_device_for 口径——绝不发送空 device_id
                    Some(d) if !d.device_id.trim().is_empty() => {
                        (d.device_id.clone(), seeded_hex(64, uid, "mach"))
                    }
                    _ => (
                        crate::commands::accounts::derive_device(uid).device_id,
                        seeded_hex(64, uid, "mach"),
                    ),
                };
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
                        // 快照文件级采集时间（issue #91 缓存保鲜）：一次刷新
                        // 全量写入 credits 与 updated_at，时间戳对全体条目生效
                        credits_fetched_at,
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
                        refresh_invalid,
                    },
                );
            }
        }
        // F-77：清理已不在池内的在途计数条目（在途请求经 Arc 独立释放，不受影响）
        {
            let mut inflight = safe_lock(&self.inflight);
            inflight.retain(|uid, _| entries.contains_key(uid));
        }
    }

    /// 池运行期三键拨新（issue #67 运行期回写 + issue #102 Qoder / issue #109
    /// Trae/WB 运行期保鲜）：credits + credits_expire_at + credits_fetched_at 一并回写。
    /// selectable 到期过滤与 expire_first/smart「先到期先消耗」排序依赖
    /// credits_expire_at——旧值钉死会让充值/续期后的账号持续被排除、新登录账号
    /// expire=None 被「有过期时间者优先」恒压，直到网关重启；快照时间戳同步拨新
    /// （issue #91 缓存保鲜），reserve 过滤自动恢复生效。
    /// 未提及的账号保持原值，不触碰冷却/inflight/禁用等运行态（与 sync_from_accounts
    /// 全量重建互补——后者会重置 hard_until/last_used 等运行状态）
    pub fn update_credits_snapshot(&self, snapshot: &HashMap<String, (f64, Option<i64>)>) {
        let mut entries = safe_lock(&self.entries);
        let now = now_ts();
        for (uid, (c, expire)) in snapshot {
            if let Some(e) = entries.get_mut(uid) {
                e.credits = Some(*c);
                e.credits_fetched_at = Some(now);
                e.credits_expire_at = *expire;
            }
        }
    }

    /// Qoder 签到奖励运行期增量回写（issue #102 补缺）：credits += delta 并拨新
    /// fetched_at——不拨新则充值/领取到账的账号仍按旧快照被零积分过滤排除，直到
    /// 下次积分快照（every_hours 2h / daily 档最长约 13.5h 的排除窗口）。
    /// 与 update_credits_snapshot 不同：**不动 credits_expire_at**（签到奖励不改变
    /// 订阅/包到期，覆写会丢真值）。credits 为 None 按 0.0 基数兜底，结果钳制非负；
    /// 不触碰冷却/inflight/禁用等运行态（对齐 update_credits 纪律）；池内不存在
    /// 的账号静默跳过（与两回写方法同口径）。网关未运行时由调用方跳过（None 句柄）。
    pub fn apply_credits_delta(&self, uid: &str, delta: f64) {
        if delta == 0.0 {
            return;
        }
        let mut entries = safe_lock(&self.entries);
        if let Some(e) = entries.get_mut(uid) {
            let base = e.credits.unwrap_or(0.0).max(0.0);
            e.credits = Some((base + delta).max(0.0));
            e.credits_fetched_at = Some(now_ts());
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
                    credits_expire_at: a.credits_expire_at,
                    credits_fetched_at: a.credits_fetched_at,
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
                    refresh_invalid: false,
                },
            );
        }
        // F-77：清理已不在池内的在途计数条目（在途请求经 Arc 独立释放，不受影响）
        {
            let mut inflight = safe_lock(&self.inflight);
            inflight.retain(|uid, _| entries.contains_key(uid));
        }
    }

    /// 从 Qoder 账号同步池（p3-3）：Qoder 专用池实例（`ApiSharedState.qoder_pool`），
    /// 全量重建语义同 sync_from_wb。积分/到期快照随 QoderSyncAccount 装配
    ///（issue #102 智能调度三键生效 + issue #91 reserve 保留额过滤）；
    /// machine_id 透传账号记录值（可能为空——请求时由 identity 回调解析
    /// effective 凭证补齐）
    pub fn sync_from_qoder(&self, accounts: &[QoderSyncAccount], enabled: &[String]) {
        let mut entries = safe_lock(&self.entries);
        entries.clear();
        let enabled_set: HashSet<&str> = enabled.iter().map(|s| s.as_str()).collect();
        for a in accounts {
            if !enabled_set.contains(a.uid.as_str()) || a.access_token.is_empty() {
                continue;
            }
            entries.insert(
                a.uid.clone(),
                PoolEntry {
                    uid: a.uid.clone(),
                    name: a.name.clone(),
                    jwt: a.access_token.clone(),
                    credits: a.credits,
                    credits_expire_at: a.credits_expire_at,
                    credits_fetched_at: a.credits_fetched_at,
                    disabled: a.needs_relogin,
                    err_count: 0,
                    until: 0,
                    reason: String::new(),
                    device_id: String::new(),
                    machine_id: a.machine_id.clone(),
                    hard_until: 0,
                    last_used: 0,
                    successes: 0,
                    failures: 0,
                    cb_trips: 0,
                    domain: String::new(),
                    enterprise_id: String::new(),
                    global_region: false,
                    refresh_invalid: false,
                },
            );
        }
        // F-77：清理已不在池内的在途计数条目（在途请求经 Arc 独立释放，不受影响）
        {
            let mut inflight = safe_lock(&self.inflight);
            inflight.retain(|uid, _| entries.contains_key(uid));
        }
    }

    /// 指定 uid 取号（T2.4 会话粘性）：账号 healthy 时返回其凭证，否则 None。
    /// `skip_credit_filters`：跳过积分维度过滤（issue #111 免费模型豁免，
    /// 仅 Qoder 免费模型取号路径传 true），见 [`selectable`]
    pub fn pick_by_uid(&self, uid: &str, skip_credit_filters: bool) -> Option<PickedAccount> {
        let entries = safe_lock(&self.entries);
        let now = now_ts();
        let tried = HashSet::new();
        let reserve = self.reserve_credits();
        entries.get(uid).filter(|e| selectable(e, &tried, now, reserve, skip_credit_filters)).map(|e| PickedAccount {
            uid: e.uid.clone(),
            jwt: e.jwt.clone(),
            device_id: e.device_id.clone(),
            machine_id: e.machine_id.clone(),
            domain: e.domain.clone(),
            enterprise_id: e.enterprise_id.clone(),
            global_region: e.global_region,
            // 专一/粘性锁定：忽略 busy 过滤（语义即「锁定即用」），无条件绑定
            over_limit: true,
        })
    }

    /// 带 Key 约束取号（F-35 子 Key 体系，批次3）：
    /// - `dedicated`：专一模式绑定 uid（healthy 即直接锁定，绕过策略）
    /// - `allowed`：上游白名单过滤（None = 不限；`Some(空集)` = 过滤全部——
    ///   issue #30 混合白名单借空集表达「该池被此 Key 排除」，调用方可有意传入）
    /// - 调度策略沿用池当前策略（子 Key「临期优先」= 池默认 expire_first，
    ///   池策略本身即用户可选的临期/积分/加权等模式；约束仅做过滤与锁定）
    pub fn pick_excluding_constrained(
        &self,
        tried: &HashSet<String>,
        allowed: Option<&HashSet<String>>,
        dedicated: Option<&str>,
    ) -> Option<PickedAccount> {
        // 积分维度过滤无豁免（false）：免费模型豁免仅 Qoder 走 ev 版本
        self.pick_excluding_constrained_ev(tried, allowed, dedicated, false)
            .map(|(p, _)| p)
    }

    /// 同 [pick_excluding_constrained]，附带 F-77 调度事件（[SCHED] 日志用），
    /// 并内置防惊群让位（T2.2：100ms 内重复选中同一 uid 且有其他候选时让位）：
    /// - `busy_yield`：候选中存在被并发上限过滤的 busy 账号（让位给空闲账号）
    /// - `busy_fallback`：全部候选 busy，降级取 inflight 最小者（不过载拒绝）
    pub fn pick_excluding_constrained_ev(
        &self,
        tried: &HashSet<String>,
        allowed: Option<&HashSet<String>>,
        dedicated: Option<&str>,
        skip_credit_filters: bool,
    ) -> Option<(PickedAccount, Option<String>)> {
        // 专一模式：绑定账号 healthy 且未试错过 → 直接锁定（专一绑定不让位）
        if let Some(uid) = dedicated {
            if !tried.contains(uid) {
                if let Some(p) = self.pick_by_uid(uid, skip_credit_filters) {
                    let mut entries = safe_lock(&self.entries);
                    if let Some(e) = entries.get_mut(uid) {
                        e.last_used = now_ts();
                    }
                    return Some((p, None));
                }
            }
        }
        let mut entries = safe_lock(&self.entries);
        let strategy = *safe_lock(&self.strategy);
        let now = now_ts();
        let reserve = self.reserve_credits();
        let all_cands: Vec<&PoolEntry> = entries
            .values()
            .filter(|e| selectable(e, tried, now, reserve, skip_credit_filters))
            .filter(|e| allowed.map_or(true, |a| a.contains(&e.uid)))
            .collect();
        // F-77 busy 过滤 + 全 busy 降级
        let inflight = self.inflight_snapshot();
        let limit = self.concurrency_limit();
        let (cands, busy_fallback) = busy_filter(&all_cands, &inflight, limit);
        let rand_seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() ^ (d.subsec_nanos() as u64).wrapping_mul(0x9e3779b97f4a7c15))
            .unwrap_or(0);
        let mut picked = if busy_fallback {
            all_cands
                .iter()
                .copied()
                .min_by_key(|e| inflight_of(e, &inflight))
        } else {
            pick_by_strategy(&cands, strategy, rand_seed, now, &inflight)
        }?;

        // 防惊群：100ms 内重复选中同一 uid 且还有其他候选 → 让位（T2.2）；
        // 全 busy 降级路径不参与——min(inflight) 本身即负载分散，策略让位反而
        // 会破坏「取在途最小者」语义（F-77）
        if !busy_fallback && cands.len() > 1 {
            let mut recent = safe_lock(&self.recent_pick);
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
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
                    &inflight,
                ) {
                    picked = alt;
                }
            }
            *recent = (picked.uid.clone(), now_ms);
        }
        let account = PickedAccount {
            uid: picked.uid.clone(),
            jwt: picked.jwt.clone(),
            device_id: picked.device_id.clone(),
            machine_id: picked.machine_id.clone(),
            domain: picked.domain.clone(),
            enterprise_id: picked.enterprise_id.clone(),
            global_region: picked.global_region,
            // busy_fallback 降级（全候选 busy 取 in-flight 最小者）保留「不过载拒绝」
            // 既有语义 → 无条件绑定；正常路径交由路由侧 CAS 复核
            over_limit: busy_fallback,
        };
        // F-77⑤ 可观测：busy 让位/降级事件（无 busy 过滤发生则 None）
        let event = if limit > 0 && busy_fallback {
            Some(format!(
                "busy_fallback picked={} inflight={} limit={}",
                picked.uid,
                inflight_of(picked, &inflight),
                limit
            ))
        } else if limit > 0 && cands.len() < all_cands.len() {
            Some(format!(
                "busy_yield filtered={} picked={}",
                all_cands.len() - cands.len(),
                picked.uid
            ))
        } else {
            None
        };
        drop(cands); // 释放 entries 不可变借用后再更新 last_used
        if let Some(e) = entries.get_mut(&account.uid) {
            e.last_used = now;
        }
        Some((account, event))
    }

    /// F-77④ 粘性让位取号：粘性账号 busy（inflight ≥ 并发上限）且池内存在其他
    /// 空闲健康候选时让位改走调度策略（并发健康优先于上游 KV cache 复用）；
    /// 全候选 busy / 未启用并发上限（limit=0）/ 无其他候选时保持粘性锁定。
    /// 返回 (账号, 调度事件)：事件非 None 时由调用方记 [SCHED] 日志
    /// （sticky_yield=让位改选 / sticky_fallback=全 busy 保持粘性）。
    pub fn pick_sticky_yield(
        &self,
        sticky_uid: &str,
        allowed: Option<&HashSet<String>>,
        skip_credit_filters: bool,
    ) -> Option<(PickedAccount, Option<String>)> {
        let sticky = self.pick_by_uid(sticky_uid, skip_credit_filters)?;
        let limit = self.concurrency_limit();
        let inflight = self.inflight_snapshot();
        let sticky_inflight = inflight.get(sticky_uid).copied().unwrap_or(0);
        if limit == 0 || sticky_inflight < limit {
            // 快照判定空闲：绑定语义交路由侧 CAS 复核（TOCTOU 收紧）；
            // limit=0 未启用上限，CAS 侧按无条件绑定处理
            let mut sticky = sticky;
            sticky.over_limit = false;
            return Some((sticky, None));
        }
        // 粘性账号 busy：判定其他健康候选（同 allowed 约束；首轮无 tried）是否存在空闲
        let reserve = self.reserve_credits();
        let (has_other, has_idle) = {
            let entries = safe_lock(&self.entries);
            let now = now_ts();
            let tried = HashSet::new();
            entries
                .values()
                .filter(|e| e.uid != sticky_uid && selectable(e, &tried, now, reserve, skip_credit_filters))
                .filter(|e| allowed.map_or(true, |a| a.contains(&e.uid)))
                .fold((false, false), |(any, idle), e| {
                    (any || true, idle || inflight_of(e, &inflight) < limit)
                })
        };
        if !has_other {
            return Some((sticky, None));
        }
        if !has_idle {
            // 全 busy：保持粘性（粘性缓存收益 > 换一个同样 busy 的账号）
            return Some((
                sticky,
                Some(format!(
                    "sticky_fallback uid={sticky_uid} inflight={} limit={limit} all_busy=1",
                    sticky_inflight
                )),
            ));
        }
        // 让位：空闲候选中走调度策略（重取锁；候选在间隙被移除则回退保持粘性）
        let mut entries = safe_lock(&self.entries);
        let now = now_ts();
        let tried = HashSet::new();
        let idle: Vec<&PoolEntry> = entries
            .values()
            .filter(|e| e.uid != sticky_uid && selectable(e, &tried, now, reserve, skip_credit_filters))
            .filter(|e| allowed.map_or(true, |a| a.contains(&e.uid)))
            .filter(|e| inflight_of(e, &inflight) < limit)
            .collect();
        let strategy = *safe_lock(&self.strategy);
        let rand_seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() ^ (d.subsec_nanos() as u64).wrapping_mul(0x9e3779b97f4a7c15))
            .unwrap_or(0);
        let Some(picked) = pick_by_strategy(&idle, strategy, rand_seed, now, &inflight) else {
            return Some((sticky, None));
        };
        let account = PickedAccount {
            uid: picked.uid.clone(),
            jwt: picked.jwt.clone(),
            device_id: picked.device_id.clone(),
            machine_id: picked.machine_id.clone(),
            domain: picked.domain.clone(),
            enterprise_id: picked.enterprise_id.clone(),
            global_region: picked.global_region,
            // 让位候选取自快照空闲集：绑定语义交路由侧 CAS 复核（TOCTOU 收紧）
            over_limit: false,
        };
        let event = Some(format!(
            "sticky_yield from={sticky_uid} to={} inflight={} limit={}",
            picked.uid, sticky_inflight, limit
        ));
        if let Some(e) = entries.get_mut(&account.uid) {
            e.last_used = now;
        }
        Some((account, event))
    }

    /// 更新账号凭证（T2.6：网关 401 刷新后回填，后续取号即用新 token）
    pub fn update_jwt(&self, uid: &str, jwt: &str) {
        let mut entries = safe_lock(&self.entries);
        if let Some(e) = entries.get_mut(uid) {
            e.jwt = jwt.to_string();
        }
    }

    /// 按 uid 取账号展示名（请求日志用）；不在池中返回空串（日志侧显示 "-"）
    pub fn name_of(&self, uid: &str) -> String {
        safe_lock(&self.entries)
            .get(uid)
            .map(|e| e.name.clone())
            .unwrap_or_default()
    }

    /// F-78 批次 3：refresh_token 判定失效 → 运行时禁用（前端 refresh_jwt 失败联动；
    /// 持久化标记由 record_refresh_failure 写 accounts 文件，重启后经 sync_from_accounts 同步）
    pub fn note_refresh_invalid(&self, uid: &str) {
        let mut entries = safe_lock(&self.entries);
        if let Some(e) = entries.get_mut(uid) {
            e.refresh_invalid = true;
            e.disabled = true;
            e.reason = "refresh_token_invalid".to_string();
        }
    }

    /// 记录错误并冷却（T2.2 错误三态 + 分级冷却）——无上游解冻信息的便捷入口，
    /// 委托 [`Self::note_error_until`]（unfreeze=None 维持既有固定时长口径）
    pub fn note_error(&self, uid: &str, kind: ErrKind) {
        self.note_error_until(uid, kind, None);
    }

    /// 记录错误并冷却，可携带上游文案解析出的**账号级**精确解冻时刻（issue #159）。
    /// 根因背景：单账号限额（6004/429 带解冻话术）此前会连带把模型级冷却表写至
    /// 解冻时刻 → 整池同名模型被 pool_health 跳过（无视其余健康账号）→ 回退池
    /// 无账号 → 503 no healthy。解耦后：账号级冷却精确到该账号的解冻时刻，
    /// 模型级冷却由调用方按错误类别另行判定。
    /// unfreeze 仅对账号级冷却类生效：
    /// - HardCredit → hard_until = unfreeze（缺省回退下个 04:00）；
    /// - PlanLimit/SoftRate/NotFound → until = unfreeze（缺省回退固定时长）；
    /// - SessionDead/Forbidden（禁用）与 Server/Client/None（熔断/计数）忽略该值，
    ///   维持既有行为。
    /// unfreeze 有效性钳制（防调用侧解析异常）：[now+60s, now+7d]。
    /// 冷却/禁用标记发生时同步落一条 app.log（F-80 可观测）：
    /// `account cooldown: pool=<池> acct=<名称> uid=<uid> kind=<错误分类> action=<处置>`，
    /// 排查「账号何时、为何被打冷却→选池 no_healthy_account→跨池回退」链路时可直接定位
    pub fn note_error_until(&self, uid: &str, kind: ErrKind, unfreeze: Option<i64>) {
        let now = now_ts();
        let mut entries = safe_lock(&self.entries);
        let mut cooldown_ev: Option<(String, String)> = None;
        if let Some(e) = entries.get_mut(uid) {
            e.failures = e.failures.saturating_add(1);
            let mut action: Option<String> = None;
            match kind {
                ErrKind::SessionDead | ErrKind::Forbidden => {
                    e.disabled = true;
                    e.reason = kind.as_str().to_string();
                    action = Some("action=disabled".to_string());
                }
                ErrKind::HardCredit => {
                    // 积分耗尽：优先上游解冻时刻（issue #159），缺省冷却到下个
                    // 04:00（凌晨 00:00–04:00 窗口内为当天，next_0400 语义），
                    // 到期自动恢复探测（F-29 v1.2）
                    let until = match unfreeze {
                        Some(u) => clamp_unfreeze(u, now),
                        None => next_0400(now),
                    };
                    e.hard_until = until;
                    e.reason = "hard_credit".to_string();
                    e.err_count = 0;
                    action = Some(format!(
                        "action=hard_cooldown until={} ({})",
                        fmt_local_ts(until),
                        if unfreeze.is_some() { "上游解冻时刻" } else { "下个04:00" }
                    ));
                }
                ErrKind::PlanLimit | ErrKind::SoftRate | ErrKind::NotFound => {
                    let until = match unfreeze {
                        Some(u) => clamp_unfreeze(u, now),
                        None => now + kind.cooldown_duration().as_secs() as i64,
                    };
                    e.until = until;
                    e.reason = kind.as_str().to_string();
                    e.err_count = 0;
                    action = Some(format!(
                        "action=cooldown secs={} until={} ({})",
                        until - now,
                        fmt_local_ts(until),
                        if unfreeze.is_some() { "上游解冻时刻" } else { "固定时长" }
                    ));
                    // P2 修复10：不重置 cb_trips——熔断记忆只应在 note_success 重置，
                    // 否则 Server 熔断记忆被非 Server 错误意外清零
                }
                ErrKind::PaidRestricted => {
                    // issue #145：付费受限（模型仅付费账号可用）是账号订阅档位与
                    // 模型的固有不匹配，非瞬时故障——不冷却不禁用不熔断（防止
                    // 免费账号被 403/400 兜底误冷却缩减可用池）。failures 计数
                    // 照加：不触发任何账号级处置，但会参与 weighted_score 成功率
                    // （×3.0 权重）软性拉低评分——有意取舍，保留调度侧「该账号
                    // 近期尝试受限模型」的可观测信号。换号重试仅覆盖 403/Solo
                    // 形态（400 形态经 retry_plan 判 Fatal 直接透传——多账号
                    // 同败时换号无意义，fail-fast）；根治靠单模型开关
                }
                _ => {
                    e.err_count += 1;
                    if e.err_count >= 3 {
                        // 熔断器：30m 起指数递增（×2/次），封顶 6h
                        let exp = 30 * 60u64 << e.cb_trips.min(4);
                        let capped = exp.min(6 * 3600);
                        let until = now + capped as i64;
                        e.until = until;
                        e.reason = "consecutive_errors".to_string();
                        e.err_count = 0;
                        e.cb_trips = e.cb_trips.saturating_add(1);
                        action = Some(format!(
                            "action=circuit_breaker secs={capped} until={}",
                            fmt_local_ts(until)
                        ));
                    }
                }
            }
            if let Some(a) = action {
                cooldown_ev = Some((e.name.clone(), a));
            }
        }
        drop(entries);
        // F-80：log_ctx 未装配（测试/未启动）时静默跳过，不产生日志副作用
        if let Some((name, ev)) = cooldown_ev {
            let ctx = safe_lock(&self.log_ctx).clone();
            if let Some((dir, label)) = ctx {
                crate::fs_utils::app_log(
                    &dir,
                    &format!(
                        "account cooldown: pool={label} acct={name} uid={uid} kind={} {ev}",
                        kind.as_str(),
                    ),
                );
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
        let inflight = self.inflight_snapshot();
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
                hard_credit: e.hard_until > 0 && now < e.hard_until,
                disabled: e.disabled,
                err_count: e.err_count,
                state: e.state_str(now).to_string(),
                inflight: inflight.get(&e.uid).copied().unwrap_or(0),
                refresh_invalid: e.refresh_invalid,
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
        let reserve = self.reserve_credits();
        // healthz/徽章口径恒不豁免（池通用可用性，免费模型豁免是请求级特例）
        entries.values().any(|e| selectable(e, &tried, now, reserve, false))
    }

    /// 同 [has_selectable]，附加 Key 级 uid 白名单过滤（issue #25 资源池绑定）：
    /// 绑定池 Key 的健康预检须感知白名单——池内仅剩白名单外账号时视为该池
    /// 对此 Key 不健康，走全局 fallback 开关回退另一池，而非取号阶段才失败。
    /// `allowed`：uid 白名单（None = 不限；`Some(空集)` = 过滤全部，即该池对此
    /// Key 不健康——issue #30 混合白名单的有意传参，语义同 pick_excluding_constrained）
    pub fn has_selectable_in(
        &self,
        allowed: Option<&HashSet<String>>,
        skip_credit_filters: bool,
    ) -> bool {
        self.selectable_stats_in(allowed, skip_credit_filters).1 > 0
    }

    /// 可选账号统计（诊断用，issue #29）：返回 (池内可选总数, 白名单内可选数)。
    /// `allowed` 语义同 has_selectable_in（None = 不限）；供 NoHealthy 错误
    /// 消息携带「池健康数 / 白名单命中数」，帮助用户自查 Key 约束配置
    pub fn selectable_stats_in(
        &self,
        allowed: Option<&HashSet<String>>,
        skip_credit_filters: bool,
    ) -> (usize, usize) {
        let entries = safe_lock(&self.entries);
        let now = now_ts();
        let tried = HashSet::new();
        let reserve = self.reserve_credits();
        let mut total = 0usize;
        let mut in_allowed = 0usize;
        for e in entries.values() {
            if selectable(e, &tried, now, reserve, skip_credit_filters) {
                total += 1;
                if allowed.map_or(true, |a| a.contains(&e.uid)) {
                    in_allowed += 1;
                }
            }
        }
        (total, in_allowed)
    }

    /// hard_credit（积分耗尽）账号统计（issue #156）：返回 (处于次日 04:00 硬冷却
    /// 的账号数, 最早恢复时刻 Unix 秒)。供 exhaust_message / no_healthy_detail
    /// 携带「积分耗尽·次日 04:00 恢复」语义，使客户端可见冷却原因与恢复时间
    /// （此前该状态只体现在管理端徽标与 app.log，对 API 客户端隐形）。
    /// 无冷却中账号时恢复时刻为 None。
    /// 禁用账号（SessionDead / refresh_token 失效）不计入——其 hard_until 可能是
    /// 禁用前遗留的截止时刻，计入会把「次日 04:00 自动恢复」挂在需重新登录的
    /// 账号上，排障方向被带偏（审查修复）。
    pub fn hard_credit_stats(&self) -> (usize, Option<i64>) {
        let entries = safe_lock(&self.entries);
        let now = now_ts();
        let mut count = 0usize;
        let mut earliest: Option<i64> = None;
        for e in entries.values() {
            if e.hard_until > now && !e.disabled {
                count += 1;
                earliest = Some(match earliest {
                    Some(t) => t.min(e.hard_until),
                    None => e.hard_until,
                });
            }
        }
        (count, earliest)
    }

    /// 诊断：返回所有账号被过滤的原因（用于 "no healthy account" 排查）
    pub fn diagnose(&self) -> Vec<PoolDiagnosis> {
        let entries = safe_lock(&self.entries);
        let now = now_ts();
        let reserve = self.reserve_credits();
        entries
            .values()
            .map(|e| {
                let reason = if e.refresh_invalid {
                    "disabled(refresh_token_invalid)".to_string()
                } else if e.disabled {
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
                            } else if c <= reserve {
                                // 保留积分阈值过滤（issue #91）：陈旧快照
                                // fail-open 不生效（缓存问题），标注供排查
                                if e.reserve_filter_active(now) {
                                    format!("low_credits(reserve={})", reserve)
                                } else {
                                    "healthy(credits_stale_fail_open)".to_string()
                                }
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
    /// 绑定语义标记（审查 P1-4 TOCTOU 收紧）：true = 按既有语义无条件绑定
    /// （专一/粘性锁定、busy_fallback 降级——拒绝会破坏其锁定/兜底语义）；
    /// false = 路由侧绑定时经 CAS 复核「在途 < 并发上限」，已满则换下一候选
    pub over_limit: bool,
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
    /// 最早积分包到期（Unix 秒，Buddy 不分包类型，issue #28）；None = 无到期约束
    pub credits_expire_at: Option<i64>,
    /// 积分快照采集时间（Unix 秒，issue #91 缓存保鲜）：条目级时间戳
    /// （WB credits_fetch 缓存回写）；None = 无时间信息，reserve 过滤不生效
    pub credits_fetched_at: Option<i64>,
    pub needs_relogin: bool,
    /// 所属 Buddy 分组 id（空 = 未分组）；池分组筛选在装配层按此过滤
    pub group_id: String,
}

/// Qoder 账号入池同步结构（p3-3）：uid 即 Cosy user_id（也是 token store 键）。
///
/// `access_token` 可能是会过期的 24h 作业令牌——池内 jwt 仅作快照展示，
/// 请求时的真凭证由网关 identity 回调按需解析（`ApiSharedState.qoder_identity`，
/// PAT 换令牌 / 刷新链路在闭包内走全防护），与 WB「池内即凭证」的模式不同。
pub struct QoderSyncAccount {
    pub uid: String,
    pub name: String,
    pub access_token: String,
    pub machine_id: String,
    pub needs_relogin: bool,
    /// 余额快照（issue #102 三池对齐）：qoder_credits 任务回写 QoderAccount.credits_balance；
    /// None = 无快照/旧数据，reserve 过滤 fail-open
    pub credits: Option<f64>,
    /// 最早积分包到期（Unix 秒，issue #102）：smart 排序键①「先到期先消耗」与
    /// selectable 到期过滤的数据源；None = 无到期信息
    pub credits_expire_at: Option<i64>,
    /// 余额快照采集时间（Unix 秒）：reserve 过滤新鲜度门槛（issue #91 口径）
    pub credits_fetched_at: Option<i64>,
}

/// 候选过滤：healthy + 未 tried + 积分未过期 + 非零积分 + 非 hard_credit 冷却
/// `reserve`：账号保留剩余积分阈值（issue #91，≥ 0；0 = 仅过滤零积分）
/// `skip_credit_filters`：跳过积分维度过滤（issue #111 免费模型豁免）——
/// 倍率 0 的模型零消耗，零积分/保留阈值/积分过期过滤对该类请求整体失义；
/// 仅 Qoder 免费模型取号路径传 true，其余调用点一律 false（既有口径）；
/// healthy/冷却/禁用/专一约束等账号级状态过滤不受影响
fn selectable(
    e: &PoolEntry,
    tried: &HashSet<String>,
    now: i64,
    reserve: f64,
    skip_credit_filters: bool,
) -> bool {
    if tried.contains(&e.uid) || !e.healthy(now) {
        return false;
    }
    if !skip_credit_filters {
        // 跳过积分已过期的（expire_time=0 视为无过期时间，不跳过）
        if let Some(exp) = e.credits_expire_at {
            if exp > 0 && exp < now {
                return false;
            }
        }
        // 跳过零通用积分账号（通用积分耗尽，llm_utils_chat 无法使用）；
        // 账号保留剩余积分（issue #91）：剩余 ≤ 阈值同样不调度——但仅当积分
        // 快照新鲜。陈旧快照 fail-open（缓存问题）：陈旧「高积分」照样会调度
        // 秃号，fail-closed 只会额外误杀；保鲜由启动刷新与 update_credits_snapshot 拨新
        // （reserve=0 时等价原零积分过滤）
        if let Some(c) = e.credits {
            if c <= 0.0 || (c <= reserve && e.reserve_filter_active(now)) {
                return false;
            }
        }
    }
    true
}

/// 账号在途数（F-77，快照查表；无记录 = 0）
fn inflight_of(e: &PoolEntry, inflight: &HashMap<String, u32>) -> u32 {
    inflight.get(&e.uid).copied().unwrap_or(0)
}

/// F-77 busy 过滤（纯函数）：inflight < limit 的账号才参与候选；
/// limit = 0 不限（返回原候选集，未降级）。过滤后为空（全部 busy）时
/// 返回 (原候选集, true)，调用方降级取 inflight 最小者（不过载拒绝）
fn busy_filter<'a>(
    cands: &[&'a PoolEntry],
    inflight: &HashMap<String, u32>,
    limit: u32,
) -> (Vec<&'a PoolEntry>, bool) {
    if limit == 0 {
        return (cands.to_vec(), false);
    }
    let idle: Vec<&PoolEntry> = cands
        .iter()
        .copied()
        .filter(|e| inflight_of(e, inflight) < limit)
        .collect();
    if idle.is_empty() {
        (cands.to_vec(), true)
    } else {
        (idle, false)
    }
}

/// 按策略从候选集中挑选（纯函数，便于单测）；
/// `inflight` 为在途计数快照（F-77 负载因子：Weighted/P2C 偏向空闲账号）
fn pick_by_strategy<'a>(
    cands: &[&'a PoolEntry],
    strategy: PoolStrategy,
    rand_seed: u64,
    now: i64,
    inflight: &HashMap<String, u32>,
) -> Option<&'a PoolEntry> {
    if cands.is_empty() {
        return None;
    }
    match strategy {
        PoolStrategy::Random => Some(cands[(rand_seed as usize) % cands.len()]),
        PoolStrategy::Weighted => pick_weighted(cands, rand_seed, now, inflight),
        PoolStrategy::P2C => pick_p2c(cands, rand_seed, now, inflight),
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
/// 从未使用按满额）+ 成功率×3 → Top5 短名单内按得分二次加权随机；
/// F-77 负载因子：每个在途请求扣 4 分（空闲账号天然优先）
fn pick_weighted<'a>(
    cands: &[&'a PoolEntry],
    rand_seed: u64,
    now: i64,
    inflight: &HashMap<String, u32>,
) -> Option<&'a PoolEntry> {
    let total_credits: f64 = cands.iter().filter_map(|e| e.credits).sum();
    let mut scored: Vec<(&'a PoolEntry, f64)> = cands
        .iter()
        .map(|e| {
            let load_penalty = inflight_of(e, inflight) as f64 * 4.0;
            (*e, (e.weighted_score(total_credits, now) - load_penalty).max(0.1))
        })
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
/// 「优」= 先比在途数（F-77 负载第一比较键，空闲者胜）再比三因子得分；仅一名候选时直接返回
fn pick_p2c<'a>(
    cands: &[&'a PoolEntry],
    rand_seed: u64,
    now: i64,
    inflight: &HashMap<String, u32>,
) -> Option<&'a PoolEntry> {
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
    let ia = inflight_of(a, inflight);
    let ib = inflight_of(b, inflight);
    if ia != ib {
        return Some(if ia < ib { a } else { b });
    }
    let sa = a.weighted_score(total_credits, now);
    let sb = b.weighted_score(total_credits, now);
    Some(if sa >= sb { a } else { b })
}

/// unfreeze 有效性钳制（issue #159，note_error_until 配套）：[now+60s, now+7d]。
/// 下限防上游异常文案（如相对秒极小）让冷却形同虚设；上限与
/// parse_quota_reset_at 的 ≤7 天口径一致，防解析异常把账号钉死过久
fn clamp_unfreeze(unfreeze: i64, now: i64) -> i64 {
    unfreeze.clamp(now + 60, now + 7 * 24 * 3600)
}

/// 次日 04:00 的 Unix 秒。固定按北京时区（UTC+8）取日界——上游积分按北京时间重置，
/// 与运行机器的本地时区无关（注意与代码库其它处 local_ts 的本地时区约定不同）
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

/// Unix 秒 → 固定东八区时间展示（与 next_0400 的 local_ts 约定同源），
/// 供冷却日志 until 可读化；解析失败回退原始数值
fn fmt_local_ts(ts: i64) -> String {
    use chrono::TimeZone;
    chrono::FixedOffset::east_opt(8 * 3600)
        .and_then(|tz| tz.timestamp_opt(ts, 0).single())
        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| ts.to_string())
}

/// 确定性派生 hex 字符串（与原 device_proxy.py 的 _seeded_stream 算法一致，accounts::derive_device 同源）
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
            refresh_token_expires_at: None,
            refresh_token_fails: 0,
            refresh_token_invalid: false,
            auth_saved_at: None,
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
            Some(now_ts()),
            &expires,
            &HashMap::new(),
        );
        pool
    }

    // ==================== hard_credit_stats（issue #156） ====================

    #[test]
    fn hard_credit_stats_counts_and_earliest() {
        // issue #156：hard_credit_stats 统计冷却中账号数与最早恢复时刻。
        // note_error 仅对已存在条目生效——先经 build_pool 注入账号再 note。
        let pool = build_pool(&[("qd-a", 50.0, 0), ("qd-b", 50.0, 0), ("qd-c", 50.0, 0)]);
        let (n0, until0) = pool.hard_credit_stats();
        assert_eq!(n0, 0, "初始无冷却账号");
        assert!(until0.is_none(), "初始无恢复时刻");

        // note_error 仅对已存在条目生效：未注入的 uid 直接被忽略
        pool.note_error("qd-not-exist", ErrKind::HardCredit);
        let (n_none, _) = pool.hard_credit_stats();
        assert_eq!(n_none, 0, "未存在条目的 note_error 不应计入");

        pool.note_error("qd-a", ErrKind::HardCredit);
        let (n1, until1) = pool.hard_credit_stats();
        assert_eq!(n1, 1, "HardCredit 后应计 1 个冷却账号");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let until = until1.expect("应有恢复时刻");
        assert!(until > now, "恢复时刻应在未来（下个 04:00）");

        // 两个账号均冷却时取更早恢复时刻：note_error 生成的 hard_until 同秒内
        // 必然相等（同映射到同一 04:00）。min() 语义用两个受控的绝对未来时刻
        // 验证——不可用 now+3600 之类相对值：03:00–04:00 之间跑测时该值会晚于
        // 04:00 截止，min() 取 04:00 导致断言偶发失败（时刻依赖型测试的坑）。
        pool.note_error("qd-b", ErrKind::HardCredit);
        let later = now_ts() + 7200; // 2 小时后恢复
        let earlier = now_ts() + 3600; // 1 小时后恢复（更早）
        {
            let mut entries = safe_lock(&pool.entries);
            if let Some(e) = entries.get_mut("qd-a") {
                e.hard_until = later;
            }
            if let Some(e) = entries.get_mut("qd-b") {
                e.hard_until = earlier;
            }
        }
        let (n2, until2) = pool.hard_credit_stats();
        assert_eq!(n2, 2, "两个账号冷却后应计 2 个");
        assert_eq!(until2, Some(earlier), "多账号时应取最早恢复时刻（qd-b 的 1 小时后）");

        // 禁用账号不计入：禁用需重新登录，挂「次日 04:00 自动恢复」会误导排障
        {
            let mut entries = safe_lock(&pool.entries);
            if let Some(e) = entries.get_mut("qd-a") {
                e.disabled = true;
            }
        }
        let (n3, until3) = pool.hard_credit_stats();
        assert_eq!(n3, 1, "禁用账号不进入积分耗尽统计");
        assert_eq!(until3, Some(earlier), "剩余最早恢复时刻取未禁用账号");
    }

    // ==================== F-80：冷却/禁用标记落 app.log ====================

    #[test]
    fn cooldown_event_logged_when_ctx_assembled() {
        let dir = std::env::temp_dir().join(format!("aiwa_f80_on_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pool = build_pool(&[("uid_a", 10.0, 0)]);
        pool.set_log_ctx(&dir, "trae");
        pool.note_error("uid_a", ErrKind::Forbidden);
        let line = std::fs::read_to_string(dir.join("logs").join("app.log")).unwrap();
        assert!(
            line.contains(
                "account cooldown: pool=trae acct=uid_a uid=uid_a kind=Forbidden action=disabled"
            ),
            "实际日志: {line}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cooldown_event_not_logged_without_ctx() {
        let dir = std::env::temp_dir().join(format!("aiwa_f80_off_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pool = build_pool(&[("uid_a", 10.0, 0)]);
        // 未装配 log_ctx（测试/未启动网关）：note_error 不产生日志副作用
        pool.note_error("uid_a", ErrKind::Forbidden);
        assert!(
            !dir.join("logs").join("app.log").exists(),
            "未装配 log_ctx 不应产生日志文件"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// issue #145：付费受限（PaidRestricted）是账号订阅档位与模型的固有不匹配，
    /// note_error 必须完全 no-op——不冷却/不禁用/不推进熔断，防止免费账号调度
    /// 付费模型必败被误冷却缩减可用池；failures 计数照加仅作统计
    #[test]
    fn note_error_paid_restricted_is_noop() {
        let pool = build_pool(&[("uid_a", 100.0, 4_000_001_000)]);
        for _ in 0..3 {
            pool.note_error("uid_a", ErrKind::PaidRestricted);
        }
        {
            let entries = safe_lock(&pool.entries);
            let e = entries.get("uid_a").unwrap();
            assert_eq!(e.failures, 3, "failures 照加仅作统计");
            assert_eq!(e.until, 0, "不短冷却");
            assert_eq!(e.hard_until, 0, "不硬冷却");
            assert!(!e.disabled, "不禁用");
            assert_eq!(e.err_count, 0, "不推进熔断计数（连续报错也不触发）");
            assert!(e.reason.is_empty(), "不留处置标记");
        }
        // 可用性不受影响：账号仍可被调度
        assert!(pool.has_selectable());
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
    fn has_selectable_in_respects_whitelist() {
        // issue #25 资源池绑定：健康预检的白名单感知行为
        let pool = build_pool(&[
            ("uid_a", 100.0, 4_000_001_000),
            ("uid_b", 100.0, 4_000_001_000),
        ]);
        // 无白名单：与 has_selectable 等价
        assert!(pool.has_selectable());
        assert!(pool.has_selectable_in(None, false));
        // 白名单命中任一健康账号即可
        let ok: HashSet<String> = ["uid_b".to_string()].into_iter().collect();
        assert!(pool.has_selectable_in(Some(&ok), false));
        // 白名单全在池外 → 视为不健康（dispatch 层据此触发 fallback）
        let miss: HashSet<String> = ["uid_x".to_string()].into_iter().collect();
        assert!(!pool.has_selectable_in(Some(&miss), false));
        // 空白名单 = 过滤所有账号：调用方须先过滤空集（与 pick_excluding_constrained 同语义）
        let empty: HashSet<String> = HashSet::new();
        assert!(!pool.has_selectable_in(Some(&empty), false));
        // 过期账号即使命中白名单也不可选
        let expired = build_pool(&[("uid_a", 100.0, 1_000)]);
        let ok_a: HashSet<String> = ["uid_a".to_string()].into_iter().collect();
        assert!(!expired.has_selectable_in(Some(&ok_a), false));
    }

    #[test]
    fn selectable_stats_in_counts_total_and_whitelisted() {
        // issue #29 修复2：NoHealthy 详情需要池健康计数（总数 + 白名单命中数）
        let pool = build_pool(&[
            ("uid_a", 100.0, 4_000_001_000),
            ("uid_b", 100.0, 4_000_001_000),
            ("uid_c", 0.0, 4_000_001_000), // 零积分 → 不可选
        ]);
        // 无白名单：(可选总数, 同值)
        assert_eq!(pool.selectable_stats_in(None, false), (2, 2));
        // 白名单命中 uid_b：总数不变，命中=1
        let ok: HashSet<String> = ["uid_b".to_string()].into_iter().collect();
        assert_eq!(pool.selectable_stats_in(Some(&ok), false), (2, 1));
        // 白名单全在池外：命中=0
        let miss: HashSet<String> = ["uid_x".to_string()].into_iter().collect();
        assert_eq!(pool.selectable_stats_in(Some(&miss), false), (2, 0));
    }

    #[test]
    fn reserve_credits_skips_low_balance_accounts() {
        // issue #91 账号保留剩余积分：剩余 ≤ 阈值的账号不参与取号与健康预检
        let pool = build_pool(&[
            ("uid_low", 30.0, 4_000_001_000),
            ("uid_ok", 100.0, 4_000_001_000),
        ]);
        // 阈值 0（默认）：零积分过滤语义，两账号均可选
        assert_eq!(pool.selectable_stats_in(None, false), (2, 2));
        pool.set_reserve_credits(50.0);
        // 低余额账号被过滤，仅剩 uid_ok；取号跳过 uid_low
        assert_eq!(pool.selectable_stats_in(None, false), (1, 1));
        let picked = pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap();
        assert_eq!(picked.uid, "uid_ok");
        assert!(pool.pick_by_uid("uid_low", false).is_none());
        // 智能调度数据源同步感知：积分总和不计保留阈值以下账号
        assert_eq!(pool.stats().1, 100.0);
        // 诊断输出 low_credits 原因（NoHealthy 排查）
        let low = pool.diagnose().into_iter().find(|d| d.uid == "uid_low").unwrap();
        assert!(low.reason.starts_with("low_credits"));
        // 阈值归零恢复可选
        pool.set_reserve_credits(0.0);
        assert_eq!(pool.selectable_stats_in(None, false), (2, 2));
    }

    #[test]
    fn reserve_credits_boundary_inclusive() {
        // 语义「低于或等于切换账号」：剩余 == 阈值即过滤
        let pool = build_pool(&[("uid_edge", 50.0, 4_000_001_000)]);
        pool.set_reserve_credits(50.0);
        assert!(!pool.has_selectable());
        // 高于阈值 0.01 即可选
        let pool2 = build_pool(&[("uid_edge", 50.01, 4_000_001_000)]);
        pool2.set_reserve_credits(50.0);
        assert!(pool2.has_selectable());
    }

    // ==================== issue #111：免费模型积分过滤豁免 ====================

    #[test]
    fn free_model_gate_admits_zero_credit() {
        // 零积分账号：标准口径排除；免费模型豁免口径（skip_credit_filters=true）
        // 可选——轮换取号 / 粘性命中两条路径均放行
        let pool = build_pool(&[("uid_zero", 0.0, 4_000_001_000)]);
        assert!(!pool.has_selectable());
        assert!(pool.has_selectable_in(None, true));
        assert!(
            pool.pick_excluding_constrained_ev(&HashSet::new(), None, None, true)
                .is_some()
        );
        assert!(pool.pick_by_uid("uid_zero", true).is_some());
        assert!(pool.pick_by_uid("uid_zero", false).is_none());
    }

    #[test]
    fn free_model_gate_ignores_reserve_threshold() {
        // 账号保留剩余积分（issue #91）对免费模型同样失义：余额 ≤ 阈值仍可选
        let pool = build_pool(&[("uid_low", 5.0, 4_000_001_000)]);
        pool.set_reserve_credits(10.0);
        assert!(!pool.has_selectable());
        assert!(pool.has_selectable_in(None, true));
    }

    #[test]
    fn free_model_gate_ignores_expired_credits() {
        // 积分已过期属积分维度过滤，豁免口径下放行（过期账号常伴随零余额）
        let pool = build_pool(&[("uid_expired", 50.0, 1_000_000_000)]);
        assert!(!pool.has_selectable());
        assert!(pool.has_selectable_in(None, true));
    }

    #[test]
    fn free_model_gate_keeps_account_state_filters() {
        // 账号级状态过滤不豁免：禁用 / hard_credit 冷却的零积分账号仍排除
        let pool = build_pool(&[("uid_dead", 0.0, 4_000_001_000)]);
        pool.note_error("uid_dead", ErrKind::Forbidden);
        assert!(!pool.has_selectable_in(None, true));
        let pool2 = build_pool(&[("uid_cool", 0.0, 4_000_001_000)]);
        pool2.note_error("uid_cool", ErrKind::HardCredit);
        assert!(!pool2.has_selectable_in(None, true));
    }

    #[test]
    fn reserve_credits_stale_snapshot_fail_open() {
        // issue #91 缓存问题：reserve 过滤仅对新鲜积分快照生效。
        // 陈旧快照 fail-open（不误杀），保鲜靠启动刷新与 update_credits_snapshot 拨新
        let pool = build_pool(&[("uid_low", 30.0, 4_000_001_000)]);
        pool.set_reserve_credits(50.0);
        // 新鲜快照：低余额被过滤
        assert_eq!(pool.selectable_stats_in(None, false), (0, 0));
        assert!(pool.has_stale_credits() == false, "build_pool 拨新后不应判陈旧");

        // 快照陈旧（>24h）：fail-open 恢复可选；stats 口径同步（计入 total）
        let now = now_ts();
        {
            let mut entries = safe_lock(&pool.entries);
            for e in entries.values_mut() {
                e.credits_fetched_at = Some(now - RESERVE_STALE_SECS - 60);
            }
        }
        assert!(pool.has_stale_credits());
        assert_eq!(pool.selectable_stats_in(None, false), (1, 1));
        assert!(pool.has_selectable());
        assert_eq!(pool.stats().1, 30.0);
        // 诊断标注陈旧 fail-open（NoHealthy 排查）
        let low = pool.diagnose().into_iter().find(|d| d.uid == "uid_low").unwrap();
        assert!(
            low.reason.contains("credits_stale"),
            "reason={}",
            low.reason
        );

        // 无时间戳（升级前旧数据）同样 fail-open
        {
            let mut entries = safe_lock(&pool.entries);
            for e in entries.values_mut() {
                e.credits_fetched_at = None;
            }
        }
        assert_eq!(pool.selectable_stats_in(None, false), (1, 1));

        // update_credits_snapshot 拨新时间戳（expire 保持原值）：过滤自动恢复
        pool.update_credits_snapshot(
            &[("uid_low".to_string(), (30.0, Some(4_000_001_000)))]
                .into_iter()
                .collect(),
        );
        assert!(!pool.has_stale_credits());
        assert_eq!(pool.selectable_stats_in(None, false), (0, 0));
    }

    /// issue #109 运行期保鲜（Trae 池）：网关启动后新登录/新刷新的账号池内
    /// expire=None，被 expire_first「有过期时间者优先」恒压——调度持续命中带旧
    /// 到期信息的老账号（新会话同样命中）；三键拨新带上下拉新 expire 后选号纠正
    #[test]
    fn credits_snapshot_push_fixes_expire_first_ordering() {
        // uid_a 无到期信息（expire=0 等价 None），uid_b 有到期信息 → 恒选 uid_b
        let pool = build_pool(&[("uid_a", 100.0, 0), ("uid_b", 50.0, 4_000_000_000)]);
        let picked = pool
            .pick_excluding_constrained(&HashSet::new(), None, None)
            .unwrap();
        assert_eq!(picked.uid, "uid_b");
        // 运行期三键拨新：uid_a 带上最早到期时间（早于 uid_b）→ 选号纠正为 uid_a；
        // 未提及的 uid_b 保持原值
        let mut m = HashMap::new();
        m.insert("uid_a".to_string(), (100.0, Some(3_900_000_000)));
        pool.update_credits_snapshot(&m);
        let picked = pool
            .pick_excluding_constrained(&HashSet::new(), None, None)
            .unwrap();
        assert_eq!(picked.uid, "uid_a");
    }

    /// issue #109 充值/续期场景：池内旧 expire 已过期被 selectable 整体排除，
    /// 三键拨新带回新到期时间后恢复可选
    #[test]
    fn credits_snapshot_push_revives_recharged_account() {
        // uid_a 旧到期时间已过 → 不可选
        let pool = build_pool(&[("uid_a", 100.0, 1), ("uid_b", 50.0, 4_000_000_000)]);
        let uid_a: HashSet<String> = ["uid_a".to_string()].into_iter().collect();
        assert!(!pool.has_selectable_in(Some(&uid_a), false));
        assert!(pool.pick_by_uid("uid_a", false).is_none());
        // 运行期拨新（续期后新到期）→ 恢复可选且可被粘性取号
        let mut m = HashMap::new();
        m.insert("uid_a".to_string(), (100.0, Some(4_000_000_000)));
        pool.update_credits_snapshot(&m);
        assert!(pool.has_selectable_in(Some(&uid_a), false));
        assert!(pool.pick_by_uid("uid_a", false).is_some());
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
        let picked = pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap();
        assert_eq!(picked.uid, "uid_b");
        // 排除后取另一个
        let mut tried = HashSet::new();
        tried.insert("uid_b".to_string());
        let picked = pool.pick_excluding_constrained(&tried, None, None).unwrap();
        assert_eq!(picked.uid, "uid_a");
    }

    #[test]
    fn expire_first_prefers_has_expiry_and_more_credits_on_tie() {
        // 有过期时间者优先于无过期时间
        let pool = build_pool(&[("uid_a", 999.0, 0), ("uid_b", 10.0, 4_000_000_000)]);
        pool.set_strategy(PoolStrategy::ExpireFirst);
        assert_eq!(pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap().uid, "uid_b");
        // 过期时间相同：积分多者胜
        let pool = build_pool(&[
            ("uid_a", 50.0, 4_000_000_000),
            ("uid_b", 200.0, 4_000_000_000),
        ]);
        assert_eq!(pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap().uid, "uid_b");
    }

    #[test]
    fn credit_first_prefers_more_credits() {
        let pool = build_pool(&[
            ("uid_a", 50.0, 3_900_000_000),
            ("uid_b", 300.0, 4_000_000_000),
        ]);
        pool.set_strategy(PoolStrategy::CreditFirst);
        let picked = pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap();
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
            let p = pool.pick_excluding_constrained(&tried, None, None).unwrap();
            tried.insert(p.uid);
        }
        assert!(pool.pick_excluding_constrained(&tried, None, None).is_none());
    }

    #[test]
    fn zero_credit_and_expired_are_skipped() {
        // 零积分账号不参与取号
        let pool = build_pool(&[("uid_a", 0.0, 0), ("uid_b", 10.0, 0)]);
        assert_eq!(pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap().uid, "uid_b");
        // 积分已过期账号不参与取号（now 之后才会过期的不受影响）
        let pool = build_pool(&[("uid_a", 10.0, 1), ("uid_b", 10.0, 0)]);
        assert_eq!(pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap().uid, "uid_b");
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
            None,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(pool.count(), 1);
        let picked = pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap();
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
            None,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(pool.count(), 2);
    }

    // ==================== F-78 批次 3 refresh_token 失效联动 ====================

    #[test]
    fn refresh_invalid_disables_on_sync() {
        // refresh_token_invalid=true 的账号同步即禁用，不参与取号
        let pool = ApiPool::new();
        let mut a_invalid = acct("A", "uid_a");
        a_invalid.refresh_token_invalid = true;
        let accounts = vec![a_invalid, acct("B", "uid_b")];
        let enabled: Vec<String> = vec!["uid_a".into(), "uid_b".into()];
        let mut credits = HashMap::new();
        credits.insert("uid_a".to_string(), 100.0);
        credits.insert("uid_b".to_string(), 10.0);
        pool.sync_from_accounts(
            &accounts,
            &enabled,
            &[],
            &HashMap::new(),
            &HashMap::new(),
            &credits,
            Some(now_ts()),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(pool.count(), 2);
        let st: Vec<_> = pool.status_list();
        let sa = st.iter().find(|s| s.uid == "uid_a").unwrap();
        assert!(sa.disabled && sa.refresh_invalid);
        assert_eq!(sa.state, "Forbidden");
        // 取号只落到 uid_b
        assert_eq!(pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap().uid, "uid_b");
        // diagnose 给出 refresh_token_invalid 原因
        let d = pool.diagnose().into_iter().find(|x| x.uid == "uid_a").unwrap();
        assert_eq!(d.reason, "disabled(refresh_token_invalid)");
    }

    #[test]
    fn refresh_invalid_runtime_note_and_recover() {
        // 运行时联动：note_refresh_invalid 禁用 → 恢复路径 = 凭据变更联动热重载
        // reload_pools_if_running → sync_from_accounts 全量重建（commands/api_server.rs），
        // 原单点回填 note_refresh_success 已被其取代并移除
        let pool = build_pool(&[("uid_a", 100.0, 0)]);
        pool.note_refresh_invalid("uid_a");
        let st = &pool.status_list()[0];
        assert!(st.disabled && st.refresh_invalid);
        assert!(pool.pick_excluding_constrained(&HashSet::new(), None, None).is_none());
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
            if pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap().uid == "uid_b" {
                seen_b = true;
                break;
            }
        }
        assert!(seen_b, "weighted 策略应能取到高分账号 uid_b");
        // 闲置补偿：uid_a 刚被用过、uid_b 闲置 3h → uid_b 得分更高
        // 直接对 pick_weighted 纯函数做断言（不依赖随机落点）
        let e_a = crate::api_server::pool::PoolEntry {
            uid: "a".into(), name: String::new(), jwt: String::new(),
            credits: Some(100.0), credits_expire_at: None,
            credits_fetched_at: Some(now_ts()), disabled: false,
            err_count: 0, until: 0, reason: String::new(),
            device_id: String::new(), machine_id: String::new(),
            hard_until: 0, last_used: 1000, successes: 0, failures: 0, cb_trips: 0,
            domain: String::new(), enterprise_id: String::new(), global_region: false,
            refresh_invalid: false,
        };
        let e_b = crate::api_server::pool::PoolEntry {
            last_used: 1000 - 3 * 3600, // 闲置 3 小时
            ..crate::api_server::pool::PoolEntry {
                uid: "b".into(), name: String::new(), jwt: String::new(),
                credits: Some(100.0), credits_expire_at: None,
                credits_fetched_at: Some(now_ts()), disabled: false,
                err_count: 0, until: 0, reason: String::new(),
                device_id: String::new(), machine_id: String::new(),
                hard_until: 0, last_used: 0, successes: 0, failures: 0, cb_trips: 0,
                domain: String::new(), enterprise_id: String::new(), global_region: false,
                refresh_invalid: false,
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
            assert_eq!(pick_p2c(&cands, seed, now, &HashMap::new()).unwrap().uid, "b");
        }
        // 撞号（两索引相同）→ 不 panic，返回任一候选
        for seed in (0..200u64).step_by(2) {
            let picked = pick_p2c(&cands, seed, now, &HashMap::new()).unwrap();
            assert!(picked.uid == "a" || picked.uid == "b");
        }
        // 单候选：直接返回
        let single = vec![&e_b];
        assert_eq!(pick_p2c(&single, 7, now, &HashMap::new()).unwrap().uid, "b");
    }

    /// 测试用 PoolEntry 快速构造
    fn test_entry(uid: &str, credits: f64, last_used: i64) -> PoolEntry {
        PoolEntry {
            uid: uid.to_string(),
            name: String::new(),
            jwt: String::new(),
            credits: Some(credits),
            credits_expire_at: None,
            credits_fetched_at: Some(now_ts()),
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
            refresh_invalid: false,
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
        assert!(pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap().uid != "uid_b");
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

    // ==================== issue #159：note_error_until 上游解冻时刻 ====================

    #[test]
    fn note_error_until_uses_and_clamps_unfreeze() {
        let pool = build_pool(&[("uid_a", 10.0, 0), ("uid_b", 10.0, 0), ("uid_c", 10.0, 0)]);
        let now = now_ts();
        // ① HardCredit：合法解冻时刻（未来 1h，在 [now+60s, now+7d] 内）原样生效
        pool.note_error_until("uid_a", ErrKind::HardCredit, Some(now + 3600));
        // ② HardCredit：低于下限钳到内部 now+60（断言区间容忍测试执行耗时）
        pool.note_error_until("uid_b", ErrKind::HardCredit, Some(now + 30));
        // ③ HardCredit：高于 7 天上限钳到 now+7d
        pool.note_error_until("uid_c", ErrKind::HardCredit, Some(now + 30 * 24 * 3600));
        {
            let entries = safe_lock(&pool.entries);
            assert_eq!(entries.get("uid_a").unwrap().hard_until, now + 3600);
            let b = entries.get("uid_b").unwrap().hard_until;
            assert!((now + 60..=now + 70).contains(&b), "下限钳制 60s: {b}");
            let c = entries.get("uid_c").unwrap().hard_until;
            assert!(
                (now + 7 * 24 * 3600..=now + 7 * 24 * 3600 + 70).contains(&c),
                "上限钳制 7 天: {c}"
            );
        }
        // ④ PlanLimit 同口径：解冻时刻优先于固定 12h 冷却
        pool.note_error_until("uid_a", ErrKind::PlanLimit, Some(now + 7200));
        {
            let entries = safe_lock(&pool.entries);
            assert_eq!(entries.get("uid_a").unwrap().until, now + 7200);
        }
    }

    #[test]
    fn note_error_until_none_falls_back_and_non_freezable_ignores() {
        let pool = build_pool(&[("uid_a", 10.0, 0), ("uid_b", 10.0, 0)]);
        // ① HardCredit 无解冻时刻 → 回退下个 04:00（既有语义不变；断言双端点
        //    容忍测试执行跨 04:00 边界的秒级抖动）
        let now = now_ts();
        pool.note_error_until("uid_a", ErrKind::HardCredit, None);
        {
            let entries = safe_lock(&pool.entries);
            let got = entries.get("uid_a").unwrap().hard_until;
            assert!(
                got == next_0400(now) || got == next_0400(now + 5),
                "应回退下个 04:00: {got}"
            );
        }
        // ② Server 类错误即便携带解冻时刻也不冷却（单次失败不熔断）——
        //    网关/模型级故障与账号解冻时刻无关，解冻值被忽略
        pool.note_error_until("uid_b", ErrKind::Server, Some(now + 3600));
        {
            let entries = safe_lock(&pool.entries);
            let e = entries.get("uid_b").unwrap();
            assert_eq!(e.until, 0, "Server 单次失败不进入冷却");
            assert_eq!(e.hard_until, 0);
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
                credits_expire_at: None,
                credits_fetched_at: None,
                needs_relogin: false,
                group_id: String::new(),
            }],
            &["wb-abc".to_string()],
        );
        pool.set_strategy(PoolStrategy::CreditFirst);
        let picked = pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap();
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
                global_region: false, credits: None, credits_expire_at: None,
                credits_fetched_at: None, needs_relogin: true,
                group_id: String::new(),
            }],
            &["wb-x".to_string()],
        );
        assert!(pool2.pick_excluding_constrained(&HashSet::new(), None, None).is_none());
    }

    #[test]
    fn qoder_sync_and_pick_carries_machine_id() {
        let pool = ApiPool::new();
        pool.sync_from_qoder(
            &[
                crate::api_server::pool::QoderSyncAccount {
                    uid: "qd-1".into(),
                    name: "Qoder账号".into(),
                    access_token: "tk".into(),
                    machine_id: "mid".into(),
                    needs_relogin: false,
                    credits: None,
                    credits_expire_at: None,
                    credits_fetched_at: None,
                },
                crate::api_server::pool::QoderSyncAccount {
                    uid: "qd-dead".into(),
                    name: String::new(),
                    access_token: "tk".into(),
                    machine_id: String::new(),
                    needs_relogin: true,
                    credits: None,
                    credits_expire_at: None,
                    credits_fetched_at: None,
                },
                // 空令牌账号不入池（无法签名）
                crate::api_server::pool::QoderSyncAccount {
                    uid: "qd-empty".into(),
                    name: String::new(),
                    access_token: String::new(),
                    machine_id: String::new(),
                    needs_relogin: false,
                    credits: None,
                    credits_expire_at: None,
                    credits_fetched_at: None,
                },
            ],
            &["qd-1".to_string(), "qd-dead".to_string(), "qd-empty".to_string()],
        );
        assert_eq!(pool.count(), 2, "空令牌账号不入池");
        let picked = pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap();
        assert_eq!(picked.uid, "qd-1");
        assert_eq!(picked.machine_id, "mid");
        // needs_relogin → disabled，不参与取号（Forbidden 态）
        assert!(pool.pick_by_uid("qd-dead", false).is_none());
        assert_eq!(pool.status_list().iter().find(|s| s.uid == "qd-dead").unwrap().state, "Forbidden");
        // 全量重建：再次同步仅含 qd-dead（enabled）→ qd-1 消失
        pool.sync_from_qoder(
            &[crate::api_server::pool::QoderSyncAccount {
                uid: "qd-dead".into(),
                name: String::new(),
                access_token: "tk".into(),
                machine_id: String::new(),
                needs_relogin: true,
                credits: None,
                credits_expire_at: None,
                credits_fetched_at: None,
            }],
            &["qd-dead".to_string()],
        );
        assert_eq!(pool.count(), 1);
        assert!(pool.pick_excluding_constrained(&HashSet::new(), None, None).is_none());
    }

    #[test]
    fn qoder_sync_carries_credits_snapshot() {
        // issue #102：Qoder 余额/到期/快照时间透传入池——smart 排序三键
        //（最早到期 → 倍率 → 积分和）与 reserve 保留额过滤的数据源
        let pool = ApiPool::new();
        pool.sync_from_qoder(
            &[crate::api_server::pool::QoderSyncAccount {
                uid: "qd-c".into(),
                name: "n".into(),
                access_token: "tk".into(),
                machine_id: String::new(),
                needs_relogin: false,
                credits: Some(42.0),
                credits_expire_at: Some(1_999_999_999),
                credits_fetched_at: Some(1_700_000_000),
            }],
            &["qd-c".to_string()],
        );
        let entries = safe_lock(&pool.entries);
        let e = entries.get("qd-c").unwrap();
        assert_eq!(e.credits, Some(42.0));
        assert_eq!(e.credits_expire_at, Some(1_999_999_999));
        assert_eq!(e.credits_fetched_at, Some(1_700_000_000));
    }

    #[test]
    fn qoder_update_credits_snapshot_three_keys_and_runtime_state_kept() {
        // issue #102 运行期拨新：三键同拨；未提及账号不动；运行态（冷却/成功数）不受影响
        let pool = ApiPool::new();
        pool.sync_from_qoder(
            &[
                crate::api_server::pool::QoderSyncAccount {
                    uid: "qd-a".into(), name: String::new(), access_token: "tk".into(),
                    machine_id: String::new(), needs_relogin: false,
                    credits: Some(0.0), credits_expire_at: Some(1), credits_fetched_at: Some(1),
                },
                crate::api_server::pool::QoderSyncAccount {
                    uid: "qd-b".into(), name: String::new(), access_token: "tk".into(),
                    machine_id: String::new(), needs_relogin: false,
                    credits: Some(5.0), credits_expire_at: Some(1_999_999_999),
                    credits_fetched_at: Some(1),
                },
            ],
            &["qd-a".to_string(), "qd-b".to_string()],
        );
        // 运行态预置：qd-a 软冷却中 + 有成功计数，拨新不得触碰
        {
            let mut entries = safe_lock(&pool.entries);
            let a = entries.get_mut("qd-a").unwrap();
            a.until = 1_234_567_890;
            a.successes = 7;
        }
        let mut m: HashMap<String, (f64, Option<i64>)> = HashMap::new();
        m.insert("qd-a".to_string(), (120.0, Some(1_999_999_999)));
        pool.update_credits_snapshot(&m);
        {
            let entries = safe_lock(&pool.entries);
            let a = entries.get("qd-a").unwrap();
            assert_eq!(a.credits, Some(120.0));
            assert_eq!(a.credits_expire_at, Some(1_999_999_999));
            assert!(a.credits_fetched_at.unwrap() > 1, "快照时间拨新");
            assert_eq!(a.until, 1_234_567_890, "软冷却不被触碰");
            assert_eq!(a.successes, 7, "运行态不被触碰");
            // 未提及账号保持原值
            let b = entries.get("qd-b").unwrap();
            assert_eq!(b.credits, Some(5.0));
            assert_eq!(b.credits_expire_at, Some(1_999_999_999));
            assert_eq!(b.credits_fetched_at, Some(1));
        }
    }

    #[test]
    fn qoder_has_zero_or_expired_credits_detects_pinned_snapshot() {
        let pool = ApiPool::new();
        pool.sync_from_qoder(
            &[crate::api_server::pool::QoderSyncAccount {
                uid: "qd-0".into(), name: String::new(), access_token: "tk".into(),
                machine_id: String::new(), needs_relogin: false,
                credits: Some(0.0), credits_expire_at: None, credits_fetched_at: Some(1),
            }],
            &["qd-0".to_string()],
        );
        assert!(pool.has_zero_or_expired_credits(), "零余额快照应触发保鲜");
        // 拨新为正余额 + 未到期 → 不再触发
        let mut m: HashMap<String, (f64, Option<i64>)> = HashMap::new();
        m.insert("qd-0".to_string(), (9.9, Some(1_999_999_999)));
        pool.update_credits_snapshot(&m);
        assert!(!pool.has_zero_or_expired_credits());
        // 已到期快照同样触发（余额为正也被到期过滤排除）
        let mut m2: HashMap<String, (f64, Option<i64>)> = HashMap::new();
        m2.insert("qd-0".to_string(), (9.9, Some(1)));
        pool.update_credits_snapshot(&m2);
        assert!(pool.has_zero_or_expired_credits());
        // disabled（needs_relogin）+ 零余额 → 不触发：刷新注定 401 永久失败，
        // 计入只会让每次启动都跑注定失败的全池刷新
        pool.sync_from_qoder(
            &[crate::api_server::pool::QoderSyncAccount {
                uid: "qd-0".into(), name: String::new(), access_token: "tk".into(),
                machine_id: String::new(), needs_relogin: true,
                credits: Some(0.0), credits_expire_at: Some(1), credits_fetched_at: Some(1),
            }],
            &["qd-0".to_string()],
        );
        assert!(!pool.has_zero_or_expired_credits(), "disabled 账号不参与保鲜判定");
    }

    #[test]
    fn wb_sync_carries_credits_expire_at() {
        // issue #28：Buddy 积分包到期透传入池，供 ExpireFirst/smart 排序消费
        let pool = ApiPool::new();
        pool.sync_from_wb(
            &[crate::api_server::pool::WbSyncAccount {
                uid: "wb-e".into(),
                name: "n".into(),
                token: "tk".into(),
                domain: "d".into(),
                enterprise_id: String::new(),
                global_region: false,
                credits: Some(10.0),
                credits_expire_at: Some(1_234_567_890),
                credits_fetched_at: None,
                needs_relogin: false,
                group_id: String::new(),
            }],
            &["wb-e".to_string()],
        );
        let entries = safe_lock(&pool.entries);
        assert_eq!(entries.get("wb-e").unwrap().credits_expire_at, Some(1_234_567_890));
    }

    // ==================== F-77 账号级并发感知调度 ====================

    #[test]
    fn busy_account_yields_to_idle() {
        // 双账号池 + limit=1：uid_a 在途 1 → 取号必落 uid_b（busy 让位 idle）
        let pool = build_pool(&[("uid_a", 10.0, 0), ("uid_b", 10.0, 0)]);
        pool.set_concurrency_limit(1);
        let h = pool.inflight_handle("uid_a");
        h.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        for _ in 0..8 {
            let picked = pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap();
            assert_eq!(picked.uid, "uid_b", "busy 账号应让位空闲账号");
        }
        // 释放后恢复可选
        h.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        let mut seen_a = false;
        for _ in 0..16 {
            if pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap().uid == "uid_a" {
                seen_a = true;
                break;
            }
        }
        assert!(seen_a, "计数归零后账号应恢复参与调度");
    }

    #[test]
    fn all_busy_falls_back_to_least_inflight() {
        // 全部 busy：不拒绝，取 inflight 最小者（uid_b=1 < uid_a=2）
        let pool = build_pool(&[("uid_a", 10.0, 0), ("uid_b", 10.0, 0)]);
        pool.set_concurrency_limit(1);
        pool.inflight_handle("uid_a").fetch_add(2, std::sync::atomic::Ordering::Relaxed);
        pool.inflight_handle("uid_b").fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        for _ in 0..8 {
            let picked = pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap();
            assert_eq!(picked.uid, "uid_b", "全部 busy 时应取 inflight 最小者");
        }
    }

    #[test]
    fn zero_limit_keeps_original_behavior() {
        // limit=0 = 不限：busy 过滤关闭，在途账号照常参与
        let pool = build_pool(&[("uid_a", 10.0, 0)]);
        pool.set_concurrency_limit(0);
        pool.inflight_handle("uid_a").fetch_add(3, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap().uid, "uid_a");
    }

    #[test]
    fn single_busy_account_still_serves() {
        // 单账号池：无让位对象，busy 也照常取号（粘性/单账号行为兜底）
        let pool = build_pool(&[("uid_a", 10.0, 0)]);
        pool.set_concurrency_limit(1);
        pool.inflight_handle("uid_a").fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap().uid, "uid_a");
    }

    #[test]
    fn pick_by_uid_ignores_busy_filter() {
        // 粘性取号不受 busy 过滤（粘住 = 上游缓存命中，优先级高于让位）
        let pool = build_pool(&[("uid_a", 10.0, 0), ("uid_b", 10.0, 0)]);
        pool.set_concurrency_limit(1);
        pool.inflight_handle("uid_a").fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        assert!(pool.pick_by_uid("uid_a", false).is_some());
    }

    #[test]
    fn p2c_prefers_lower_inflight_first() {
        // P2C 负载第一比较键：在途数不同时空闲者直接胜出（无视三因子得分）
        let e_a = test_entry("a", 900.0, 1000); // 高分但 busy
        let e_b = test_entry("b", 10.0, 1000); // 低分但空闲
        let cands = vec![&e_a, &e_b];
        let mut inflight = HashMap::new();
        inflight.insert("a".to_string(), 2u32);
        inflight.insert("b".to_string(), 0u32);
        for seed in (1..50u64).step_by(2) {
            assert_eq!(pick_p2c(&cands, seed, 2000, &inflight).unwrap().uid, "b");
        }
    }

    #[test]
    fn status_list_reports_inflight() {
        let pool = build_pool(&[("uid_a", 10.0, 0)]);
        assert_eq!(pool.status_list()[0].inflight, 0);
        let h = pool.inflight_handle("uid_a");
        h.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(pool.status_list()[0].inflight, 1);
        h.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(pool.status_list()[0].inflight, 0);
    }

    #[test]
    fn guard_style_inflight_via_handle() {
        // 模拟 InflightGuard 配对：取号句柄 +1/-1，中途丢弃（Drop 兜底语义）计数归零
        let pool = build_pool(&[("uid_a", 10.0, 0)]);
        {
            let _h = pool.inflight_handle("uid_a");
            _h.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            assert_eq!(pool.status_list()[0].inflight, 1);
        }
        // Arc 丢弃后（guard Drop fetch_sub 语义由调用方保证）：
        // 这里直接验证句柄释放 + 计数递减路径可用
        let h = pool.inflight_handle("uid_a");
        h.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(pool.status_list()[0].inflight, 0);
    }

    // ==================== F-77④ 粘性让位 + busy 调度事件 ====================

    #[test]
    fn sticky_yield_when_busy_and_idle_exists() {
        // 粘性账号 busy（inflight≥limit=1）且存在空闲候选 → 让位改选空闲账号
        let pool = build_pool(&[("uid_sticky", 10.0, 0), ("uid_idle", 10.0, 0)]);
        pool.inflight_handle("uid_sticky").fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (p, ev) = pool.pick_sticky_yield("uid_sticky", None, false).unwrap();
        assert_eq!(p.uid, "uid_idle", "busy 粘性账号应让位给空闲账号");
        let ev = ev.unwrap();
        assert!(
            ev.starts_with("sticky_yield from=uid_sticky to=uid_idle"),
            "让位事件应包含 from/to：{ev}"
        );
    }

    #[test]
    fn sticky_kept_when_all_busy() {
        // 全候选 busy → 保持粘性（sticky_fallback：缓存收益 > 换一个同样 busy 的账号）
        let pool = build_pool(&[("uid_sticky", 10.0, 0), ("uid_b", 10.0, 0)]);
        pool.inflight_handle("uid_sticky").fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        pool.inflight_handle("uid_b").fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (p, ev) = pool.pick_sticky_yield("uid_sticky", None, false).unwrap();
        assert_eq!(p.uid, "uid_sticky", "全 busy 时应保持粘性");
        assert!(ev.unwrap().starts_with("sticky_fallback"));
    }

    #[test]
    fn sticky_kept_when_idle() {
        // 粘性账号空闲 → 正常锁定，无调度事件
        let pool = build_pool(&[("uid_sticky", 10.0, 0), ("uid_b", 10.0, 0)]);
        let (p, ev) = pool.pick_sticky_yield("uid_sticky", None, false).unwrap();
        assert_eq!(p.uid, "uid_sticky");
        assert!(ev.is_none());
    }

    #[test]
    fn sticky_yield_respects_allowed() {
        // 让位候选受 allowed 白名单约束：唯一其他候选不在白名单 → 保持粘性
        let pool = build_pool(&[("uid_sticky", 10.0, 0), ("uid_b", 10.0, 0)]);
        pool.inflight_handle("uid_sticky").fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut allowed = HashSet::new();
        allowed.insert("uid_sticky".to_string());
        let (p, ev) = pool.pick_sticky_yield("uid_sticky", Some(&allowed), false).unwrap();
        assert_eq!(p.uid, "uid_sticky", "无白名单内其他候选时保持粘性");
        assert!(ev.is_none());
    }

    #[test]
    fn sticky_yield_no_limit_always_keeps() {
        // limit=0（不限并发）→ 无 busy 概念，始终粘性锁定
        let pool = build_pool(&[("uid_sticky", 10.0, 0), ("uid_b", 10.0, 0)]);
        pool.set_concurrency_limit(0);
        pool.inflight_handle("uid_sticky").fetch_add(5, std::sync::atomic::Ordering::Relaxed);
        let (p, ev) = pool.pick_sticky_yield("uid_sticky", None, false).unwrap();
        assert_eq!(p.uid, "uid_sticky");
        assert!(ev.is_none());
    }

    #[test]
    fn busy_yield_event_on_constrained_pick() {
        // 调度路径：busy 账号被 idle 过滤淘汰 → busy_yield 事件 + 选空闲账号
        let pool = build_pool(&[("uid_a", 10.0, 0), ("uid_b", 10.0, 0)]);
        pool.inflight_handle("uid_a").fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (p, ev) =
            pool.pick_excluding_constrained_ev(&HashSet::new(), None, None, false).unwrap();
        assert_eq!(p.uid, "uid_b");
        assert!(ev.unwrap().starts_with("busy_yield"));
    }

    #[test]
    fn busy_fallback_event_when_all_busy() {
        // 全候选 busy → busy_fallback 降级取 inflight 最小者
        let pool = build_pool(&[("uid_a", 10.0, 0), ("uid_b", 10.0, 0)]);
        pool.inflight_handle("uid_a").fetch_add(2, std::sync::atomic::Ordering::Relaxed);
        pool.inflight_handle("uid_b").fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (p, ev) =
            pool.pick_excluding_constrained_ev(&HashSet::new(), None, None, false).unwrap();
        assert_eq!(p.uid, "uid_b", "全 busy 时应取 inflight 最小者");
        assert!(ev.unwrap().starts_with("busy_fallback"));
    }

    #[test]
    fn no_busy_event_when_limit_zero() {
        // limit=0 → 无 busy 过滤，无事件（兼容原语义）
        let pool = build_pool(&[("uid_a", 10.0, 0), ("uid_b", 10.0, 0)]);
        pool.set_concurrency_limit(0);
        pool.inflight_handle("uid_a").fetch_add(3, std::sync::atomic::Ordering::Relaxed);
        let (_, ev) =
            pool.pick_excluding_constrained_ev(&HashSet::new(), None, None, false).unwrap();
        assert!(ev.is_none());
    }
}
