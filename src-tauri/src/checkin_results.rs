//! 签到结果按日落库：per-uid 记录每日最终签到状态，落盘 `data/checkin_results.json`。
//!
//! 重试轮次自然合并为最终态（同 uid 同日多次记录以最后一次为准）；
//! 保留 90 天自动裁剪；供 Dashboard「签到成功率趋势」堆叠图查询。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fs_utils;

/// 结果数据文件名（位于 data/ 目录）
pub const RESULTS_FILE: &str = "checkin_results.json";
/// 历史数据保留天数（超出部分读写时裁剪）
pub const RETENTION_DAYS: i64 = 90;

/// 单账号某日最终签到状态
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AccountResult {
    pub name: String,
    /// success | already | fail
    pub status: String,
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
    /// 合并记录一日结果：同 uid 以最后一次状态为准（重试轮自然覆盖为最终态）
    pub fn record_day(
        &mut self,
        date: &str,
        entries: impl IntoIterator<Item = (String, String, String)>,
    ) {
        let day = self.days.entry(date.to_string()).or_default();
        for (uid, name, status) in entries {
            day.accounts.insert(
                uid,
                AccountResult {
                    name,
                    status,
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

/// 结果文件路径：data_dir/data/checkin_results.json
pub fn results_path(data_dir: &Path) -> PathBuf {
    let dir = data_dir.join("data");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(RESULTS_FILE)
}

/// 从磁盘加载结果数据（缺失/损坏回退空结构），并裁剪过期日期
pub fn load(data_dir: &Path) -> ResultsFile {
    let mut f: ResultsFile = fs_utils::read_json(&results_path(data_dir));
    f.trim(RETENTION_DAYS);
    f
}

/// 原子写盘
pub fn save(data_dir: &Path, results: &ResultsFile) {
    let _ = fs_utils::write_json(&results_path(data_dir), results);
}

/// 记录当日签到最终状态（签到完成后的 done 落库入口），随后写盘
pub fn record_today(
    data_dir: &Path,
    entries: impl IntoIterator<Item = (String, String, String)>,
) {
    let mut f = load(data_dir);
    f.record_day(&today_key(), entries);
    save(data_dir, &f);
}

/// 趋势查询结果点（单日汇总计数）
#[derive(Serialize, Clone, Debug)]
pub struct TrendPoint {
    pub date: String,
    pub ok: u64,
    pub already: u64,
    pub failed: u64,
}

/// 查询最近 N 天趋势（按日期升序，不足 N 天只返回已有的），供 `checkin_trends` 命令使用
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
            for st in day.accounts.values() {
                match st.status.as_str() {
                    "success" => ok += 1,
                    "already" => already += 1,
                    _ => failed += 1,
                }
            }
            TrendPoint {
                date: date.clone(),
                ok,
                already,
                failed,
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
        // 第 0 轮：u1 成功、u2 失败
        f.record_day(&today, [
            ("u1".into(), "账号1".into(), "success".into()),
            ("u2".into(), "账号2".into(), "fail".into()),
        ]);
        // 重试轮：u2 转为成功（同 uid 以最后一次为准）
        f.record_day(&today, [("u2".into(), "账号2".into(), "success".into())]);
        let day = f.days.get(&today).unwrap();
        assert_eq!(day.accounts.get("u2").unwrap().status, "success");
        let trend = trend_from(&f, 30);
        assert_eq!(trend.len(), 1);
        assert_eq!(trend[0].ok, 2);
        assert_eq!(trend[0].failed, 0);
    }

    #[test]
    fn trim_drops_old_days() {
        let mut f = ResultsFile::default();
        let old = (chrono::Local::now().date_naive() - chrono::Duration::days(120))
            .format("%Y-%m-%d")
            .to_string();
        f.days.insert(old, DayRecord::default());
        f.days.insert(today_key(), DayRecord::default());
        f.trim(RETENTION_DAYS);
        assert_eq!(f.days.len(), 1);
        assert!(f.days.contains_key(&today_key()));
    }

    #[test]
    fn query_recent_按日期升序且限量() {
        let mut f = ResultsFile::default();
        for i in 1..=5 {
            let d = (chrono::Local::now().date_naive() - chrono::Duration::days(i))
                .format("%Y-%m-%d")
                .to_string();
            f.record_day(&d, [("u".into(), "n".into(), "fail".into())]);
        }
        f.record_day(&today_key(), [("u".into(), "n".into(), "success".into())]);
        let got = trend_from(&f, 3);
        assert_eq!(got.len(), 3);
        // 升序：最后一天应为今日
        assert_eq!(got.last().unwrap().date, today_key());
        assert!(got[0].date < got[1].date);
        // 今日 1 成功
        assert_eq!(got.last().unwrap().ok, 1);
    }

    #[test]
    fn results_file_roundtrip() {
        let dir = std::env::temp_dir().join(format!("twa_checkin_results_{}", std::process::id()));
        let mut f = ResultsFile::default();
        f.record_day(&today_key(), [("u1".into(), "账号1".into(), "already".into())]);
        save(&dir, &f);
        let loaded = load(&dir);
        let day = loaded.days.get(&today_key()).expect("应能读回当日数据");
        assert_eq!(day.accounts.get("u1").unwrap().status, "already");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
