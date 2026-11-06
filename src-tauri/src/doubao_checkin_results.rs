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
    pub fn record_day(
        &mut self,
        date: &str,
        entries: impl IntoIterator<Item = (String, String, String, Option<i64>)>,
    ) {
        let day = self.days.entry(date.to_string()).or_default();
        for (uid, name, status, reward) in entries {
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
/// entries 为 (uid, name, status, reward, day) 五元组：day 由 outcome 产生时刻打点
/// （run_one 各 outcome 站点 / 调用方补打；旧 outcome 无 day 字段由调用方回退当日），
/// 跨零点批次按各自日期归组逐日落库，不再统一记「当日」串日。
/// 跨进程互斥：GUI 与 schtasks CLI 双进程并发落库同此丢行问题，命名互斥体串行化。
/// 锁 Busy（他方持锁落库中）行为由 skip_on_busy 决定：
/// - skip_on_busy=true（调度器/CLI 批次落库）：幂等跳过本轮——同批签到的两进程入口
///   并发时，他方完成同批写入，跳过无损失，且避免本方超时后降级整表 load→save
///   覆盖先到方（丢行竞态）；
/// - skip_on_busy=false（代理实时捕获，success 落库）：本轮条目是客户端侧独有
///   reward（调度器同批写入不含这笔），跳过即永久丢失且无重试、「fail 不覆盖
///   success」规则也救不回——降级进程内锁继续写（跨进程并发窗口内可能与持锁方
///   互踩读改写，SQLite 写原子性兜底，尽力不丢优于确定性丢失），落日志留痕。
/// 仅机制性失败（创建/等待出错）一律降级仅进程内锁继续写（尽力不丢，保留降级日志）。
pub fn record_day_at(
    data_dir: &Path,
    entries: impl IntoIterator<Item = (String, String, String, Option<i64>, String)>,
    skip_on_busy: bool,
) {
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
                // 他方正持锁落库：同批签到的另一进程入口并发时跳过无损失（权衡见函数注释）
                crate::fs_utils::app_log(
                    data_dir,
                    "[doubao_checkin_results] record_day_at 跨进程锁 Busy（他方落库中），幂等跳过本轮",
                );
                return;
            }
            // 代理捕获路径：本轮条目是客户端侧独有 reward，跳过即永久丢失——
            // 降级进程内锁继续写（尽力不丢，SQLite 写原子性兜底互踩风险，权衡见函数注释）
            crate::fs_utils::app_log(
                data_dir,
                "[doubao_checkin_results] record_day_at 跨进程锁 Busy，代理捕获路径降级进程内锁继续写（尽力不丢）",
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
    let _cross = cross;
    let mut f = load(data_dir);
    // 按 outcome 携带的 day 归组逐日落库（BTreeMap 保证日期序稳定）
    let mut by_day: std::collections::BTreeMap<String, Vec<(String, String, String, Option<i64>)>> =
        Default::default();
    for (uid, name, status, reward, day) in entries {
        by_day.entry(day).or_default().push((uid, name, status, reward));
    }
    for (day, ents) in by_day {
        f.record_day(&day, ents);
    }
    save(data_dir, &f);
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

/// 查询最近 N 天趋势（按日期升序，不足 N 天只返回已有的）
pub fn query_recent(data_dir: &Path, days: u32) -> Vec<TrendPoint> {
    let f = load(data_dir);
    trend_from(&f, days)
}

/// 从结果结构聚合趋势点（按日期升序、限量最近 N 天）
fn trend_from(f: &ResultsFile, days: u32) -> Vec<TrendPoint> {
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
            ("u1".into(), "账号1".into(), "success".into(), Some(200)),
            ("u2".into(), "账号2".into(), "fail".into(), None),
        ]);
        f.record_day(&today, [("u2".into(), "账号2".into(), "success".into(), Some(200))]);
        let day = f.days.get(&today).unwrap();
        assert_eq!(day.accounts.get("u2").unwrap().status, "success");
        let trend = trend_from(&f, 30);
        assert_eq!(trend.len(), 1);
        assert_eq!(trend[0].ok, 2);
        assert_eq!(trend[0].reward_total, 400);
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
        f.record_day(&today, [("u1".to_string(), "甲".into(), "success".into(), Some(200))]);
        f.record_day(&today, [("u1".to_string(), "甲".into(), "fail".into(), None)]);
        let r = &f.days[&today].accounts["u1"];
        assert_eq!(r.status, "success");
        assert_eq!(r.reward, Some(200));
        // 反向：成功覆盖失败（重试成功为最终态）
        f.record_day(&today, [("u2".to_string(), "乙".into(), "fail".into(), None)]);
        f.record_day(&today, [("u2".to_string(), "乙".into(), "already".into(), None)]);
        assert_eq!(f.days[&today].accounts["u2"].status, "already");
        // fail 覆盖 fail（同态刷新 updated_at 正常）
        f.record_day(&today, [("u3".to_string(), "丙".into(), "fail".into(), None)]);
        f.record_day(&today, [("u3".to_string(), "丙".into(), "fail".into(), None)]);
        assert_eq!(f.days[&today].accounts["u3"].status, "fail");
    }

    #[test]
    fn query_recent_按日期升序且限量() {
        let mut f = ResultsFile::default();
        for i in 1..=5 {
            let d = (chrono::Local::now().date_naive() - chrono::Duration::days(i))
                .format("%Y-%m-%d")
                .to_string();
            f.record_day(&d, [("u".into(), "n".into(), "fail".into(), None)]);
        }
        f.record_day(&today_key(), [("u".into(), "n".into(), "success".into(), Some(200))]);
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
        f.record_day(&today_key(), [("u1".into(), "账号1".into(), "already".into(), None)]);
        save(&dir, &f);
        let loaded = load(&dir);
        let day = loaded.days.get(&today_key()).expect("应能读回当日数据");
        assert_eq!(day.accounts.get("u1").unwrap().status, "already");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
