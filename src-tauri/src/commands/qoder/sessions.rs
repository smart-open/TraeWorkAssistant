//! Qoder 会话明细（issue #82 问题5）：官网无会话消耗明细，查询网关本地流水表
//! `qoder_sessions`（由 `record_usage_qoder` 按请求级写入）。
//!
//! 契约与 Trae `usage_sessions_query` / Buddy `workbuddy_sessions_query` 完全对齐
//! （复用 `UsageSessionView` / `UsageSessionsResult`），前端 SessionsTab 三平台共用：
//! 纯本地零网络、区间按 YYYY-MM-DD 字典序比较、model/user_id 可选过滤、
//! ts 倒序分页、models 清单不受 model 过滤影响。
//!
//! 展示口径（会话合并，两遍归并）：
//! ① `session_id` 非空的行按 (date, uid, session_id) 成桶（新写入路径 seed 由
//!    首条 user 指纹兜底，同会话 session_id 稳定 → 一行一会话）；
//! ② 各桶再按 (date, uid, 桶内最新 preview) 二次归并——兜底覆盖**修复前已落库**
//!    的随机 session_id 行（同轮请求 preview 相同，即使 sid 各异也归并展示，
//!    当天数据即时生效，无需等定时合并）。credits/pt/ct 求和、ts/model/preview
//!    取桶内最新请求，mode 复用为「N 次请求」徽标（Qoder 无 mode 语义）。
//! `session_id` 为空的失败早退行（uid="none"）同走 preview 归并（同说明合并、
//! 合计诚实），不隐藏。积分合计恒一致（行和 = 会话和）。
//! 口径差异：Qoder 侧无官网明细，故 cost_money/缓存 token 恒为空或 0。

use std::collections::{BTreeSet, HashMap};

use tauri::State;

use crate::commands::usage_history::{UsageSessionView, UsageSessionsResult};
use crate::state::AppState;

/// 原始流水行（qoder_sessions 查询列序解包，便于聚合核心独立测试）
pub(crate) type RawSessionRow = (
    String, // request_id
    String, // session_id
    String, // uid
    String, // date
    i64,    // ts
    String, // model
    f64,    // credits
    i64,    // pt
    i64,    // ct
    String, // preview
);

/// 会话桶聚合中间态：sum 三元组累加；代表行按 (ts, request_id) 双键取最新
#[derive(Default)]
struct SessionAgg {
    credits: f64,
    pt: i64,
    ct: i64,
    count: u64,
    last_ts: i64,
    last_request: String,
    sid: String,
    model: String,
    preview: String,
}

impl SessionAgg {
    fn push(&mut self, request_id: &str, ts: i64, sid: &str, model: &str, preview: &str, row: &RawSessionRow) {
        self.credits += row.6;
        self.pt += row.7.max(0);
        self.ct += row.8.max(0);
        self.count += 1;
        if ts > self.last_ts || (ts == self.last_ts && request_id > self.last_request.as_str()) {
            self.last_ts = ts;
            self.last_request = request_id.to_string();
            self.sid = sid.to_string();
            self.model = model.to_string();
            self.preview = preview.to_string();
        }
    }

    fn merge(&mut self, other: SessionAgg) {
        self.credits += other.credits;
        self.pt += other.pt;
        self.ct += other.ct;
        self.count += other.count;
        if other.last_ts > self.last_ts
            || (other.last_ts == self.last_ts && other.last_request > self.last_request)
        {
            self.last_ts = other.last_ts;
            self.last_request = other.last_request;
            self.sid = other.sid;
            self.model = other.model;
            self.preview = other.preview;
        }
    }
}

/// 聚合核心（纯函数，独立于 Tauri State 可单测）：
/// uid 过滤（G1）→ models 收集 → model 过滤 → 两遍归并 → 视图行展开。
/// 归并规则见模块注释「展示口径」。
pub(crate) fn aggregate_session_rows(
    rows: Vec<RawSessionRow>,
    name_of: &HashMap<String, String>,
    uid_filter: Option<&str>,
    model_filter: Option<&str>,
) -> (Vec<(String, UsageSessionView)>, BTreeSet<String>) {
    // 第一遍：session_id 非空按 (date, uid, sid) 成桶；空 sid（失败早退行）直接进 preview 桶
    let mut sid_aggs: HashMap<(String, String, String), SessionAgg> = HashMap::new();
    let mut prev_aggs: HashMap<(String, String, String), SessionAgg> = HashMap::new();
    let mut models: BTreeSet<String> = BTreeSet::new();
    for row in rows {
        let (ref request_id, ref session_id, ref uid, ref date, _ts, ref model, ..) = row;
        // uid 过滤先行（G1）：models 清单不收录被 uid 过滤掉的账号用过的模型——
        // 否则 A 账号可见 B 账号用过的模型名（对齐 Trae 侧口径：uid 过滤 →
        // 收录 → model 过滤）
        if uid_filter.is_some_and(|f| uid != f) {
            continue;
        }
        // models 清单收录不受 model 过滤影响（筛选下拉用）
        models.insert(model.clone());
        if model_filter.is_some_and(|m| model != m) {
            continue;
        }
        let ts = row.4;
        if session_id.is_empty() {
            prev_aggs
                .entry((date.clone(), uid.clone(), row.9.clone()))
                .or_default()
                .push(request_id, ts, "", model, &row.9, &row);
        } else {
            sid_aggs
                .entry((date.clone(), uid.clone(), session_id.clone()))
                .or_default()
                .push(request_id, ts, session_id, model, &row.9, &row);
        }
    }
    // 第二遍：sid 桶按 (date, uid, 桶内最新 preview) 归并进 preview 桶——
    // 覆盖修复前已落库的随机 session_id 行（同轮 preview 相同即归并）
    for ((date, uid, _), agg) in sid_aggs {
        prev_aggs
            .entry((date, uid, agg.preview.clone()))
            .or_default()
            .merge(agg);
    }
    // 展开为视图行（request_id 槽位存桶内最新请求 id，仅作排序 tie-breaker）
    let rows = prev_aggs
        .into_iter()
        .map(|((date, uid, _), a)| {
            (
                a.model.clone(),
                UsageSessionView {
                    account_name: name_of.get(&uid).cloned().unwrap_or_default(),
                    user_id: uid,
                    session_id: a.sid,
                    ts: a.last_ts,
                    date,
                    model: a.model.clone(),
                    mode: if a.count > 1 {
                        format!("{} 次请求", a.count)
                    } else {
                        String::new()
                    },
                    failed: None,
                    credits: a.credits,
                    cost_money: 0.0,
                    input_tokens: a.pt as u64,
                    output_tokens: a.ct as u64,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    preview: a.preview,
                    record_id: String::new(),
                },
            )
        })
        .collect();
    (rows, models)
}

/// 会话明细查询（纯本地流水读时聚合；区间 [start, end] 按本地日字符串比较）。
#[tauri::command(async)]
pub fn qoder_sessions_query(
    state: State<'_, AppState>,
    start: String,
    end: String,
    model: Option<String>,
    user_id: Option<String>,
    page: Option<u32>,
    page_size: Option<u32>,
) -> Result<UsageSessionsResult, String> {
    let page = page.unwrap_or(1).max(1);
    let page_size = page_size.unwrap_or(50).clamp(1, 200);
    // 审查修复（中等项）：start/end trim 后空串不校验会退化 `date >= ''` 命中全表
    let start = start.trim().to_string();
    let end = end.trim().to_string();
    if start.is_empty() || end.is_empty() {
        return Err("start/end 日期区间不能为空".into());
    }
    let model_filter = model.as_deref().map(str::trim).filter(|m| !m.is_empty());
    let uid_filter = user_id.as_deref().map(str::trim).filter(|u| !u.is_empty());

    // 账号展示名以账号池为准（uid → nickname）；拷入闭包所有权（with_conn 要求 'static 闭包）
    let name_of: HashMap<String, String> = load_account_names(&state);

    let raw = crate::store::db(&state.data_dir).with_conn(move |c| {
        let mut stmt = c.prepare(
            "SELECT request_id, session_id, uid, date, ts, model, credits, pt, ct, preview
             FROM qoder_sessions WHERE date >= ?1 AND date <= ?2",
        )?;
        let mut it = stmt.query_map([start.as_str(), end.as_str()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, f64>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, i64>(8)?,
                r.get::<_, String>(9)?,
            ))
        })?;
        let mut rows: Vec<RawSessionRow> = Vec::new();
        for row in it.by_ref() {
            rows.push(row?);
        }
        Ok(aggregate_session_rows(rows, &name_of, uid_filter, model_filter))
    })?;

    let (mut rows, models) = raw;
    // 新会话在前；桶内最新 request_id 唯一，作最终 tie-breaker 保证翻页次序稳定
    rows.sort_by(|a, b| {
        b.1.ts
            .cmp(&a.1.ts)
            .then_with(|| b.1.session_id.cmp(&a.1.session_id))
            .then_with(|| b.0.cmp(&a.0))
    });
    let total = rows.len() as u64;
    let credits_total = rows.iter().map(|r| r.1.credits).sum();
    // 页码夹取 + u64 中间量：越界页回夹到最后一页（原实现越界返回空页，前端
    // 翻页期间数据减少会出现「空白页卡死」）；乘法走 u64 饱和防溢出
    let total_pages = total.div_ceil(page_size as u64).max(1) as u32;
    let page = page.min(total_pages);
    let begin = ((page as u64 - 1).saturating_mul(page_size as u64)) as usize;
    let rows: Vec<UsageSessionView> = rows
        .into_iter()
        .skip(begin)
        .take(page_size as usize)
        .map(|(_, v)| v)
        .collect();

    Ok(UsageSessionsResult {
        // 本地流水为权威态（网关记账即时落库），取当前时刻
        fetched_at: chrono::Local::now().timestamp(),
        total,
        credits_total,
        page,
        page_size,
        rows,
        models: models.into_iter().collect(),
    })
}

/// 账号展示名映射（uid → nickname）：双键匹配（id + uid）——流水 uid 列记录的是
/// 取号账号标识，不同写入路径分别落账号 id（qd- 前缀）与 uid（数字），
/// 仅按单键索引会查不到 → 归属名恒空
fn load_account_names(state: &AppState) -> HashMap<String, String> {
    super::common::load_pool(state)
        .into_iter()
        .filter(|a| !a.uid.is_empty())
        .flat_map(|a| {
            let mut kv = vec![(a.uid.clone(), a.nickname.clone())];
            if !a.id.is_empty() && a.id != a.uid {
                kv.push((a.id.clone(), a.nickname.clone()));
            }
            kv
        })
        .collect()
}

/// 会话展开明细（「展开会话看各轮」）：按会话行代表键查请求级行。
/// 键口径与读时聚合一致：`session_id` 精确匹配（新数据跨轮全捞）∪
/// `preview` 精确匹配（历史随机 sid 行同轮全捞），限定同日同账号防跨会话误捞
/// （不同会话恰好同说明文本时可能多捞，同日同账号范围内可接受）。
/// 已被定时合并的历史区间仅剩聚合行（展开返回单行），诚实口径。
#[tauri::command(async)]
pub fn qoder_session_detail_query(
    state: State<'_, AppState>,
    date: String,
    uid: String,
    session_id: String,
    preview: String,
) -> Result<Vec<UsageSessionView>, String> {
    // 会话行代表键为空串时（失败桶）不按 sid 匹配，仅按 preview 归并
    let sid = session_id.trim().to_string();
    let pv = preview.trim().to_string();
    let name_of = load_account_names(&state);
    crate::store::db(&state.data_dir).with_conn(move |c| {
        // 失败桶（sid 空）不得带 `session_id = ''` 条件——那会把同日同账号
        // **所有**失败行（其他错误说明）都误捞进本会话明细；仅按 preview 匹配
        let sql = if sid.is_empty() {
            "SELECT request_id, session_id, ts, model, credits, pt, ct, preview, record_id
             FROM qoder_sessions
             WHERE date = ?1 AND uid = ?2 AND preview = ?4
             ORDER BY ts ASC
             LIMIT 500"
        } else {
            "SELECT request_id, session_id, ts, model, credits, pt, ct, preview, record_id
             FROM qoder_sessions
             WHERE date = ?1 AND uid = ?2 AND (session_id = ?3 OR preview = ?4)
             ORDER BY ts ASC
             LIMIT 500"
        };
        let mut stmt = c.prepare(sql)?;
        let mut rows: Vec<UsageSessionView> = Vec::new();
        let mut it = stmt.query_map(
            rusqlite::params![date, uid, sid, pv],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, f64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, String>(8)?,
                ))
            },
        )?;
        while let Some(r) = it.next() {
            let (rid, row_sid, ts, model, credits, pt, ct, row_preview, record_id) = r?;
            let _ = rid; // request_id 不外显（幂等键内部用途）
            rows.push(UsageSessionView {
                user_id: uid.clone(),
                account_name: name_of.get(&uid).cloned().unwrap_or_default(),
                session_id: row_sid,
                ts,
                date: date.clone(),
                model,
                mode: String::new(),
                failed: None,
                credits,
                cost_money: 0.0,
                input_tokens: pt.max(0) as u64,
                output_tokens: ct.max(0) as u64,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                preview: row_preview,
                record_id,
            });
        }
        Ok(rows)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(rid: &str, sid: &str, uid: &str, date: &str, ts: i64, model: &str, credits: f64, pt: i64, ct: i64, preview: &str) -> RawSessionRow {
        (rid.into(), sid.into(), uid.into(), date.into(), ts, model.into(), credits, pt, ct, preview.into())
    }

    /// 修复验证核心场景：修复前落库的随机 session_id 行（同轮 preview 相同）
    /// 即使 sid 各异也归并；稳定 sid 跨轮（preview 不同）保持会话级归并；
    /// 空 sid 失败行同 preview 归并；积分合计恒等于行和。
    #[test]
    fn aggregate_merges_by_sid_then_preview() {
        let rows = vec![
            // 修复前旧行：同轮同 preview、随机 sid 各异 → 两遍归并成一行
            row("r1", "aaaa", "u1", "2026-10-12", 100, "gpt", 0.31, 686, 14, "审查代码"),
            row("r2", "bbbb", "u1", "2026-10-12", 200, "gpt", 0.35, 60, 14, "审查代码"),
            row("r3", "cccc", "u1", "2026-10-12", 300, "gpt", 0.14, 54, 26, "审查代码"),
            // 修复后新行：稳定 sid 跨轮（preview 不同）→ sid 桶会话级归并（代表取最新行）
            row("r4", "sess-1", "u1", "2026-10-12", 400, "gpt", 1.0, 10, 5, "第二轮"),
            row("r5", "sess-1", "u1", "2026-10-12", 500, "gpt", 2.0, 20, 8, "第三轮"),
            // 空 sid 失败行：同 preview 归并
            row("r6", "", "none", "2026-10-12", 600, "gpt", 0.0, 0, 0, "超时"),
            row("r7", "", "none", "2026-10-12", 700, "gpt", 0.0, 0, 0, "超时"),
        ];
        let name_of = HashMap::new();
        let (views, models) = aggregate_session_rows(rows, &name_of, None, None);
        assert_eq!(models.len(), 1);
        // 桶：旧行(审查代码) + sess-1 + 失败(超时) = 3 行
        assert_eq!(views.len(), 3, "两遍归并后 3 个会话桶");
        let total: f64 = views.iter().map(|(_, v)| v.credits).sum();
        assert!((total - 3.8).abs() < 1e-9, "积分合计恒等于行和");
        // 旧行桶：3 请求合并，代表取最新 r3（ts=300）
        let old = views.iter().find(|(_, v)| v.preview == "审查代码").unwrap();
        assert_eq!(old.1.ts, 300);
        assert_eq!(old.1.mode, "3 次请求");
        assert!((old.1.credits - 0.8).abs() < 1e-9);
        // 稳定 sid 桶：跨轮 preview 不同仍归并（代表 preview=第三轮 ts=500）
        let sess = views.iter().find(|(_, v)| v.preview == "第三轮").unwrap();
        assert_eq!(sess.1.ts, 500);
        assert!((sess.1.credits - 3.0).abs() < 1e-9);
        // 失败行桶
        let fail = views.iter().find(|(_, v)| v.preview == "超时").unwrap();
        assert_eq!(fail.1.mode, "2 次请求");
    }

    /// uid/model 过滤与 G1 口径：被 uid 过滤账号的模型不进清单；
    /// models 清单不受 model 过滤影响
    #[test]
    fn aggregate_filters_and_models() {
        let rows = vec![
            row("r1", "s1", "u1", "2026-10-12", 100, "gpt", 1.0, 1, 1, "p"),
            row("r2", "s2", "u2", "2026-10-12", 200, "k2", 1.0, 1, 1, "p"),
        ];
        let name_of = HashMap::new();
        let (views, models) = aggregate_session_rows(rows.clone(), &name_of, Some("u1"), None);
        assert_eq!(views.len(), 1);
        assert_eq!(models.len(), 1, "uid 过滤先行：u2 的模型不收录");
        let (views2, models2) = aggregate_session_rows(rows, &name_of, None, Some("gpt"));
        assert_eq!(views2.len(), 1, "model 过滤生效");
        assert_eq!(models2.len(), 2, "models 清单不受 model 过滤影响");
    }
}
