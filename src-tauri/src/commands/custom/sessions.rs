//! Custom 池会话明细（issue #149 P2）：查询网关本地流水表 `custom_sessions`
//! （由 `record_usage_custom` 按请求级写入）。
//!
//! 复用 Qoder 侧聚合核心（`aggregate_session_rows` 两遍归并）与契约类型
//! （`UsageSessionView` / `UsageSessionsResult`）。custom 侧差异：
//! - 无积分语义（credits 恒 0，前端 custom 视图不渲染积分列）；
//! - 流水 uid 落真实 key_id（区别于 custom 用量桶固定 "custom"），账号展示名
//!   直接取流水 key_name 列（自研 Key 无独立账号池）；
//! - session_id = conversation_key（前 2 条消息指纹）落库即稳定，无历史随机
//!   sid 行 → 明细展开仅按 session_id 精确匹配（sid 空串防御回退 preview）；
//! - 明细行 mode 位展示请求阶段中文标签（主对话/工具轮），失败行追加「·失败」。

use std::collections::HashMap;

use tauri::State;

use crate::commands::qoder::{aggregate_session_rows, RawSessionRow};
use crate::commands::usage_history::{UsageSessionView, UsageSessionsResult};
use crate::state::AppState;

/// 阶段中文标签（明细行 mode 位展示；口径与网关侧 classify_stage 一致）
fn stage_label(stage: &str) -> &'static str {
    match stage {
        "main_first" => "主对话·首轮",
        "main" => "主对话",
        "tool" => "工具轮",
        _ => "未知阶段",
    }
}

/// 会话明细查询（纯本地流水读时聚合；区间 [start, end] 按本地日字符串比较）。
#[tauri::command(async)]
pub fn custom_sessions_query(
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
    // start/end trim 后空串不校验会退化 `date >= ''` 命中全表（对齐 qoder 口径）
    let start = start.trim().to_string();
    let end = end.trim().to_string();
    if start.is_empty() || end.is_empty() {
        return Err("start/end 日期区间不能为空".into());
    }
    let model_filter = model.as_deref().map(str::trim).filter(|m| !m.is_empty());
    let uid_filter = user_id.as_deref().map(str::trim).filter(|u| !u.is_empty());

    let raw = crate::store::db(&state.data_dir).with_conn(move |c| {
        let mut stmt = c.prepare(
            "SELECT request_id, session_id, uid, date, ts, model, pt, ct, preview, key_name
             FROM custom_sessions WHERE date >= ?1 AND date <= ?2",
        )?;
        let mut it = stmt.query_map([start.as_str(), end.as_str()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, String>(8)?,
                r.get::<_, String>(9)?,
            ))
        })?;
        // 账号展示名映射（uid(key_id) → key_name）：流水行自带，同遍构建
        let mut name_of: HashMap<String, String> = HashMap::new();
        let mut rows: Vec<RawSessionRow> = Vec::new();
        for row in it.by_ref() {
            let (rid, sid, uid, date, ts, model, pt, ct, preview, key_name) = row?;
            name_of
                .entry(uid.clone())
                .or_insert_with(|| key_name.clone());
            // credits 槽位恒 0.0（custom 无积分语义）
            rows.push((rid, sid, uid, date, ts, model, 0.0, pt, ct, preview));
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
    // 页码夹取 + u64 中间量：越界页回夹到最后一页；乘法走 u64 饱和防溢出
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

/// 会话展开明细（「展开会话看各轮」）：session_id = conversation_key 落库即稳定，
/// 无历史随机 sid 行 → 仅按 session_id 精确匹配，限定同日同 Key 防跨会话误捞。
/// sid 空串（理论不出现，防御）不得带 `session_id = ''` 条件——那会把同日同 Key
/// **所有**行都误捞进本会话明细；仅按 preview 匹配。
#[tauri::command(async)]
pub fn custom_session_detail_query(
    state: State<'_, AppState>,
    date: String,
    uid: String,
    session_id: String,
    preview: String,
) -> Result<Vec<UsageSessionView>, String> {
    let sid = session_id.trim().to_string();
    let pv = preview.trim().to_string();
    crate::store::db(&state.data_dir).with_conn(move |c| {
        let sql = if sid.is_empty() {
            "SELECT ts, model, stage, key_name, pt, ct, preview, ok
             FROM custom_sessions
             WHERE date = ?1 AND uid = ?2 AND preview = ?3
             ORDER BY ts ASC
             LIMIT 500"
        } else {
            "SELECT ts, model, stage, key_name, pt, ct, preview, ok
             FROM custom_sessions
             WHERE date = ?1 AND uid = ?2 AND session_id = ?3
             ORDER BY ts ASC
             LIMIT 500"
        };
        let match_key = if sid.is_empty() { pv } else { sid.clone() };
        let mut stmt = c.prepare(sql)?;
        let mut rows: Vec<UsageSessionView> = Vec::new();
        let mut it = stmt.query_map(rusqlite::params![date, uid, match_key], |r| {
            Ok((
                r.get::<_, i64>(0)?,    // ts
                r.get::<_, String>(1)?, // model
                r.get::<_, String>(2)?, // stage
                r.get::<_, String>(3)?, // key_name
                r.get::<_, i64>(4)?,    // pt
                r.get::<_, i64>(5)?,    // ct
                r.get::<_, String>(6)?, // preview
                r.get::<_, i64>(7)?,    // ok
            ))
        })?;
        while let Some(r) = it.next() {
            let (ts, model, stage, key_name, pt, ct, row_preview, ok) = r?;
            let label = stage_label(&stage);
            rows.push(UsageSessionView {
                user_id: uid.clone(),
                account_name: key_name,
                session_id: sid.clone(),
                ts,
                date: date.clone(),
                model,
                mode: if ok != 0 {
                    label.to_string()
                } else {
                    format!("{label}·失败")
                },
                // 失败行结构化标记（3.8.5 审查 L6）：前端徽标据此判定，
                // 不再依赖 mode 字符串 contains("失败") 耦合
                failed: Some(ok == 0),
                credits: 0.0,
                cost_money: 0.0,
                input_tokens: pt.max(0) as u64,
                output_tokens: ct.max(0) as u64,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                preview: row_preview,
                record_id: String::new(),
            });
        }
        Ok(rows)
    })
}
