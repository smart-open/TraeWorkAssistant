//! 自定义模型上游组负载均衡（P2）+ 实例级熔断（P3）
//!
//! 与方案文档对应的核心模块（选路 + 轮换状态，纯函数为主便于测试）：
//! - `pick_upstream`：组内加权轮询选路——跳过熔断中/已试条目；优先活跃条目
//!   （standby=false），活跃候选为空时才启用备胎（failover 主备语义）。
//!   策略收敛为唯一入口，后续 least-inflight / P2C 可插拔。
//! - 会话粘性：同会话粘同上游（保护上游侧 prompt cache），键 = wb_sticky
//!   SessionKey cache_key，值 = cm.id，TTL 60s，内存态不落盘（与池粘性同口径）。
//! - 熔断：完全镜像 `qoder_route.rs::QODER_MODEL_COOLDOWNS` 模式——键从 model
//!   换成 cm.id（同名多上游各自独立熔断），渐进冷却 30s×2ⁿ 封顶 480s，
//!   成功即清，重启清零；401/403/402 落 600s 长窗（配置/账户错误恢复慢）。
//!
//! TOCTOU 说明：选路时未熔断、执行时刚过期的竞态无碍——熔断表是优化而非
//! 正确性保证，多打一次故障上游由组内轮换兜底；不做加锁串行化。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::custom_models::CustomModel;
use super::ApiSharedState;

/// 会话粘上游 TTL（秒）：窗口内单会话不均衡是有意取舍，全局视角仍按权重分摊
pub const UPSTREAM_STICKY_TTL_SECS: u64 = 60;

/// Server 类渐进冷却参数（沿用 Qoder 口径，写死——决策点 1 拍板）
const CIRCUIT_BASE_SECS: u64 = 30;
const CIRCUIT_MAX_SECS: u64 = 480;
/// 401/403/402 长窗（Key 失效 / 欠费 / 额度耗尽——恢复慢，不吃渐进表）
const AUTH_LONG_WINDOW_SECS: u64 = 600;

/// 失败类别（熔断记账 + 轮换判定共用口径）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailKind {
    /// 连接错误 / 连接超时 / 5xx / 429 / 首字超时 / 流内错误（!sent_any）
    Server,
    /// 401/403/402（配置/账户错误）——参与组内轮换，但落 600s 长窗
    Auth,
}

// ==================== 组内选路（P2） ====================

/// 组内选路：加权轮询（组内共享计数器），跳过熔断中的上游（P3 表查询）与
/// 已试条目；优先活跃条目（standby=false），活跃候选为空时才启用备胎。
/// 返回 None = 组内全部熔断/已试尽（交由调用方按错误矩阵处置）。
pub fn pick_upstream(
    group: &[CustomModel],
    tried: &HashSet<String>,
    counter: &AtomicUsize,
) -> Option<CustomModel> {
    let weight_of = |m: &CustomModel| m.weight.max(1) as usize;
    // 候选 = 组内 enabled - tried - 熔断中（find_enabled_all 已按 enabled 过滤，
    // 此处再过滤一次防御调用方直传全量列表）
    let candidates: Vec<&CustomModel> = group
        .iter()
        .filter(|m| m.enabled && !tried.contains(&m.id) && !circuit_open(&m.id))
        .collect();
    // 主备两步：① 活跃非空 → 活跃集轮询；② 活跃为空 → 备胎集轮询；③ 皆空 → None
    let active: Vec<&CustomModel> = candidates.iter().copied().filter(|m| !m.standby).collect();
    let pool: Vec<&CustomModel> = if !active.is_empty() {
        active
    } else {
        candidates.iter().copied().filter(|m| m.standby).collect()
    };
    if pool.is_empty() {
        return None;
    }
    let total: usize = pool.iter().map(|m| weight_of(m)).sum();
    let mut pick = counter.fetch_add(1, Ordering::Relaxed) % total;
    for m in &pool {
        let w = weight_of(m);
        if pick < w {
            return Some((*m).clone());
        }
        pick -= w;
    }
    None
}

/// 从共享状态取组内轮询计数器并选路（canonical 模型名各计各的，避免多组并发
/// 时轮询分布互相干扰）。锁内 fetch_add：持有短临界区，无重入风险。
pub fn pick_with_state(
    state: &ApiSharedState,
    canonical: &str,
    group: &[CustomModel],
    tried: &HashSet<String>,
) -> Option<CustomModel> {
    pick_locked(&state.custom_lb_counters, canonical, group, tried)
}

/// pick_with_state 的 map 直收形态（测试/无状态环境复用）
fn pick_locked(
    counters: &Mutex<HashMap<String, AtomicUsize>>,
    canonical: &str,
    group: &[CustomModel],
    tried: &HashSet<String>,
) -> Option<CustomModel> {
    let mut map = counters.lock().unwrap_or_else(|e| e.into_inner());
    let counter = map.entry(canonical.to_string()).or_default();
    pick_upstream(group, tried, counter)
}

// ==================== 会话粘上游（P2） ====================

/// 会话粘住的上游（命中且仍有效时返回 Some(cm_id)）
pub fn sticky_upstream(state: &ApiSharedState, session_key: &str) -> Option<String> {
    sticky_lookup(&state.custom_upstream_sticky, session_key)
}

/// sticky_upstream 的 map 直收形态（测试/无状态环境复用）
fn sticky_lookup(
    sticky: &Mutex<HashMap<String, (String, Instant)>>,
    session_key: &str,
) -> Option<String> {
    let map = sticky.lock().unwrap_or_else(|e| e.into_inner());
    let (id, until) = map.get(session_key)?;
    (Instant::now() < *until && !circuit_open(id)).then(|| id.clone())
}

/// 记录会话粘上游（最后写入胜出；同请求并发竞争无害）
pub fn record_sticky_upstream(state: &ApiSharedState, session_key: &str, cm_id: &str) {
    record_sticky_locked(&state.custom_upstream_sticky, session_key, cm_id);
}

/// record_sticky_upstream 的 map 直收形态（测试/无状态环境复用）
fn record_sticky_locked(
    sticky: &Mutex<HashMap<String, (String, Instant)>>,
    session_key: &str,
    cm_id: &str,
) {
    let until = Instant::now() + Duration::from_secs(UPSTREAM_STICKY_TTL_SECS);
    let mut map = sticky.lock().unwrap_or_else(|e| e.into_inner());
    map.insert(session_key.to_string(), (cm_id.to_string(), until));
    // 顺手清理过期条目（表规模 = 活跃会话数，残留条目随 TTL 自然过期；此处
    // 仅在写入时增量回收，无独立清理线程）
    let now = Instant::now();
    map.retain(|_, (_, u)| *u > now);
}

// ==================== 实例级熔断（P3） ====================

/// 熔断表：键 = cm.id（同名多上游各自独立熔断，不是 name），
/// 值 = (冷却截止时刻, 连续失败片段数)；成功即清；重启清零。
/// 表不做主动回收，残留条目随冷却窗口自然过期（Qoder 同口径）。
static CUSTOM_UPSTREAM_CIRCUITS: OnceLock<Mutex<HashMap<String, (Instant, u32)>>> =
    OnceLock::new();

fn circuit_map() -> &'static Mutex<HashMap<String, (Instant, u32)>> {
    CUSTOM_UPSTREAM_CIRCUITS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 该上游是否熔断中（未到期 = 熔断中；过期残留条目按未熔断处理）
pub fn circuit_open(cm_id: &str) -> bool {
    circuit_remaining_secs(cm_id).is_some()
}

/// 冷却剩余秒数；None = 未熔断
pub fn circuit_remaining_secs(cm_id: &str) -> Option<u64> {
    let map = circuit_map().lock().unwrap_or_else(|e| e.into_inner());
    let until = map.get(cm_id)?;
    let left = until.0.saturating_duration_since(Instant::now());
    if left.is_zero() {
        None
    } else {
        Some(left.as_secs())
    }
}

/// 熔断记账（按类）：
/// - Server 类：渐进冷却 min(30s × 2ⁿ, 480s)，n = 失败片段数。**递增条件是
///   既有窗口已过期**而非调用次数——一次请求内组内轮换会对同一上游记账多次，
///   按调用次数递增会把「跨片段渐进」放大成「单请求轮换放大」（Qoder 审查
///   修复同款语义：同片段内换号不递增）。半开 = 窗口到期后下一请求自然放行，
///   放行请求失败 → 新片段重新记账（此处 `Instant::now() >= *until` 分支）。
/// - Auth 类（401/403/402）：直接落 600s 长窗，不吃渐进表；参与组内轮换。
pub fn note_circuit_failure(cm_id: &str, kind: FailKind) {
    circuit_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(cm_id.to_string())
        .and_modify(|(until, fails)| match kind {
            FailKind::Auth => {
                // 长窗取较大者：既有渐进窗（理论上 ≤480s）不得被缩短
                let candidate = Instant::now() + Duration::from_secs(AUTH_LONG_WINDOW_SECS);
                if candidate > *until {
                    *until = candidate;
                }
            }
            FailKind::Server => {
                if Instant::now() >= *until {
                    // 既有窗口已过期 = 新一次失败片段 → 递增并重设窗口；
                    // 未过期 = 同片段内的换号/并发失败，窗口维持原截止不后延
                    *fails = fails.saturating_add(1);
                    let delay = (CIRCUIT_BASE_SECS << (*fails).saturating_sub(1).min(4))
                        .min(CIRCUIT_MAX_SECS);
                    *until = Instant::now() + Duration::from_secs(delay);
                } else if *fails == 0 {
                    let cand = Instant::now() + Duration::from_secs(CIRCUIT_BASE_SECS);
                    if cand > *until {
                        *until = cand;
                    }
                }
            }
        })
        .or_insert_with(|| match kind {
            FailKind::Auth => (
                Instant::now() + Duration::from_secs(AUTH_LONG_WINDOW_SECS),
                1,
            ),
            FailKind::Server => (
                Instant::now() + Duration::from_secs(CIRCUIT_BASE_SECS),
                1,
            ),
        });
}

/// 请求成功后清除该上游熔断（连续失败计数随条目一并清除——恢复后渐进冷却
/// 重新从基准值起步）
pub fn clear_circuit(cm_id: &str) {
    circuit_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(cm_id);
}

/// 熔断表快照（池状态接口/可观测用）：(cm.id, 连续失败片段数, 剩余冷却秒)。
/// 仅含仍在冷却中的条目（过期残留不导出）。
pub fn circuit_snapshot() -> Vec<(String, u32, u64)> {
    let map = circuit_map().lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    map.iter()
        .filter_map(|(id, (until, fails))| {
            let left = until.saturating_duration_since(now);
            (!left.is_zero()).then(|| (id.clone(), *fails, left.as_secs()))
        })
        .collect()
}

/// 清空熔断表（运维兜底；生产路径不调用——重启清零同口径。测试隔离用唯一
/// cm.id 键隔离，不用清场方式——清表会干掉并行用例正在记账的条目）
#[allow(dead_code)]
pub(crate) fn clear_all() {
    circuit_map().lock().unwrap_or_else(|e| e.into_inner()).clear();
}

// ==================== 测试 ====================

#[cfg(test)]
mod tests {
    use super::*;

    fn cm(id: &str, weight: u32, standby: bool) -> CustomModel {
        CustomModel {
            id: id.to_string(),
            name: "m".into(),
            base_url: format!("https://{id}.example.com"),
            enabled: true,
            weight,
            standby,
            ..Default::default()
        }
    }

    fn counter() -> AtomicUsize {
        AtomicUsize::new(0)
    }

    /// 用例唯一 cm.id（进程级熔断表跨用例串扰隔离：并行测试以唯一键隔离，
    /// 对齐 qoder_route 测试隔离备忘——不用 clear_all 清场，避免清掉并行
    /// 用例正在记账的条目）
    fn uid(n: usize) -> String {
        use std::sync::atomic::AtomicUsize;
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        format!(
            "cm-t{:02x}-{:012x}",
            n,
            SEQ.fetch_add(1, Ordering::Relaxed) as u64
                ^ std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .subsec_nanos() as u64
        )
    }

    /// 加权轮询分布：weight=3/1 两上游，8 次选路命中比 = 3:1
    #[test]
    fn t01_weighted_round_robin() {
        let (ida, idb) = (uid(1), uid(1));
        let group = vec![cm(&ida, 3, false), cm(&idb, 1, false)];
        let c = counter();
        let mut hits = HashMap::new();
        for _ in 0..8 {
            let picked = pick_upstream(&group, &HashSet::new(), &c).unwrap();
            *hits.entry(picked.id).or_insert(0usize) += 1;
        }
        assert_eq!(hits[&ida], 6);
        assert_eq!(hits[&idb], 2);
    }

    /// 排除重试：tried 含首条 → 选路必返回第二条；全 tried → None
    #[test]
    fn t02_tried_exclusion() {
        let (ida, idb) = (uid(2), uid(2));
        let group = vec![cm(&ida, 1, false), cm(&idb, 1, false)];
        let c = counter();
        let mut tried = HashSet::new();
        tried.insert(ida.clone());
        let picked = pick_upstream(&group, &tried, &c).unwrap();
        assert_eq!(picked.id, idb);
        tried.insert(idb.clone());
        assert!(pick_upstream(&group, &tried, &c).is_none());
    }

    /// 主备：活跃条目存在时备胎永不命中；活跃全熔断 → 备胎接管；活跃恢复 → 回流活跃
    #[test]
    fn t03_standby_failover() {
        let (ida, idb, ids) = (uid(3), uid(3), uid(3));
        let group = vec![cm(&ida, 1, false), cm(&idb, 1, false), cm(&ids, 1, true)];
        let c = counter();
        // 活跃健康：备胎永不命中
        for _ in 0..6 {
            assert_ne!(pick_upstream(&group, &HashSet::new(), &c).unwrap().id, ids);
        }
        // 活跃全熔断 → 备胎接管
        note_circuit_failure(&ida, FailKind::Server);
        note_circuit_failure(&idb, FailKind::Server);
        assert_eq!(
            pick_upstream(&group, &HashSet::new(), &c).unwrap().id,
            ids,
            "活跃全熔断时备胎接管"
        );
        // 活跃恢复 → 回流活跃
        clear_circuit(&ida);
        clear_circuit(&idb);
        assert_ne!(pick_upstream(&group, &HashSet::new(), &c).unwrap().id, ids);
    }

    /// 熔断：连续失败进入冷却、选路跳过；窗口未过期内多次记账不递增（防放大）；
    /// 成功即清
    #[test]
    fn t04_circuit_progressive_and_skip() {
        let (id, idb) = (uid(4), uid(4));
        // 首次失败 → 基准窗 30s
        note_circuit_failure(&id, FailKind::Server);
        let r1 = circuit_remaining_secs(&id).unwrap();
        assert!(r1 > 28 && r1 <= 30, "基准窗 30s，实际 {r1}");
        // 同片段内（窗口未过期）多次记账不递增：窗口不后延、计数不涨
        let until_before = {
            let map = circuit_map().lock().unwrap_or_else(|e| e.into_inner());
            map.get(&id).unwrap().0
        };
        note_circuit_failure(&id, FailKind::Server);
        note_circuit_failure(&id, FailKind::Server);
        {
            let map = circuit_map().lock().unwrap_or_else(|e| e.into_inner());
            let e = map.get(&id).unwrap();
            assert_eq!(e.0, until_before, "窗口未过期不后延（防单请求轮换放大）");
            assert_eq!(e.1, 1, "同片段计数不递增");
        }
        // 冷却中：选路跳过该上游
        let group = vec![cm(&id, 1, false), cm(&idb, 1, false)];
        let c = counter();
        for _ in 0..4 {
            assert_eq!(pick_upstream(&group, &HashSet::new(), &c).unwrap().id, idb);
        }
        // 成功即清
        clear_circuit(&id);
        assert!(!circuit_open(&id), "成功即清");
        assert_eq!(pick_upstream(&group, &HashSet::new(), &c).unwrap().id, id);
    }

    /// Auth 长窗：401/403/402 直接落 600s，不吃渐进表
    #[test]
    fn t05_auth_long_window() {
        let id = uid(5);
        note_circuit_failure(&id, FailKind::Auth);
        let r = circuit_remaining_secs(&id).unwrap();
        assert!(r > 590 && r <= 600, "Auth 长窗 600s，实际 {r}");
        // 后续 Server 类失败不把长窗缩短（取较大者）
        note_circuit_failure(&id, FailKind::Server);
        let r2 = circuit_remaining_secs(&id).unwrap();
        assert!(r2 > 590, "长窗不得被渐进窗缩短");
        clear_circuit(&id);
    }

    /// 窗口过期后的失败 = 新片段（§6.4）：片段 +1、窗口翻倍（30s → 60s）。
    /// Instant 无法快进——直接改表把冷却截止拨到过去（对齐 qoder_route 测试
    /// 直接操作 cooldown_map 的既有做法），验证递增条件 = 既有窗口已过期
    #[test]
    fn t05b_circuit_expiry_increments_fragment() {
        let id = uid(8);
        // 首次失败 → 基准窗（fails=1, 30s）
        note_circuit_failure(&id, FailKind::Server);
        {
            let mut map = circuit_map().lock().unwrap_or_else(|e| e.into_inner());
            // 拨到过去：模拟冷却窗口已过期（半开探测失败的前置状态）
            let e = map.get_mut(&id).unwrap();
            e.0 = Instant::now() - Duration::from_secs(1);
        }
        // 过期后再失败 → 片段 +1，窗口 = 30s << 1 = 60s（翻倍）
        note_circuit_failure(&id, FailKind::Server);
        {
            let map = circuit_map().lock().unwrap_or_else(|e| e.into_inner());
            let e = map.get(&id).unwrap();
            assert_eq!(e.1, 2, "窗口过期后失败 → 片段 +1");
            let left = e.0.saturating_duration_since(Instant::now()).as_secs();
            assert!(left > 55 && left <= 60, "窗口翻倍至 60s，实际 {left}");
        }
        // 再次过期 → 第三片段 120s
        {
            let mut map = circuit_map().lock().unwrap_or_else(|e| e.into_inner());
            let e = map.get_mut(&id).unwrap();
            e.0 = Instant::now() - Duration::from_secs(1);
        }
        note_circuit_failure(&id, FailKind::Server);
        {
            let map = circuit_map().lock().unwrap_or_else(|e| e.into_inner());
            let e = map.get(&id).unwrap();
            assert_eq!(e.1, 3);
            let left = e.0.saturating_duration_since(Instant::now()).as_secs();
            assert!(left > 110 && left <= 120, "第三次片段窗口 120s，实际 {left}");
        }
        clear_circuit(&id);
    }

    /// 粘性：同 session 粘同上游；粘住的上游熔断后失效重选
    #[test]
    fn t06_sticky() {
        let id = uid(6);
        let sticky = Mutex::new(HashMap::new());
        record_sticky_locked(&sticky, "fp:abc", &id);
        assert_eq!(sticky_lookup(&sticky, "fp:abc").unwrap(), id);
        assert!(sticky_lookup(&sticky, "fp:other").is_none());
        // 粘住的上游熔断后失效
        note_circuit_failure(&id, FailKind::Server);
        assert!(sticky_lookup(&sticky, "fp:abc").is_none(), "粘住条目熔断后重选");
        clear_circuit(&id);
        assert!(sticky_lookup(&sticky, "fp:abc").is_some());
    }

    /// 组间隔离：per-model counter 各计各的，A 组轮询不影响 B 组分布
    #[test]
    fn t07_per_model_counter_isolation() {
        let (ida, idb, idc) = (uid(7), uid(7), uid(7));
        let counters = Mutex::new(HashMap::new());
        let ga = vec![cm(&ida, 3, false), cm(&idb, 1, false)];
        let gb = vec![cm(&idc, 1, false)];
        let mut a_hits = HashMap::new();
        let empty = HashSet::new();
        for _ in 0..8 {
            let p = pick_locked(&counters, "model-a", &ga, &empty).unwrap();
            *a_hits.entry(p.id).or_insert(0) += 1;
            // B 组单条模型夹在中间轮询，不得影响 A 组分布
            assert_eq!(pick_locked(&counters, "model-b", &gb, &empty).unwrap().id, idc);
        }
        assert_eq!(a_hits[&ida], 6);
        assert_eq!(a_hits[&idb], 2);
    }
}
