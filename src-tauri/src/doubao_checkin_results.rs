//! 豆包签到结果按日落库：per-uid 记录每日签到状态与奖励额度，
//! 落 SQLite `doubao_checkin_results` 表（与 Trae checkin_results 独立，互不混用）。
//!
//! 重试轮次自然合并为最终态（同 uid 同日多次记录以最后一次为准）；
//! 保留 365 天自动裁剪；供豆包额度看板「签到趋势 / 今日新增额度」查询。

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// 历史数据保留天数（超出部分读写时裁剪；365 天支撑额度看板「近一年」区间）
pub const RETENTION_DAYS: i64 = 365;

/// 单账号某日最终签到状态
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AccountResult {
    pub name: String,
    /// success | already | fail
    pub status: String,
    /// 签到奖励额度（landing/checkin 响应 reward_amount；already/fail 时通常 None）
    #[serde(default)]
    pub reward: Option<i64>,
    /// 失败原因（仅 fail 时携带；旧数据/成功记录为 None——serde default 兼容）。
    /// 2026-11-07 可观测性补齐：此前失败 message 不落库，一键签到页切页后
    /// 逐账号失败原因即丢失，用户只看到笼统「失败」无法排障（风控 710010702
    /// 等需要看上游 message 才能定性）。长度可控（上游 message 透出 + 固定指引）。
    #[serde(default)]
    pub message: Option<String>,
    pub updated_at: String,
}

/// 单日记录：uid → 最终状态
#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct DayRecord {
    #[serde(default)]
    pub accounts: BTreeMap<String, AccountResult>,
}

/// 结果数据根结构：日期 → 单日记录（BTreeMap 保持日期有序）
#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct ResultsFile {
    #[serde(default)]
    pub days: BTreeMap<String, DayRecord>,
}

impl ResultsFile {
    /// 合并记录一日结果：同 uid 以最后一次状态为准（重试轮自然覆盖为最终态）。
    /// 例外（2026-10-31）：fail 不得覆盖当日已有的 success/already——客户端手动签到
    /// 由代理捕获落库（success+reward），随后 CLI 轮次对该账号可能报 fail（风控拒绝
    /// 幂等提交），若照搬「最后写入者胜」会把已到账的 reward 抹掉。
    /// entries 为 (uid, name, status, reward, message) 五元组；message 仅 fail 携带
    /// （失败原因持久化，2026-11-07 可观测性补齐），success/already 传 None 不膨胀落库。
    pub fn record_day(
        &mut self,
        date: &str,
        entries: impl IntoIterator<Item = (String, String, String, Option<i64>, Option<String>)>,
    ) {
        let day = self.days.entry(date.to_string()).or_default();
        for (uid, name, status, reward, message) in entries {
            if status == "fail" {
                if let Some(prev) = day.accounts.get(&uid) {
                    if prev.status == "success" || prev.status == "already" {
                        continue; // 成功是当日最终事实，失败重试不回退
                    }
                }
            }
            day.accounts.insert(
                uid,
                AccountResult {
                    name,
                    status,
                    reward,
                    message,
                    updated_at: crate::fs_utils::now_ts(),
                },
            );
        }
    }

    /// 裁剪保留期之外的历史日期
    pub fn trim(&mut self, keep_days: i64) {
        let cutoff = chrono::Local::now().date_naive() - chrono::Duration::days(keep_days);
        let cutoff_str = cutoff.format("%Y-%m-%d").to_string();
        self.days.retain(|d, _| d.as_str() >= cutoff_str.as_str());
    }
}

/// 当日日期键（本机时区，YYYY-MM-DD）
pub fn today_key() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// 从存储加载结果数据（缺失/损坏回退空结构），并裁剪过期日期。
pub fn load(data_dir: &Path) -> ResultsFile {
    let mut f = crate::store::docs::doubao_checkin_results_load(&crate::store::db(data_dir));
    f.trim(RETENTION_DAYS);
    f
}

/// 原子写库（事务内整表替换；失败落 app_log——按日数据是看板唯一数据源，静默丢失难排查）
pub fn save(data_dir: &Path, results: &ResultsFile) {
    if let Err(e) = crate::store::docs::doubao_checkin_results_save(&crate::store::db(data_dir), results) {
        crate::fs_utils::app_log(
            data_dir,
            &format!("[doubao_checkin_results] save 失败（当日结果可能丢失）: {e}"),
        );
    }
}

/// 当日落库进程内互斥：UI 手动签到、调度器/CLI 落库多路并发时，
/// record_day_at 整表 load→record→save 的后保存方会覆盖先保存方的记录，
/// 持锁串行化保证同进程内不丢行
static RECORD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 记录签到最终状态（签到完成后的 done 落库入口），随后写盘。
/// entries 为 (uid, name, status, reward, message, day) 六元组：day 由 outcome 产生时刻打点
/// （run_one 各 outcome 站点 / 调用方补打；旧 outcome 无 day 字段由调用方回退当日），
/// 跨零点批次按各自日期归组逐日落库，不再统一记「当日」串日。message 仅 fail 携带
/// （失败原因落库，排障可见），success/already 传 None。
/// 跨进程互斥：GUI 与 schtasks CLI 双进程并发落库同此丢行问题，命名互斥体串行化。
/// 锁 Busy（他方持锁落库中）行为由 skip_on_busy 决定：
/// - skip_on_busy=true（调度器/CLI 批次落库）：幂等跳过本轮——同批签到的两进程入口
///   并发时，他方完成同批写入，跳过无损失，且避免本方超时后降级整表 load→save
///   覆盖先到方（丢行竞态）；
/// - skip_on_busy=false（代理实时捕获 / GUI manual 单账号签到，success 落库）：
///   条目可能为本进程侧独有——代理捕获是客户端侧独有 reward（调度器同批写入
///   不含这笔）；manual 单账号与 CLI 批次并发时两批账号不保证相同，跳过会把
///   手动签到结果丢成 fail——跳过即永久丢失且无重试、「fail 不覆盖 success」
///   规则也救不回——降级进程内锁继续写（跨进程并发窗口内可能与持锁方互踩
///   读改写，SQLite 写原子性兜底，尽力不丢优于确定性丢失），落日志留痕。
/// 仅机制性失败（创建/等待出错）一律降级仅进程内锁继续写（尽力不丢，保留降级日志）。
/// 审查 #17：降级路径保存前重读盘上最新视图合并本批条目——降级意味着与持锁方
/// 并发，锁判定后的首次 load 可能早于持锁方的 save（过期视图），直接保存会把
/// 持锁方刚写入的 success+reward 整体回滚；重读后按 record_day 语义（fail 不
/// 覆盖 success/already）合并，尽力保住并发 success（剩余窗口为重读~写盘间的
/// 竞态，SQLite 写原子性兜底，属作者注释明示的已知权衡）。
/// 审查 M-2：降级路径合并范围扩为**全部 uid/日期**（原仅本批触达 uid）——存储层
/// save 为整表 DELETE+INSERT，快照中未触达账号的陈旧行仍会被盲覆盖（如调度器
/// 并发写入的 success 被本进程过期 fail 覆盖）。最终重读按 updated_at 新者胜
/// （fresh 优先级高者胜；本批刚写入行带最新时间戳天然胜出），fresh 独有 uid/日期
/// 行也必须并入——否则整表替换会删除他进程新写入的行。
///
/// 返回是否实际落库（审查 M-3）：false 仅在 skip_on_busy=true 且锁 Busy 幂等跳过
/// 时出现——调用方（run_batch）据此打 skipped_busy 交调度器不 mark_run、下一轮
/// 重跑补录（已签账号幂等归 already），否则批次结果丢失且「当日已跑」被固化。
pub fn record_day_at(
    data_dir: &Path,
    entries: impl IntoIterator<Item = (String, String, String, Option<i64>, Option<String>, String)>,
    skip_on_busy: bool,
) -> bool {
    let _g = RECORD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cross = match crate::tasks::qoder_common::CrossProcLock::try_acquire_ns(
        data_dir,
        // 锁名随函数由 record_today 改为 record_day_at：跨版本升级窗口（新旧二进制
        // 并存，如仅部分机器完成升级）内两代锁不同名互不互斥，属一次性瞬态，
        // 同版分发无影响
        "doubao_checkin",
        "record_day_at",
        3_000,
    ) {
        (Some(g), _) => Some(g),
        (None, Some(crate::tasks::qoder_common::CrossProcLockFail::Busy)) => {
            if skip_on_busy {
                // 他方正持锁落库：同批签到的另一进程入口并发时跳过无损失（权衡见函数注释）。
                // 返回 false 供 run_batch 打 skipped_busy——tick 不 mark_run，下一轮重跑补录
                crate::fs_utils::app_log(
                    data_dir,
                    "[doubao_checkin_results] record_day_at 跨进程锁 Busy（他方落库中），幂等跳过本轮",
                );
                return false;
            }
            // skip_on_busy=false 路径（代理捕获/manual 单账号）：条目可能为本进程侧独有，
            // 跳过即永久丢失——降级进程内锁继续写（尽力不丢，SQLite 写原子性兜底互踩
            // 风险，权衡见函数注释）
            crate::fs_utils::app_log(
                data_dir,
                "[doubao_checkin_results] record_day_at 跨进程锁 Busy，skip_on_busy=false 路径降级进程内锁继续写（尽力不丢）",
            );
            None
        }
        (None, Some(fail)) => {
            // 机制性失败（创建/等待出错）：降级仅进程内锁继续写（尽力不丢）
            crate::fs_utils::app_log(
                data_dir,
                &format!(
                    "[doubao_checkin_results] record_day_at 跨进程锁失败：{}（降级进程内锁继续）",
                    fail.describe()
                ),
            );
            None
        }
        (None, None) => None, // 非 Windows 占位实现不会走到
    };
    // 审查 #17：降级路径（cross=None）与持锁方并发——从锁 Busy 判定到保存之间
    // 持锁方可能已完成 save，首次 load 的快照属过期视图，直接保存会整体回滚其
    // 刚写入的 success+reward。降级时保存前重读盘上最新视图，把本批条目合并到
    // 最新状态上（record_day 的 fail-不覆盖-success 语义保住并发成功）；
    // 持锁成功路径独占窗口内盘上数据即最新，单次 load 即可。
    // 先按 cross 判定再 move 进 `_cross`（借用检查：move 后不得再读）
    let degraded = cross.is_none();
    let mut f = load(data_dir);
    if degraded {
        // 保存前重读：覆盖首次 load 读取窗口内持锁方落盘的写入
        f = load(data_dir);
    }
    let _cross = cross;
    let mut by_day: std::collections::BTreeMap<
        String,
        Vec<(String, String, String, Option<i64>, Option<String>)>,
    > = Default::default();
    for (uid, name, status, reward, message, day) in entries {
        by_day.entry(day).or_default().push((uid, name, status, reward, message));
    }
    for (day, ents) in by_day {
        f.record_day(&day, ents);
    }
    if degraded {
        // 审查 M-2：保存前最终重读合并，范围扩为全部 uid/日期——上一步重读~record~
        // save 之间持锁方/他进程仍可能落盘，且存储层 save 为整表 DELETE+INSERT，
        // 快照（f）中未触达 uid 的行会被盲覆盖、他进程新写入的 uid/日期行会被删除。
        // 剩余竞态窗口为本次重读~save 之间（SQLite 写原子性兜底，作者注释明示的
        // 已知权衡）。胜负规则见 merge_fresh_into
        merge_fresh_into(&mut f, load(data_dir));
    }
    save(data_dir, &f);
    true
}

/// 降级路径最终合并（审查 M-2）：fresh（保存前重读的盘上最新视图）并入快照 f。
/// 胜负规则：一侧 success/already、另一侧 fail 时恒取成功侧——复刻 record_day
/// 「fail 不覆盖 success/already」例外并跨进程双向适用：本批 fail 的 updated_at
/// 是落库时刻 now、几乎必然比持锁方先写的 success 新，纯新者胜会把他进程刚到账的
/// success+reward 抹成 fail；反向同理，更晚落库的他进程 fail 不得回退本批 success。
/// 其余组合（含两行同为 fail）updated_at 新者胜（"YYYY-MM-DD HH:MM:SS" 字典序即
/// 时间序，平局保留快照行，本批刚写入行带最新时间戳天然胜出）；fresh 独有
/// uid/日期行必须并入——否则整表替换会删除他进程新写入的行
fn merge_fresh_into(f: &mut ResultsFile, fresh: ResultsFile) {
    for (day, fr) in fresh.days {
        let rec = f.days.entry(day).or_default();
        for (uid, a) in fr.accounts {
            let fresh_wins = match rec.accounts.get(&uid) {
                // 成功侧恒胜（双向）：快照行为 fail 而 fresh 行已到账 → fresh 胜
                Some(prev) if prev.status == "fail" && a.status != "fail" => true,
                // 快照行已到账而 fresh 行为 fail → 保留快照行
                Some(prev) if prev.status != "fail" && a.status == "fail" => false,
                // 其余组合：updated_at 新者胜，平局保留快照行
                Some(prev) => prev.updated_at < a.updated_at,
                None => true, // fresh 独有行并入，防整表替换丢他进程写入
            };
            if fresh_wins {
                rec.accounts.insert(uid, a);
            }
        }
    }
}

/// 趋势查询结果点（单日汇总计数 + 奖励额度合计）
#[derive(Serialize, Clone, Debug)]
pub struct TrendPoint {
    pub date: String,
    pub ok: u64,
    pub already: u64,
    pub failed: u64,
    /// 当日签到奖励额度合计（success 各账号 reward_amount 之和）
    pub reward_total: i64,
}

/// 从结果结构聚合趋势点（按日期升序、限量最近 N 天）。
/// pub：供 doubao_dashboard_stats 等已持有 ResultsFile 的调用方直接复用，
/// 避免为趋势再 load 一次 365 天全表（2026-11-07 打开卡顿修复：同一张表
/// 此前被全量反序列化两次，原 load+query_recent 包装已移除）
pub fn trend_from(f: &ResultsFile, days: u32) -> Vec<TrendPoint> {
    let mut sorted: Vec<(&String, &DayRecord)> = f.days.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let skip = sorted.len().saturating_sub(days as usize);
    sorted
        .into_iter()
        .skip(skip)
        .map(|(date, day)| {
            let mut ok = 0u64;
            let mut already = 0u64;
            let mut failed = 0u64;
            let mut reward_total = 0i64;
            for st in day.accounts.values() {
                match st.status.as_str() {
                    "success" => {
                        ok += 1;
                        reward_total += st.reward.unwrap_or(0);
                    }
                    "already" => already += 1,
                    _ => failed += 1,
                }
            }
            TrendPoint {
                date: date.clone(),
                ok,
                already,
                failed,
                reward_total,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_重试轮覆盖为最终态() {
        let mut f = ResultsFile::default();
        let today = today_key();
        f.record_day(&today, [
            ("u1".into(), "账号1".into(), "success".into(), Some(200), None),
            ("u2".into(), "账号2".into(), "fail".into(), None, Some("接口返回异常 code=710010702".into())),
        ]);
        f.record_day(&today, [("u2".into(), "账号2".into(), "success".into(), Some(200), None)]);
        let day = f.days.get(&today).unwrap();
        assert_eq!(day.accounts.get("u2").unwrap().status, "success");
        let trend = trend_from(&f, 30);
        assert_eq!(trend.len(), 1);
        assert_eq!(trend[0].ok, 2);
        assert_eq!(trend[0].reward_total, 400);
    }

    /// 失败原因落库 + 旧数据兼容（message 缺省 None，serde default 反序列化不报错）
    #[test]
    fn record_day_fail_原因落库且旧数据兼容() {
        let today = today_key();
        let mut f = ResultsFile::default();
        f.record_day(&today, [(
            "u1".to_string(),
            "甲".into(),
            "fail".into(),
            None,
            Some("触发风控验证码，请在豆包客户端完成一次人工验证后重试".into()),
        )]);
        let r = &f.days[&today].accounts["u1"];
        assert_eq!(r.status, "fail");
        assert!(r.message.as_deref().unwrap().contains("风控"));
        // 旧格式 JSON（无 message 字段）反序列化兼容
        let legacy = serde_json::json!({
            "name": "乙", "status": "success", "reward": 100,
            "updated_at": "2026-11-06 10:00:00"
        });
        let parsed: AccountResult = serde_json::from_value(legacy).expect("旧数据应可反序列化");
        assert!(parsed.message.is_none());
        assert_eq!(parsed.status, "success");
    }

    #[test]
    fn trim_drops_old_days() {
        let mut f = ResultsFile::default();
        let old = (chrono::Local::now().date_naive() - chrono::Duration::days(RETENTION_DAYS + 30))
            .format("%Y-%m-%d")
            .to_string();
        f.days.insert(old, DayRecord::default());
        f.days.insert(today_key(), DayRecord::default());
        f.trim(RETENTION_DAYS);
        assert_eq!(f.days.len(), 1);
        assert!(f.days.contains_key(&today_key()));
    }

    /// fail 不得覆盖当日 success/already（客户端签到落库后 CLI fail 轮不得抹掉 reward）
    #[test]
    fn record_day_fail_不覆盖成功() {
        let today = today_key();
        let mut f = ResultsFile::default();
        f.record_day(&today, [("u1".to_string(), "甲".into(), "success".into(), Some(200), None)]);
        f.record_day(&today, [("u1".to_string(), "甲".into(), "fail".into(), None, None)]);
        let r = &f.days[&today].accounts["u1"];
        assert_eq!(r.status, "success");
        assert_eq!(r.reward, Some(200));
        // 反向：成功覆盖失败（重试成功为最终态）
        f.record_day(&today, [("u2".to_string(), "乙".into(), "fail".into(), None, None)]);
        f.record_day(&today, [("u2".to_string(), "乙".into(), "already".into(), None, None)]);
        assert_eq!(f.days[&today].accounts["u2"].status, "already");
        // fail 覆盖 fail（同态刷新 updated_at 正常）
        f.record_day(&today, [("u3".to_string(), "丙".into(), "fail".into(), None, None)]);
        f.record_day(&today, [("u3".to_string(), "丙".into(), "fail".into(), None, None)]);
        assert_eq!(f.days[&today].accounts["u3"].status, "fail");
    }

    /// 降级路径最终合并（merge_fresh_into）回归：一侧 success/already、另一侧
    /// fail 时恒取成功侧（双向），其余组合 updated_at 新者胜，fresh 独有行并入
    #[test]
    fn merge_fresh_into_成功侧恒胜_纯新者胜不抹账() {
        let today = today_key();
        let mk = |status: &str, reward: Option<i64>, at: &str| AccountResult {
            name: "甲".into(),
            status: status.into(),
            reward,
            message: None,
            updated_at: at.into(),
        };
        // ① 本批 fail（updated_at 更新）不得覆盖持锁方刚写入的 success+reward
        // （record_day 例外跨进程复刻：成功是当日最终事实）
        let mut f = ResultsFile::default();
        f.record_day(&today, [("u1".into(), "甲".into(), "fail".into(), None, None)]);
        let mut fresh = ResultsFile::default();
        fresh.record_day(&today, [("u1".into(), "甲".into(), "success".into(), Some(200), None)]);
        // 时间戳对齐真实时序：持锁方先写（旧）、本批后写（新）
        for r in f.days.get_mut(&today).unwrap().accounts.values_mut() {
            r.updated_at = "2026-11-08 10:00:02".into();
        }
        for r in fresh.days.get_mut(&today).unwrap().accounts.values_mut() {
            r.updated_at = "2026-11-08 10:00:01".into();
        }
        merge_fresh_into(&mut f, fresh);
        let r = &f.days[&today].accounts["u1"];
        assert_eq!(r.status, "success", "本批更晚的 fail 不得抹掉他进程 success");
        assert_eq!(r.reward, Some(200));

        // ② 反向：本批 success（旧）不被更晚落库的他进程 fail 回退
        let mut f = ResultsFile::default();
        f.record_day(&today, [("u2".into(), "乙".into(), "success".into(), Some(300), None)]);
        let mut fresh = ResultsFile::default();
        fresh.record_day(&today, [("u2".into(), "乙".into(), "fail".into(), None, None)]);
        for r in f.days.get_mut(&today).unwrap().accounts.values_mut() {
            r.updated_at = "2026-11-08 10:00:01".into();
        }
        for r in fresh.days.get_mut(&today).unwrap().accounts.values_mut() {
            r.updated_at = "2026-11-08 10:00:02".into();
        }
        merge_fresh_into(&mut f, fresh);
        let r = &f.days[&today].accounts["u2"];
        assert_eq!(r.status, "success", "更晚的 fail 不得回退本批 success");
        assert_eq!(r.reward, Some(300));

        // ③ 同态 success：新者胜（重试轮最终态覆盖）
        let mut f = ResultsFile::default();
        f.days.entry(today.clone()).or_default().accounts.insert(
            "u3".into(),
            mk("already", None, "2026-11-08 10:00:01"),
        );
        let mut fresh = ResultsFile::default();
        fresh.days.entry(today.clone()).or_default().accounts.insert(
            "u3".into(),
            mk("success", Some(100), "2026-11-08 10:00:02"),
        );
        merge_fresh_into(&mut f, fresh);
        assert_eq!(f.days[&today].accounts["u3"].status, "success");

        // ④ fresh 独有 uid/日期行并入（防整表 DELETE+INSERT 丢他进程写入）
        let mut f = ResultsFile::default();
        f.days.entry(today.clone()).or_default().accounts.insert(
            "u4".into(),
            mk("fail", None, "2026-11-08 10:00:02"),
        );
        let mut fresh = ResultsFile::default();
        fresh.days.entry(today.clone()).or_default().accounts.insert(
            "u5".into(),
            mk("success", Some(200), "2026-11-08 10:00:01"),
        );
        fresh.days.entry("2026-11-07".into()).or_default().accounts.insert(
            "u6".into(),
            mk("already", None, "2026-11-07 09:00:00"),
        );
        merge_fresh_into(&mut f, fresh);
        assert!(f.days[&today].accounts.contains_key("u5"), "fresh 独有 uid 并入");
        assert!(f.days["2026-11-07"].accounts.contains_key("u6"), "fresh 独有日期并入");
        assert!(f.days[&today].accounts.contains_key("u4"), "快照既有行保留");
    }

    #[test]
    fn trend_from_按日期升序且限量() {
        let mut f = ResultsFile::default();
        for i in 1..=5 {
            let d = (chrono::Local::now().date_naive() - chrono::Duration::days(i))
                .format("%Y-%m-%d")
                .to_string();
            f.record_day(&d, [("u".into(), "n".into(), "fail".into(), None, None)]);
        }
        f.record_day(&today_key(), [("u".into(), "n".into(), "success".into(), Some(200), None)]);
        let got = trend_from(&f, 3);
        assert_eq!(got.len(), 3);
        assert_eq!(got.last().unwrap().date, today_key());
        assert!(got[0].date < got[1].date);
        assert_eq!(got.last().unwrap().ok, 1);
        assert_eq!(got.last().unwrap().reward_total, 200);
    }

    #[test]
    fn results_file_roundtrip() {
        let dir = std::env::temp_dir().join(format!("twa_doubao_checkin_results_{}", std::process::id()));
        let mut f = ResultsFile::default();
        f.record_day(&today_key(), [
            ("u1".into(), "账号1".into(), "already".into(), None, None),
            ("u2".into(), "账号2".into(), "fail".into(), None, Some("接口返回异常 code=710010702（上游提示：…）".into())),
        ]);
        save(&dir, &f);
        let loaded = load(&dir);
        let day = loaded.days.get(&today_key()).expect("应能读回当日数据");
        assert_eq!(day.accounts.get("u1").unwrap().status, "already");
        // message 列往返（SQLite 列序/读写错误只有运行时暴露，回归必须走 save→load）
        let u2 = day.accounts.get("u2").expect("fail 记录应落库");
        assert_eq!(u2.status, "fail");
        assert_eq!(u2.message.as_deref(), Some("接口返回异常 code=710010702（上游提示：…）"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
