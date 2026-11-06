//! 豆包签到结果按日落库：per-uid 记录每日签到状态与奖励额度，
//! 落 SQLite `doubao_checkin_results` 表（与 Trae checkin_results 独立，互不混用）。
//!
//! 重试轮次按状态优先级合并为最终态（success > already > fail，低优先级不覆盖）；
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
    /// 状态优先级：success(3) > already(2) > fail(1)；未知状态按 0（可被任意覆盖）。
    /// 「重试轮合并为最终态」的正确口径是按优先级而非「最后一次」——
    /// 调度器先签成功（success, reward=N）后，用户手动再点一次命中幂等
    /// （already, reward=None）或瞬时网络失败（fail），「最后一次」会把当日
    /// reward 抹零/状态降级，看板「今日新增额度」随之缩水。
    fn status_rank(status: &str) -> u8 {
        match status {
            "success" => 3,
            "already" => 2,
            "fail" => 1,
            _ => 0,
        }
    }

    /// 合并记录一日结果：同 uid 按状态优先级合并（success > already > fail），
    /// 低优先级不覆盖已有高优先级记录；reward 取新旧中的非 None 值
    ///（success 后续轮的 already/fail 不会抹掉已入账的奖励）。
    pub fn record_day(
        &mut self,
        date: &str,
        entries: impl IntoIterator<Item = (String, String, String, Option<i64>)>,
    ) {
        let day = self.days.entry(date.to_string()).or_default();
        for (uid, name, status, reward) in entries {
            match day.accounts.get_mut(&uid) {
                Some(existing) if Self::status_rank(&existing.status) >= Self::status_rank(&status) => {
                    // 既有记录优先级更高：仅补 reward 空洞（如 success→already 轮次）
                    if existing.reward.is_none() {
                        existing.reward = reward;
                    }
                }
                _ => {
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
        }
    }

    /// 裁剪保留期之外的历史日期
    pub fn trim(&mut self, keep_days: i64) {
        let cutoff_str = self.cutoff_day(keep_days);
        self.days.retain(|d, _| d.as_str() >= cutoff_str.as_str());
    }

    /// 保留期下限日期串（YYYY-MM-DD）：供 trim 与存储层按日裁剪共用
    pub fn cutoff_day(&self, keep_days: i64) -> String {
        let cutoff = chrono::Local::now().date_naive() - chrono::Duration::days(keep_days);
        cutoff.format("%Y-%m-%d").to_string()
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

/// 当日落库进程内互斥：UI 手动签到、调度器/CLI 落库三路并发时，
/// record_day_at 整表 load→record→save 的后保存方会覆盖先保存方的当日记录，
/// 持锁串行化保证同进程内不丢行
static RECORD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 记录各日签到最终状态（签到完成后的 done 落库入口），随后写盘。
/// entries 首项为 day 打点（run_one 在 outcome 产生时刻记下，见 doubao_checkin
/// 的 json! 站点），本函数按 day 分组逐日 record_day——落库跨零点不串日。
/// 跨进程互斥：GUI 与 schtasks CLI 双进程并发落库同此丢行问题，
/// 命名互斥体串行化（对齐 checkin_results 锁语义）。抢锁失败分级处理：
/// Busy（他方正持锁落库）按 skip_on_busy 分级——调度器/CLI 落库传 true（他方
/// 必然完成同批写入，跳过无损失）；**代理捕获路径传 false**：捕获的是客户端侧
/// 独有 reward（调度器同批写入不含这笔），Busy 跳过即永久丢失（无重试），
/// 降级进程内锁继续写（审查 B-H1 修复）；
/// 仅锁机制性失败（创建/等待出错）同样降级进程内锁继续写。
pub fn record_day_at(
    data_dir: &Path,
    entries: impl IntoIterator<Item = (String, String, String, String, Option<i64>)>,
    skip_on_busy: bool,
) {
    let _g = RECORD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _cross = match crate::tasks::qoder_common::CrossProcLock::try_acquire_ns(
        data_dir,
        // 锁名随函数由 record_today 改为 record_day_at：跨版本升级窗口（新旧二进制
        // 并存，如仅部分机器完成升级）内两代锁不同名互不互斥，属一次性瞬态，
        // 同版分发无影响
        "doubao_checkin",
        "record_day_at",
        3_000,
    ) {
        (Some(g), _) => Some(g),
        (None, Some(fail))
            if matches!(
                fail,
                crate::tasks::qoder_common::CrossProcLockFail::Busy
            ) =>
        {
            if skip_on_busy {
                crate::fs_utils::app_log(
                    data_dir,
                    "[doubao_checkin_results] record_day_at 跨进程锁 Busy（他方落库中），本轮落库跳过",
                );
                return;
            }
            // 捕获路径：跳过会丢客户端独有 reward，降级进程内锁继续写
            //（跨进程并发窗口内可能与持锁方互踩读改写，SQLite 写原子性兜底，
            // 尽力不丢优于确定性丢失）
            crate::fs_utils::app_log(
                data_dir,
                "[doubao_checkin_results] record_day_at 跨进程锁 Busy（捕获路径），降级进程内锁继续写",
            );
            None
        }
        (None, Some(fail)) => {
            // 机制性失败（创建/等待出错）：降级仅进程内锁继续写（尽力不丢）
            crate::fs_utils::app_log(
                data_dir,
                &format!(
                    "[doubao_checkin_results] record_day_at 跨进程锁未获取：{}（降级进程内锁继续）",
                    fail.describe()
                ),
            );
            None
        }
        (None, None) => None, // 跨进程锁不可用平台（无锁实现），降级进程内锁
    };
    // 写路径必须用带失败透传的加载：DB 读失败时中止本轮落库（留痕）——
    // 若沿用「失败回空 + 写入」，本次 save 会以残缺视图覆盖当日/历史记录
    let mut f = match crate::store::docs::doubao_checkin_results_try_load(&crate::store::db(data_dir)) {
        Some(f) => f,
        None => {
            crate::fs_utils::app_log(
                data_dir,
                "[doubao_checkin_results] load 失败，本轮签到结果落库中止（避免清库写入）",
            );
            return;
        }
    };
    f.trim(RETENTION_DAYS);
    // 按 day 分组逐日落库（BTreeMap 保日期有序）
    let mut by_day: BTreeMap<String, Vec<(String, String, String, Option<i64>)>> = BTreeMap::new();
    for (day, uid, name, status, reward) in entries {
        by_day.entry(day).or_default().push((uid, name, status, reward));
    }
    for (day, day_entries) in by_day {
        f.record_day(&day, day_entries);
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

    /// 状态优先级合并（审查修复）：success 不被后续 already/fail 轮降级、
    /// reward 不被幂等轮（reward=None）抹零——「最后一次」≠「最终态」
    #[test]
    fn record_按状态优先级合并_低优先级不覆盖() {
        let mut f = ResultsFile::default();
        let today = today_key();
        // 调度器先签成功 → 用户手动再点命中幂等（already, reward=None）→ 瞬时失败
        f.record_day(&today, [("u1".into(), "账号1".into(), "success".into(), Some(200))]);
        f.record_day(&today, [("u1".into(), "账号1".into(), "already".into(), None)]);
        f.record_day(&today, [("u1".into(), "账号1".into(), "fail".into(), None)]);
        let day = f.days.get(&today).unwrap();
        let a = day.accounts.get("u1").unwrap();
        assert_eq!(a.status, "success", "高优先级状态不被降级");
        assert_eq!(a.reward, Some(200), "已入账奖励不被抹零");

        // 反向：先 fail 后 success 仍可升级；success 后 already 仅补 reward 空洞
        f.record_day(&today, [("u2".into(), "账号2".into(), "fail".into(), None)]);
        f.record_day(&today, [("u2".into(), "账号2".into(), "already".into(), None)]);
        let a2 = f.days.get(&today).unwrap().accounts.get("u2").unwrap();
        assert_eq!(a2.status, "already", "低优先级先写入仍可被高优先级升级");
        f.record_day(&today, [("u2".into(), "账号2".into(), "success".into(), Some(50))]);
        let a2 = f.days.get(&today).unwrap().accounts.get("u2").unwrap();
        assert_eq!((a2.status.as_str(), a2.reward), ("success", Some(50)));
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
