//! 本地 WorkBuddy / CodeBuddy CLI JSONL Token 统计（F-26/F-57，批次3）。
//! 方案对齐 oss-research/workbuddy-switch token_stats.rs 的解析语义：
//! usage 取值优先级 message.usage > providerData.usage > 顶层 usage；
//! cache_read 别名链优先正值（cache_read_input_tokens → prompt_cache_hit_tokens），
//! 兼容嵌套 prompt_tokens_details / inputTokensDetails；
//! cache_write 仅认显式别名（prompt_cache_miss_tokens 是新增输入，不是写入）。
//!
//! ⚠ serde 命名约定：输出字段全部 snake_case；响应只含聚合数字，不返回消息正文/凭证。

use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

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
    fn add(&mut self, usage: Usage) {
        self.usage.input = self.usage.input.saturating_add(usage.input);
        self.usage.output = self.usage.output.saturating_add(usage.output);
        self.usage.read = self.usage.read.saturating_add(usage.read);
        self.usage.write = self.usage.write.saturating_add(usage.write);
        self.calls = self.calls.saturating_add(1);
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

/// 扫描单个根目录（~/.workbuddy/projects 或 ~/.codebuddy/projects），返回聚合视图
fn scan_root(root: &Path, name: &str, cutoff_ms: i64) -> Value {
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
        let Ok(file) = std::fs::File::open(path) else {
            parse_errors = parse_errors.saturating_add(1);
            continue;
        };
        let fallback_project = dir_project_name(root, path);
        for line in BufReader::new(file).lines() {
            let Ok(line) = line else {
                parse_errors += 1;
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                parse_errors += 1;
                continue;
            };
            // 缺时间戳或早于 cutoff 的记录排除（不按 mtime 猜测）
            let Some(ts) = record_ts(&value) else {
                continue;
            };
            if ts < cutoff_ms {
                continue;
            }
            let Some(u) = usage(&value) else {
                continue;
            };
            let project = project_of(&value, &fallback_project);
            let model_name = record_model(&value);
            total.add(u);
            models.entry(model_name.clone()).or_default().add(u);
            projects.entry(project).or_default().add(u);
            if let Some(day) = record_date(&value) {
                daily.entry(day.clone()).or_default().add(u);
                daily_by_model
                    .entry(model_name)
                    .or_default()
                    .entry(day)
                    .or_default()
                    .add(u);
            }
            coverage_start = Some(coverage_start.map_or(ts, |c| c.min(ts)));
            coverage_end = Some(coverage_end.map_or(ts, |c| c.max(ts)));
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
#[tauri::command(async)]
pub fn workbuddy_token_stats() -> Value {
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    let now_ms = chrono::Utc::now().timestamp_millis();
    let cutoff = now_ms - 365 * 86_400_000;

    let mut merged = scan_root(&Path::new(&home).join(".workbuddy").join("projects"), "workbuddy", cutoff);
    let second = scan_root(&Path::new(&home).join(".codebuddy").join("projects"), "codebuddy-cli", cutoff);

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
    merged["window_days"] = json!(365);
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
}
