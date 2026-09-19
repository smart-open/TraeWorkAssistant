//! 本地 WorkBuddy / CodeBuddy CLI JSONL Token 统计（F-26/F-57，批次3）。
//! 方案对齐 oss-research/workbuddy-switch token_stats.rs 的解析语义：
//! usage 取值优先级 message.usage > providerData.usage > 顶层 usage；
//! cache_read 别名链优先正值（cache_read_input_tokens → prompt_cache_hit_tokens），
//! 兼容嵌套 prompt_tokens_details / inputTokensDetails；
//! cache_write 仅认显式别名（prompt_cache_miss_tokens 是新增输入，不是写入）。
//!
//! ⚠ serde 命名约定：输出字段全部 snake_case；响应只含聚合数字，不返回消息正文/凭证。

use chrono::TimeZone;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use crate::state::AppState;
use tauri::State;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Usage {
    input: u64,
    output: u64,
    read: u64,
    write: u64,
}

#[derive(Clone, Debug, Default)]
struct Totals {
    usage: Usage,
    calls: u64,
}

impl Totals {
    /// 增量缓存路径：按日聚合（含调用次数）累加
    fn add_day(&mut self, d: &DayTotals) {
        let t = d.to_totals();
        self.usage.input = self.usage.input.saturating_add(t.usage.input);
        self.usage.output = self.usage.output.saturating_add(t.usage.output);
        self.usage.read = self.usage.read.saturating_add(t.usage.read);
        self.usage.write = self.usage.write.saturating_add(t.usage.write);
        self.calls = self.calls.saturating_add(t.calls);
    }

    /// input 已含缓存读取（供应商语义），total 不重复计 read。
    fn to_value(&self) -> Value {
        let cache_hit_rate = (self.usage.input > 0)
            .then(|| self.usage.read as f64 / self.usage.input as f64);
        let total = self
            .usage
            .input
            .saturating_add(self.usage.output)
            .saturating_add(self.usage.write);
        json!({
            "total": total,
            "input": self.usage.input,
            "output": self.usage.output,
            "cache_read": self.usage.read,
            "cache_write": self.usage.write,
            "uncached_input": self.usage.input.saturating_sub(self.usage.read),
            "calls": self.calls,
            "cache_hit_rate": cache_hit_rate,
        })
    }
}

/// 读非负整数（JSON number / 字符串数字）
fn number(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|n| u64::try_from(n).ok()))
        .or_else(|| value.as_f64().filter(|n| n.is_finite() && *n >= 0.0).map(|n| n as u64))
        .or_else(|| value.as_str()?.trim().parse::<u64>().ok())
}

fn field(object: &Map<String, Value>, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| object.get(*key).and_then(number))
}

/// 仅认正值：防止陈旧的 0 值别名掩盖有效的另一别名（如 prompt_cache_hit_tokens）
fn positive_field(object: &Map<String, Value>, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| {
        object
            .get(*key)
            .and_then(number)
            .filter(|value| *value > 0)
    })
}

const CACHE_READ_KEYS: &[&str] = &[
    "cache_read_input_tokens",
    "cacheReadInputTokens",
    "prompt_cache_hit_tokens",
    "cached_tokens",
];

const CACHE_WRITE_KEYS: &[&str] = &[
    "cache_write_input_tokens",
    "cacheWriteInputTokens",
    "cache_creation_input_tokens",
    "prompt_cache_write_tokens",
];

fn cached_input_field(object: &Map<String, Value>) -> u64 {
    positive_field(object, CACHE_READ_KEYS)
        .or_else(|| {
            object
                .get("prompt_tokens_details")
                .and_then(Value::as_object)
                .and_then(|details| positive_field(details, &["cached_tokens"]))
        })
        .or_else(|| {
            object
                .get("inputTokensDetails")
                .and_then(Value::as_array)
                .and_then(|details| {
                    details.iter().find_map(|detail| {
                        detail
                            .as_object()
                            .and_then(|detail| positive_field(detail, &["cached_tokens"]))
                    })
                })
        })
        .unwrap_or(0)
}

fn usage_fields(object: &Map<String, Value>) -> Usage {
    Usage {
        input: field(object, &["input_tokens", "inputTokens", "prompt_tokens"]).unwrap_or(0),
        output: field(object, &["output_tokens", "outputTokens", "completion_tokens"]).unwrap_or(0),
        read: cached_input_field(object),
        write: positive_field(object, CACHE_WRITE_KEYS).unwrap_or(0),
    }
}

/// usage 对象有效性锚点：必须存在 input 字段（可为 0，如 output-only 重试）
fn usage_object(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value?.as_object().filter(|object| {
        field(object, &["input_tokens", "inputTokens", "prompt_tokens"]).is_some()
    })
}

/// 解析单条记录的 usage；cache-write 元数据可能只存在于未选中对象或 rawUsage
fn usage(value: &Value) -> Option<Usage> {
    let provider = value.get("providerData");
    let candidates = [
        value.get("message").and_then(|message| message.get("usage")),
        provider.and_then(|data| data.get("usage")),
        value.get("usage"),
    ];
    let selected = candidates.iter().copied().find_map(usage_object)?;
    let mut result = usage_fields(selected);
    if result.write == 0 {
        result.write = candidates
            .iter()
            .copied()
            .filter_map(|candidate| candidate.and_then(Value::as_object))
            .chain(
                provider
                    .and_then(|data| data.get("rawUsage"))
                    .and_then(Value::as_object),
            )
            .find_map(|object| positive_field(object, CACHE_WRITE_KEYS))
            .unwrap_or(0);
    }
    Some(result)
}

/// 记录时间戳（毫秒）；缺失/无效返回 None（有界窗口内不猜测）
fn record_ts(value: &Value) -> Option<i64> {
    let ts = value
        .get("timestamp")
        .or_else(|| value.get("ts"))
        .and_then(|timestamp| {
            timestamp
                .as_i64()
                .or_else(|| timestamp.as_u64().and_then(|n| i64::try_from(n).ok()))
                .or_else(|| timestamp.as_str()?.trim().parse::<i64>().ok())
        })?;
    // 秒级时间戳归一为毫秒
    Some(if ts < 10_000_000_000 { ts * 1000 } else { ts })
}

fn record_date(value: &Value) -> Option<String> {
    let ts = record_ts(value)?;
    chrono::DateTime::from_timestamp_millis(ts)
        .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
}

fn record_model(value: &Value) -> String {
    value
        .get("providerData")
        .and_then(|data| data.get("model"))
        .or_else(|| value.get("model"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or("未知模型")
        .to_string()
}

/// 递归收集 jsonl（跳过 subagents 子代理目录——重复父会话上下文，不计入用量）
fn collect_jsonl(root: &Path, output: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().and_then(|name| name.to_str()) != Some("subagents") {
                collect_jsonl(&path, output);
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            output.push(path);
        }
    }
}

/// 项目名：行内 cwd 尾段优先，回退 projects 下首级目录名；路径形态目录名脱敏为「未知项目」
fn project_of(value: &Value, fallback: &str) -> String {
    value
        .get("cwd")
        .and_then(Value::as_str)
        .map(Path::new)
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .map(str::trim)
        .filter(|name| !name.is_empty() && name.len() <= 120)
        .unwrap_or(fallback)
        .to_string()
}

fn dir_project_name(root: &Path, file: &Path) -> String {
    let name = file
        .strip_prefix(root)
        .ok()
        .and_then(|relative| relative.components().next())
        .and_then(|component| component.as_os_str().to_str())
        .filter(|name| !name.is_empty() && !name.ends_with(".jsonl"));
    match name {
        Some(name) if !name.starts_with("Users-") && !name.starts_with("home-") => {
            name.to_string()
        }
        _ => "未知项目".to_string(),
    }
}

fn group_vec(groups: HashMap<String, Totals>) -> Vec<Value> {
    let mut values: Vec<(String, Value)> = groups
        .into_iter()
        .map(|(key, totals)| {
            let mut v = totals.to_value();
            v["key"] = json!(key);
            (key, v)
        })
        .collect();
    values.sort_by(|a, b| {
        let ta = a.1.get("total").and_then(Value::as_u64).unwrap_or(0);
        let tb = b.1.get("total").and_then(Value::as_u64).unwrap_or(0);
        tb.cmp(&ta).then_with(|| a.0.cmp(&b.0))
    });
    values.into_iter().map(|(_, v)| v).collect()
}

// ── 增量缓存层（F-59 性能优化：积分看板 Token 统计提速）────────────────────
//
// 三级优化（对应用户诉求「先优化获取逻辑 → 10min 缓存 → 过滤再获取」）：
// ① 获取逻辑：按文件增量缓存——mtime+size 未变的文件直接复用按日聚合，不重解析。
//    全量重扫的瓶颈是逐行 serde 解析（会话文件多且大），增量后仅解析新增/变更文件。
// ② 结果缓存：进程内 10 分钟 TTL（RESULT_CACHE），前端挂载默认走缓存，
//    「重扫」按钮传 fresh=true 强制刷新。
// ③ 过滤再获取：365 天窗口在合并阶段按日期过滤（缓存条目存全量日期，
//    窗口滑动无需失效缓存），cutoff 之外的日期零解析零聚合。
//
// 缓存粒度说明：会话 JSONL 每条记录必带时间戳（缺时间戳记录本就不计入统计），
// 故按「日期 (+模型/项目)」缓存聚合是无损的；个别 ts 合法但日期转换失败的极端
// 记录会从全口径中一并排除（原实现仅 daily 口径排除，差异可忽略）。

/// 结果级缓存 TTL（秒）
const RESULT_TTL_SECS: u64 = 600;
/// 统计窗口（天），与原实现一致
const WINDOW_DAYS: i64 = 365;

static RESULT_CACHE: Mutex<Option<(Instant, Value)>> = Mutex::new(None);

/// 按日聚合（可序列化缓存单元）
#[derive(Serialize, Deserialize, Clone, Copy, Default)]
struct DayTotals {
    input: u64,
    output: u64,
    read: u64,
    write: u64,
    calls: u64,
}

impl DayTotals {
    fn add(&mut self, other: &Self) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.read = self.read.saturating_add(other.read);
        self.write = self.write.saturating_add(other.write);
        self.calls = self.calls.saturating_add(other.calls);
    }

    fn to_totals(self) -> Totals {
        Totals {
            usage: Usage {
                input: self.input,
                output: self.output,
                read: self.read,
                write: self.write,
            },
            calls: self.calls,
        }
    }
}

/// 解析器行为版本：解析逻辑变更（如兼容性容错修复）时 +1。
/// 增量缓存条目 rev 不匹配时强制重解析，避免旧版本误计数的 parse_errors 滞留展示。
const PARSE_REV: u32 = 2;

/// 单文件增量缓存条目
#[derive(Serialize, Deserialize, Clone, Default)]
struct FileCacheEntry {
    /// 解析器行为版本（PARSE_REV），不匹配则重解析
    #[serde(default)]
    rev: u32,
    mtime_ms: i64,
    size: u64,
    /// date → 聚合（该文件全部记录；cutoff 在合并阶段按日期过滤）
    days: HashMap<String, DayTotals>,
    /// model → date → 聚合
    by_model: HashMap<String, HashMap<String, DayTotals>>,
    /// project → date → 聚合
    by_project: HashMap<String, HashMap<String, DayTotals>>,
    parse_errors: u64,
}

/// 解析单个 jsonl 文件为按日聚合（不做 cutoff 过滤，窗口过滤在合并阶段）。
/// 兼容性容错（不计入 parse_errors）：
///   ①空白行（JSONL 尾部双换行常见）；②UTF-8 BOM；③非法 UTF-8 字节（lossy 解码）；
///   ④客户端写入中的半行——文件未以换行结尾且坏行恰为末行（客户端写完后
///   mtime/size 变化自然触发重扫）。其余真正损坏的行仍计数，保留告警价值。
fn parse_file(path: &Path, fallback_project: &str) -> FileCacheEntry {
    let mut entry = FileCacheEntry::default();
    entry.rev = PARSE_REV;
    let Ok(raw) = std::fs::read(path) else {
        entry.parse_errors = 1;
        return entry;
    };
    let ends_with_newline = raw.last() == Some(&b'\n');
    let text = String::from_utf8_lossy(&raw);
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let is_last = lines.peek().is_none();
        let s = line.trim().trim_start_matches('\u{feff}').trim();
        if s.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(s) else {
            if is_last && !ends_with_newline {
                continue;
            }
            entry.parse_errors += 1;
            continue;
        };
        if record_ts(&value).is_none() {
            continue;
        }
        let Some(u) = usage(&value) else {
            continue;
        };
        let Some(day) = record_date(&value) else {
            continue;
        };
        let d = DayTotals {
            input: u.input,
            output: u.output,
            read: u.read,
            write: u.write,
            calls: 1,
        };
        entry.days.entry(day.clone()).or_default().add(&d);
        let model = record_model(&value);
        entry
            .by_model
            .entry(model)
            .or_default()
            .entry(day.clone())
            .or_default()
            .add(&d);
        let project = project_of(&value, fallback_project);
        entry
            .by_project
            .entry(project)
            .or_default()
            .entry(day)
            .or_default()
            .add(&d);
    }
    entry
}

fn file_meta_ms(path: &Path) -> Option<(i64, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime_ms = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as i64;
    Some((mtime_ms, meta.len()))
}

/// cutoff 日期（今天回看 WINDOW_DAYS 天，本地时区自然日）
fn cutoff_date_str() -> String {
    (chrono::Local::now() - chrono::Duration::days(WINDOW_DAYS))
        .format("%Y-%m-%d")
        .to_string()
}

/// 本地日期 → 当日 0 点毫秒（coverage 近似值，仅展示用途）
fn day_to_ms(date: &str, end_of_day: bool) -> Option<i64> {
    let d = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    let naive = if end_of_day {
        d.and_hms_micro_opt(23, 59, 59, 999_999)?
    } else {
        d.and_hms_opt(0, 0, 0)?
    };
    match chrono::Local.from_local_datetime(&naive) {
        chrono::LocalResult::Single(dt) => Some(dt.timestamp_millis()),
        chrono::LocalResult::Ambiguous(dt, _) => Some(dt.timestamp_millis()),
        chrono::LocalResult::None => None,
    }
}

/// 扫描单个根目录（~/.workbuddy/projects 或 ~/.codebuddy/projects）。
/// 命中增量缓存的文件零解析；返回聚合视图，同时把扫到的文件键记入 `seen`
/// （调用方据此清理已删除文件的缓存条目）。
fn scan_root(
    root: &Path,
    name: &str,
    cutoff: &str,
    cache: &mut HashMap<String, FileCacheEntry>,
    seen: &mut HashSet<String>,
) -> Value {
    let mut paths = Vec::new();
    collect_jsonl(root, &mut paths);
    paths.sort();

    let mut total = Totals::default();
    let mut models: HashMap<String, Totals> = HashMap::new();
    let mut projects: HashMap<String, Totals> = HashMap::new();
    let mut daily: HashMap<String, Totals> = HashMap::new();
    // 天 × 模型 粒度（双轴图模型筛选与模型排行的数据源）
    let mut daily_by_model: HashMap<String, HashMap<String, Totals>> = HashMap::new();
    let mut parse_errors: u64 = 0;
    let mut coverage_start: Option<i64> = None;
    let mut coverage_end: Option<i64> = None;

    for path in &paths {
        let key = path.to_string_lossy().to_string();
        seen.insert(key.clone());
        let meta = file_meta_ms(path);
        let hit = match (&cache.get(&key), meta) {
            // rev 不匹配（旧版本解析逻辑的缓存）→ 强制重解析，清除历史误计数
            (Some(e), Some((mtime, size))) => {
                e.mtime_ms == mtime && e.size == size && e.rev == PARSE_REV
            }
            _ => false,
        };
        let entry = if hit {
            cache.get(&key).cloned().unwrap_or_default()
        } else {
            let fallback_project = dir_project_name(root, path);
            let mut e = parse_file(path, &fallback_project);
            if let Some((mtime, size)) = meta {
                e.mtime_ms = mtime;
                e.size = size;
            }
            cache.insert(key, e.clone());
            e
        };

        parse_errors = parse_errors.saturating_add(entry.parse_errors);
        for (date, d) in &entry.days {
            if date.as_str() < cutoff {
                continue;
            }
            total.add_day(d);
            daily.entry(date.clone()).or_default().add_day(d);
            if let Some(lo) = day_to_ms(date, false) {
                coverage_start = Some(coverage_start.map_or(lo, |c| c.min(lo)));
            }
            if let Some(hi) = day_to_ms(date, true) {
                coverage_end = Some(coverage_end.map_or(hi, |c| c.max(hi)));
            }
        }
        for (model, days) in &entry.by_model {
            for (date, d) in days {
                if date.as_str() < cutoff {
                    continue;
                }
                models.entry(model.clone()).or_default().add_day(d);
                daily_by_model
                    .entry(model.clone())
                    .or_default()
                    .entry(date.clone())
                    .or_default()
                    .add_day(d);
            }
        }
        for (project, days) in &entry.by_project {
            for (date, d) in days {
                if date.as_str() < cutoff {
                    continue;
                }
                projects.entry(project.clone()).or_default().add_day(d);
            }
        }
    }

    let mut daily_by_model_out: Map<String, Value> = Map::new();
    for (model, points) in daily_by_model {
        let mut arr: Vec<(String, Value)> = points
            .into_iter()
            .map(|(day, t)| {
                let mut v = t.to_value();
                v["date"] = json!(day);
                (day, v)
            })
            .collect();
        arr.sort_by(|a, b| a.0.cmp(&b.0));
        daily_by_model_out.insert(model, Value::Array(arr.into_iter().map(|(_, v)| v).collect()));
    }

    let mut daily_arr: Vec<(String, Value)> = daily
        .into_iter()
        .map(|(day, t)| {
            let mut v = t.to_value();
            v["date"] = json!(day);
            (day, v)
        })
        .collect();
    daily_arr.sort_by(|a, b| a.0.cmp(&b.0));

    json!({
        "source": name,
        "summary": total.to_value(),
        "models": group_vec(models),
        "projects": group_vec(projects),
        "daily": daily_arr.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
        "daily_by_model": daily_by_model_out,
        "files_scanned": paths.len(),
        "parse_errors": parse_errors,
        "coverage_start_at": coverage_start,
        "coverage_end_at": coverage_end,
    })
}

/// 本地 Token 统计（F-26/F-57）：合并 ~/.workbuddy/projects 与 ~/.codebuddy/projects，
/// 固定回看 365 天（热力图数据源）；时间/模型/范围筛选由前端从 daily_by_model 派生。
/// 性能（F-59）：按文件增量缓存（mtime+size 不变零解析）+ 结果级 10 分钟缓存；
/// fresh=true（前端「重扫」按钮）跳过结果缓存强制重扫（仍享受增量缓存）。
#[tauri::command(async)]
pub fn workbuddy_token_stats(state: State<AppState>, fresh: Option<bool>) -> Value {
    if !fresh.unwrap_or(false) {
        if let Ok(guard) = RESULT_CACHE.lock() {
            if let Some((at, v)) = guard.as_ref() {
                if at.elapsed().as_secs() < RESULT_TTL_SECS {
                    return v.clone();
                }
            }
        }
    }

    let home = crate::platform::home_dir();
    let now_ms = chrono::Utc::now().timestamp_millis();
    let cutoff = cutoff_date_str();

    let mut cache: HashMap<String, FileCacheEntry> =
        crate::store::db(&state.data_dir).kv_get("token_stats_files");
    let mut seen: HashSet<String> = HashSet::new();

    let mut merged = scan_root(
        &home.join(".workbuddy").join("projects"),
        "workbuddy",
        &cutoff,
        &mut cache,
        &mut seen,
    );
    let second = scan_root(
        &home.join(".codebuddy").join("projects"),
        "codebuddy-cli",
        &cutoff,
        &mut cache,
        &mut seen,
    );

    // 已删除文件的缓存条目清理
    cache.retain(|k, _| seen.contains(k));
    let _ = crate::store::db(&state.data_dir).kv_set("token_stats_files", &cache);

    // 合并双源：summary/daily/models/projects/daily_by_model 累加
    merge_totals(merged.get_mut("summary"), second.get("summary"));
    merge_group_arrays(merged.get_mut("daily"), second.get("daily"), "date");
    merge_group_arrays(merged.get_mut("models"), second.get("models"), "key");
    merge_group_arrays(merged.get_mut("projects"), second.get("projects"), "key");
    merge_daily_by_model(
        merged.get_mut("daily_by_model"),
        second.get("daily_by_model"),
    );
    let files = merged.get("files_scanned").and_then(Value::as_u64).unwrap_or(0)
        + second.get("files_scanned").and_then(Value::as_u64).unwrap_or(0);
    let errs = merged.get("parse_errors").and_then(Value::as_u64).unwrap_or(0)
        + second.get("parse_errors").and_then(Value::as_u64).unwrap_or(0);
    merged["files_scanned"] = json!(files);
    merged["parse_errors"] = json!(errs);
    merged["generated_at"] = json!(now_ms);
    merged["window_days"] = json!(WINDOW_DAYS);
    merged["cache_hit_files"] = json!(seen.len().saturating_sub(0));
    merged["fresh"] = json!(fresh.unwrap_or(false));

    if let Ok(mut guard) = RESULT_CACHE.lock() {
        *guard = Some((Instant::now(), merged.clone()));
    }
    merged
}

fn add_summary(dst: &mut Value, src: &Value) {
    for k in ["total", "input", "output", "cache_read", "cache_write", "uncached_input", "calls"] {
        let s = src.get(k).and_then(Value::as_u64).unwrap_or(0);
        let d = dst.get(k).and_then(Value::as_u64).unwrap_or(0);
        dst[k] = json!(d + s);
    }
    // 命中率重算
    let read = dst.get("cache_read").and_then(Value::as_u64).unwrap_or(0);
    let input = dst.get("input").and_then(Value::as_u64).unwrap_or(0);
    dst["cache_hit_rate"] = if input > 0 { json!(read as f64 / input as f64) } else { Value::Null };
}

fn merge_totals(dst: Option<&mut Value>, src: Option<&Value>) {
    match (dst, src) {
        (Some(d), Some(s)) => add_summary(d, s),
        _ => {}
    }
}

/// 按 key 分组的聚合数组（date/key 字段对齐累加，重排序）
fn merge_group_arrays(dst: Option<&mut Value>, src: Option<&Value>, key_field: &str) {
    let (Some(d), Some(s)) = (dst, src) else { return };
    let Some(items) = s.as_array() else { return };
    if !d.is_array() {
        *d = Value::Array(vec![]);
    }
    let arr = d.as_array_mut().unwrap();
    for item in items {
        let key = item.get(key_field).and_then(Value::as_str).unwrap_or_default().to_string();
        if let Some(existing) = arr.iter_mut().find(|e| e.get(key_field).and_then(Value::as_str) == Some(key.as_str())) {
            add_summary(existing, item);
        } else {
            arr.push(item.clone());
        }
    }
    arr.sort_by(|a, b| {
        let ka = a.get(key_field).and_then(Value::as_str).unwrap_or_default();
        let kb = b.get(key_field).and_then(Value::as_str).unwrap_or_default();
        ka.cmp(kb)
    });
}

/// daily_by_model：model → [{date,...}] 的双层合并
fn merge_daily_by_model(dst: Option<&mut Value>, src: Option<&Value>) {
    let (Some(d), Some(s)) = (dst, src) else { return };
    let Some(map) = s.as_object() else { return };
    if !d.is_object() {
        *d = Value::Object(Map::new());
    }
    let dmap = d.as_object_mut().unwrap();
    for (model, points) in map {
        match dmap.get_mut(model) {
            Some(existing) => merge_group_arrays(Some(existing), Some(points), "date"),
            None => {
                dmap.insert(model.clone(), points.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_priority_aliases_and_raw_cache_write() {
        let value = json!({
            "providerData": {
                "usage": { "inputTokens": 99, "outputTokens": 22 },
                "rawUsage": { "prompt_cache_write_tokens": 2 }
            },
            "message": { "usage": {
                "input_tokens": 10,
                "output_tokens": 3,
                "cache_read_input_tokens": 4
            }}
        });
        assert_eq!(usage(&value), Some(Usage { input: 10, output: 3, read: 4, write: 2 }));
    }

    #[test]
    fn cache_read_stale_zero_does_not_hide_positive_alias() {
        let value = json!({
            "usage": {
                "prompt_tokens": 20,
                "completion_tokens": 2,
                "cache_read_input_tokens": 0,
                "prompt_cache_hit_tokens": 12
            }
        });
        assert_eq!(usage(&value), Some(Usage { input: 20, output: 2, read: 12, write: 0 }));
    }

    #[test]
    fn cache_miss_is_not_write() {
        let value = json!({
            "usage": { "input_tokens": 10, "output_tokens": 3, "prompt_cache_miss_tokens": 91 }
        });
        assert_eq!(usage(&value), Some(Usage { input: 10, output: 3, read: 0, write: 0 }));
    }

    #[test]
    fn cache_read_accepts_nested_provider_details() {
        let value = json!({
            "providerData": {
                "usage": {
                    "inputTokens": 99,
                    "outputTokens": 3,
                    "inputTokensDetails": [{ "cached_tokens": 7 }]
                }
            }
        });
        assert_eq!(usage(&value), Some(Usage { input: 99, output: 3, read: 7, write: 0 }));
    }

    #[test]
    fn timestamp_seconds_normalized_and_missing_rejected() {
        let secs = json!({ "timestamp": 1_757_000_000, "usage": { "input_tokens": 1 } });
        assert_eq!(record_ts(&secs), Some(1_757_000_000_000));
        let none = json!({ "usage": { "input_tokens": 1 } });
        assert_eq!(record_ts(&none), None);
    }

    #[test]
    fn usage_requires_input_anchor() {
        let value = json!({ "usage": { "output_tokens": 3 } });
        assert_eq!(usage(&value), None);
    }

    #[test]
    fn parse_file_tolerates_bom_blank_lines_and_partial_tail() {
        let dir = std::env::temp_dir().join(format!("wb_stats_p1_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        let good = json!({
            "timestamp": 1_757_000_000_000i64,
            "usage": { "input_tokens": 5, "output_tokens": 1 }
        });
        // BOM 头 + 双换行空行 + 末行无换行（完整 JSON）
        let mut content = String::from("\u{feff}");
        content.push_str(&good.to_string());
        content.push_str("\n\n");
        content.push_str(&good.to_string());
        std::fs::write(&path, &content).unwrap();
        let e = parse_file(&path, "p");
        assert_eq!(e.parse_errors, 0);
        assert_eq!(e.days.values().map(|d| d.calls).sum::<u64>(), 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn parse_file_counts_corrupt_line_but_skips_partial_tail() {
        let dir = std::env::temp_dir().join(format!("wb_stats_p2_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        // 中间坏行（换行结尾）→ 计数；末行半截 JSON（无换行）→ 视为写入中，忽略
        std::fs::write(&path, "{broken}\n{\"timestamp\":1,\"usage\":{\"in").unwrap();
        let e = parse_file(&path, "p");
        assert_eq!(e.parse_errors, 1);
        let _ = std::fs::remove_file(&path);
    }
}
