//! 本地 Token 统计（F-26/F-57，批次3）：三路来源合并为「本地源」。
//! ① `~/.workbuddy/projects` / `~/.codebuddy/projects` 会话 JSONL（WorkBuddy 桌面端、
//!    CodeBuddy CLI）：usage 取值优先级 message.usage > providerData.usage > 顶层 usage；
//!    cache_read 别名链优先正值（cache_read_input_tokens → prompt_cache_hit_tokens），
//!    兼容嵌套 prompt_tokens_details / inputTokensDetails；
//!    cache_write 仅认显式别名（prompt_cache_miss_tokens 是新增输入，不是写入）。
//! ② `%LOCALAPPDATA%\CodeBuddyExtension\Data\<uid>\CodeBuddyIDE\<uid>\history\<md5(工作区)>
//!    \<convId>\index.json` 的 `requests[]`（CodeBuddy **IDE** 侧，2026-10-06 接入）——
//!    见 `parse_codebuddy_index` 的字段契约与口径说明。
//!
//! ⚠ serde 命名约定：输出字段全部 snake_case；响应只含聚合数字，不返回消息正文/凭证。
//! ⚠ 只读：CodeBuddy IDE 目录仅读取，绝不写入（其历史树由客户端自身维护）。

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

/// 递归收集 jsonl（跳过 subagents 子代理目录——重复父会话上下文，不计入用量）。
/// 性能（2026-10-07）：目录判定用 `DirEntry::file_type()`（Windows 读目录项自带，
/// 免一次额外 stat），替代原 `path.is_dir()`。
fn collect_jsonl(root: &Path, output: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            if path.file_name().and_then(|name| name.to_str()) != Some("subagents") {
                collect_jsonl(&path, output);
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            output.push(path);
        }
    }
}

/// 子目录列表（仅目录；`DirEntry::file_type()` 免额外 stat）。
fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            out.push(entry.path());
        }
    }
    out
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
/// 2 → 3（2026-10-07）：解析路径引入 `"usage"` 快速预筛（不含该字面量的行不再解析，
/// parse_errors 语义随之收紧），旧缓存条目须强制重解析一次以清除口径差。
/// 3 → 4（2026-10-08）：CodeBuddy IDE 源新增跨文件按 request id 去重（`FileCacheEntry::ids`），
/// 旧条目没有 id 列表无法判重，须强制重解析一次。
const PARSE_REV: u32 = 4;

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
    /// 本文件**实际计入**的 request id（仅 CodeBuddy IDE 索引源填充；JSONL 源为空）。
    /// 用途：同一批请求会被以**新的会话 id** 重新登记到另一个 uid 的目录下（实测同一
    /// workspace 下 609 个 id 跨 uid 重复，仅靠「文件内去重」会整份多计），聚合层据此
    /// 做跨文件去重：整份重复 → 整文件跳过；无重复 → 走快路径；部分重复 → 过滤重解析。
    #[serde(default)]
    ids: Vec<String>,
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
        // 快速预筛（2026-10-07 性能）：本函数只消费含 usage 的记录，不含 `"usage"`
        // 字面量的行无需解析——会话 jsonl 里多数行是用户/工具消息，实测可省掉大半
        // serde 开销（344 MB jsonl 集的逐行解析是冷扫的可变成本主项）。
        if !s.contains("\"usage\"") {
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

// ── CodeBuddy IDE 会话索引（requests[] 用量）────────────────────────────────
//
// 数据位置（2026-10-06 本机实测；与 WorkDaddy `scripts/codebuddy-files.js` 的
// tokenOptions() 独立互证——其 readRecords 同样读 index.requests，source 标记
// 'local-codebuddy-requests'）：
//   %LOCALAPPDATA%\CodeBuddyExtension\Data\
//     <uid>\CodeBuddyIDE\<uid>\history\<md5(工作区路径)>\<会话id>\index.json
// 会话目录下的 messages/*.json 只有正文（无用量字段），**用量只在会话级 index.json**。
//
// 字段契约：
//   requests[]: { id, type: "craft"|"plan", messages[], state: "complete"|"running"|"canceled",
//                 startedAt: 毫秒, usage: {...} }
//   usage:      { inputTokens, outputTokens, totalTokens, lastTokens,
//                 cacheTokens(缓存读), cachedWriteTokens, cachedMissTokens, credit }
//   inputTokens == cacheTokens + cachedMissTokens —— 单条 input 是「整段 prompt」口径，
//   与 WorkBuddy 侧同构：total = input + output + cache_write，缓存读单列不重复计。
//
// 计入规则：
//   - `state == "running"` 跳过（进行中，usage 可能只是部分快照）；
//   - 四类 token 全 0 跳过（对齐 WorkDaddy 的丢弃条件）；
//   - 日期取 startedAt（<1e12 视为秒 ×1000）；
//   - 模型取同工作区索引 conversations[].modelMap[type]（回退 modelMap.craft /
//     selectedModelId）——会话级粒度，会话内换模型不回溯；
//   - 项目维度记常量 `CodeBuddy IDE`（工作区目录名是 md5(cwd)，反查项目名需另读
//     `%APPDATA%\CodeBuddy CN\codebuddy-sessions.vscdb`，留作后续增强）；
//   - `usage.credit`（该请求真实扣积分）暂不并入积分统计，避免与官方积分源重复计数。
const CODEBUDDY_IDE_PROJECT: &str = "CodeBuddy IDE";

fn codebuddy_usage(usage: &Map<String, Value>) -> Usage {
    Usage {
        input: field(usage, &["inputTokens", "input_tokens", "promptTokens"]).unwrap_or(0),
        output: field(usage, &["outputTokens", "output_tokens", "completionTokens"]).unwrap_or(0),
        read: field(usage, &["cacheTokens", "cache_read_input_tokens", "cached_tokens"]).unwrap_or(0),
        write: field(usage, &["cachedWriteTokens", "cache_write_input_tokens"]).unwrap_or(0),
    }
}

/// 毫秒时间戳（<1e12 视为秒，×1000）→ 本地日期
fn ms_date(value: &Value) -> Option<String> {
    let ts = number(value)? as i64;
    let ms = if ts < 10_000_000_000 { ts.saturating_mul(1000) } else { ts };
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
}

/// 会话模型：工作区索引 conversations[].modelMap[type] > modelMap.craft > selectedModelId。
/// `memo` 在同一轮扫描内缓存已解析的工作区索引（同一工作区多个会话共享一次读取）。
fn conversation_model(
    memo: &mut HashMap<String, Value>,
    workspace_dir: &Path,
    conv_id: &str,
    req_type: &str,
) -> String {
    let key = workspace_dir.to_string_lossy().to_string();
    let conversations = memo.entry(key).or_insert_with(|| {
        std::fs::read(workspace_dir.join("index.json"))
            .ok()
            .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
            .and_then(|v| v.get("conversations").cloned())
            .unwrap_or(Value::Null)
    });
    conversations
        .as_array()
        .and_then(|arr| {
            arr.iter()
                .find(|c| c.get("id").and_then(Value::as_str) == Some(conv_id))
        })
        .and_then(|c| {
            let model_map = c.get("modelMap");
            model_map
                .and_then(|m| m.get(req_type))
                .and_then(Value::as_str)
                .or_else(|| model_map.and_then(|m| m.get("craft")).and_then(Value::as_str))
                .or_else(|| c.get("selectedModelId").and_then(Value::as_str))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "未知模型".to_string())
}

/// `%LOCALAPPDATA%\CodeBuddyExtension\Data`（CodeBuddy IDE 历史根）；
/// 缺环境变量或非 Windows 时返回 None（该源整体跳过，不影响另两路）。
fn codebuddy_ide_root() -> Option<PathBuf> {
    let dir = std::env::var("LOCALAPPDATA").ok()?;
    if dir.trim().is_empty() {
        return None;
    }
    Some(Path::new(&dir).join("CodeBuddyExtension").join("Data"))
}

/// 收集 CodeBuddy IDE 历史下的**会话级** `index.json`。
///
/// 性能（2026-10-07）：改为**按已知结构定向下探**，
/// `Data\<uid>\<product>\<workspace>\history\<ws-hash>\<session>\index.json`——
/// 只收集会话级 index.json（工作区级那个由 `conversation_model` 按需直读，
/// 不进本列表，省掉一次无用解析；实测工作区级索引不含 `requests`，排除无损失）。
/// 原实现从 `Data` 整树递归找「名为 index.json 且祖先含 history」的文件——
/// 该根下有约 1 万个目录 / 10 万个文件（约 1.5 GB，含 `messages\`、file-tree、
/// check-point 等无关子树），为找数百个会话索引要把整棵树 stat 一遍
/// （实测遍历+stat 约 7~12s），是「Token 统计慢」的主要成本。
///
/// `<product>` 不写死：实测至少存在 `CodeBuddyIDE` 与 `VSCode` 两套布局，
/// 后者同样落 `history\<ws-hash>\<session>\index.json` 且含真实用量
/// （本机 45 个会话索引中 4 个有用量），只看 `CodeBuddyIDE` 会静默漏计。
/// 结构异常（新版布局变化）时按**产品子树**回退限深递归兜底（剪掉已知重子树）。
fn collect_codebuddy_indexes(data_root: &Path, output: &mut Vec<PathBuf>) {
    for uid_dir in subdirs(data_root) {
        for product_dir in subdirs(&uid_dir) {
            let before = output.len();
            collect_session_indexes(&product_dir, output);
            if output.len() == before {
                // 兜底按**产品子树**判零：某产品布局漂移（如新增层级）而其余产品仍
                // 命中时，全局判零不会触发兜底、该子树的会话索引会被静默漏收。
                // 子树级判零只对异常子树限深递归，正常子树零慢路径不变；
                // 起点比全局兜底深一层、深度预算更足，覆盖原全局兜底的全部搜索
                // 范围（Data 根直接子树即全部 product_dir，根上文件本就不含
                // history 祖先），故不再保留全局兜底。
                collect_codebuddy_indexes_deep(&product_dir, 0, output);
            }
        }
    }
}

/// 单个产品子树的结构化定向下探：`<workspace>\history\<ws-hash>\<session>\index.json`。
fn collect_session_indexes(product_dir: &Path, output: &mut Vec<PathBuf>) {
    for workspace_root in subdirs(product_dir) {
        let history = workspace_root.join("history");
        for ws_hash in subdirs(&history) {
            for session in subdirs(&ws_hash) {
                let idx = session.join("index.json");
                if idx.is_file() {
                    output.push(idx);
                }
            }
        }
    }
}

/// 兜底用的限深递归（剪掉已知重子树：messages/file-tree/check-point/plan-task）。
fn collect_codebuddy_indexes_deep(dir: &Path, depth: usize, output: &mut Vec<PathBuf>) {
    if depth > 8 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if matches!(name, "messages" | "file-tree" | "check-point" | "plan-task") {
                continue;
            }
            collect_codebuddy_indexes_deep(&path, depth + 1, output);
        } else if path.file_name().and_then(|n| n.to_str()) == Some("index.json")
            && path
                .components()
                .any(|c| c.as_os_str().to_str() == Some("history"))
        {
            output.push(path);
        }
    }
}

/// 解析 CodeBuddy IDE 会话索引为按日聚合（与 parse_file 同构的增量缓存单元）。
/// 工作区级 index.json（只有 conversations/current）自然产出空条目，零误计。
///
/// 去重分两层（实测驱动，2026-10-08）：
/// 1. **文件内**：同一会话内按 request id 去重（原地）；
/// 2. **跨文件**：由聚合层做（见 `aggregate_files` 的 IDE 分支）——同一批请求会被以
///    **新的会话 id** 重新登记到另一个 uid 的目录下（实测同一 workspace 下 609 个 id
///    跨 uid 重复，仅靠文件内去重会整份多计）。此处把「实际计入的 id」记进 `entry.ids`，
///    `skip` 非空时跳过其中已计的 id（跨文件部分重复时的重算路径）。
fn parse_codebuddy_index(
    path: &Path,
    memo: &mut HashMap<String, Value>,
    skip: Option<&HashSet<String>>,
) -> FileCacheEntry {
    let mut entry = FileCacheEntry { rev: PARSE_REV, ..Default::default() };
    let Ok(raw) = std::fs::read(path) else {
        entry.parse_errors = 1;
        return entry;
    };
    let Ok(value) = serde_json::from_slice::<Value>(&raw) else {
        entry.parse_errors = 1;
        return entry;
    };
    let Some(requests) = value.get("requests").and_then(Value::as_array) else {
        return entry;
    };
    let session_dir = path.parent();
    let conv_id = session_dir
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let workspace_dir = session_dir.and_then(|p| p.parent());
    // 同会话内按 request id 去重（跨文件/跨 uid 副本由聚合层判重，见函数注释）
    let mut seen_ids: HashSet<&str> = HashSet::new();
    for request in requests {
        let Some(object) = request.as_object() else { continue };
        let Some(id) = object.get("id").and_then(Value::as_str) else { continue };
        if !seen_ids.insert(id) {
            continue;
        }
        if object.get("state").and_then(Value::as_str) == Some("running") {
            continue;
        }
        let Some(usage_object) = object.get("usage").and_then(Value::as_object) else { continue };
        let u = codebuddy_usage(usage_object);
        if u == Usage::default() {
            continue;
        }
        let Some(day) = object.get("startedAt").and_then(ms_date) else { continue };
        // 实际计入的 id（供聚合层跨文件判重）；skip 命中表示该 id 已在别的文件计过，
        // 只登记不聚合。聚合结果是文件的纯函数（与 skip 无关时），故仍可进增量缓存
        entry.ids.push(id.to_string());
        if skip.is_some_and(|counted| counted.contains(id)) {
            continue;
        }
        let req_type = object.get("type").and_then(Value::as_str).unwrap_or("craft");
        let model = match workspace_dir {
            Some(ws) => conversation_model(memo, ws, conv_id, req_type),
            None => "未知模型".to_string(),
        };
        let d = DayTotals {
            input: u.input,
            output: u.output,
            read: u.read,
            write: u.write,
            calls: 1,
        };
        entry.days.entry(day.clone()).or_default().add(&d);
        entry
            .by_model
            .entry(model)
            .or_default()
            .entry(day.clone())
            .or_default()
            .add(&d);
        entry
            .by_project
            .entry(CODEBUDDY_IDE_PROJECT.to_string())
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

/// 扫描单个会话 JSONL 根目录（~/.workbuddy/projects 或 ~/.codebuddy/projects）
fn scan_root(
    root: &Path,
    name: &str,
    cutoff: &str,
    cache: &mut HashMap<String, FileCacheEntry>,
    seen: &mut HashSet<String>,
) -> Value {
    let t_walk = Instant::now();
    let mut paths = Vec::new();
    collect_jsonl(root, &mut paths);
    let walk_ms = t_walk.elapsed().as_millis() as u64;
    let mut view = aggregate_files(ScanKind::SessionJsonl, root, name, paths, cutoff, cache, seen);
    view["walk_ms"] = json!(walk_ms);
    view
}

/// 扫描 CodeBuddy IDE 明细根（`%LOCALAPPDATA%\CodeBuddyExtension\Data`）：
/// 会话索引 `requests[]` 与 WorkBuddy 侧合并进同一个「本地源」。
fn scan_codebuddy_ide(
    root: &Path,
    cutoff: &str,
    cache: &mut HashMap<String, FileCacheEntry>,
    seen: &mut HashSet<String>,
) -> Value {
    let t_walk = Instant::now();
    let mut paths = Vec::new();
    collect_codebuddy_indexes(root, &mut paths);
    let walk_ms = t_walk.elapsed().as_millis() as u64;
    let mut view = aggregate_files(
        ScanKind::CodebuddyIndex,
        root,
        "codebuddy-ide",
        paths,
        cutoff,
        cache,
        seen,
    );
    view["walk_ms"] = json!(walk_ms);
    view
}

/// 扫描来源类型：决定「路径 → 增量缓存条目」所用的解析器
#[derive(Clone, Copy, PartialEq, Eq)]
enum ScanKind {
    /// 会话 JSONL（~/.workbuddy/projects、~/.codebuddy/projects）
    SessionJsonl,
    /// CodeBuddy IDE 会话索引（requests[] 数组）
    CodebuddyIndex,
}

/// 多来源共用的聚合过程：命中增量缓存的文件零解析；返回聚合视图，
/// 同时把扫到的文件键记入 `seen`（调用方据此清理已删除文件的缓存条目）。
fn aggregate_files(
    kind: ScanKind,
    root: &Path,
    name: &str,
    mut paths: Vec<PathBuf>,
    cutoff: &str,
    cache: &mut HashMap<String, FileCacheEntry>,
    seen: &mut HashSet<String>,
) -> Value {
    paths.sort();
    // CodeBuddy 侧工作区索引解析缓存（同一工作区多会话共享一次读取）
    let mut memo: HashMap<String, Value> = HashMap::new();

    let mut total = Totals::default();
    let mut models: HashMap<String, Totals> = HashMap::new();
    let mut projects: HashMap<String, Totals> = HashMap::new();
    let mut daily: HashMap<String, Totals> = HashMap::new();
    // 天 × 模型 粒度（双轴图模型筛选与模型排行的数据源）
    let mut daily_by_model: HashMap<String, HashMap<String, Totals>> = HashMap::new();
    let mut parse_errors: u64 = 0;
    let mut coverage_start: Option<i64> = None;
    let mut coverage_end: Option<i64> = None;
    // 性能指标（2026-10-07）：命中/解析计数与解析耗时，随视图返回供 app_log 汇总
    let mut hits: u64 = 0;
    let mut parsed: u64 = 0;
    let mut parse_ms: u64 = 0;
    // 本轮扫描已计入的 request id（仅 CodeBuddy IDE 源使用，见下方跨文件去重）
    let mut counted_ids: HashSet<String> = HashSet::new();

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
            hits += 1;
            cache.get(&key).cloned().unwrap_or_default()
        } else {
            let t_parse = Instant::now();
            let mut e = match kind {
                ScanKind::SessionJsonl => parse_file(path, &dir_project_name(root, path)),
                ScanKind::CodebuddyIndex => parse_codebuddy_index(path, &mut memo, None),
            };
            parsed += 1;
            parse_ms = parse_ms.saturating_add(t_parse.elapsed().as_millis() as u64);
            if let Some((mtime, size)) = meta {
                e.mtime_ms = mtime;
                e.size = size;
            }
            cache.insert(key, e.clone());
            e
        };

        // CodeBuddy IDE 源：跨文件按 request id 全局去重（见 FileCacheEntry::ids）。
        // 同一批请求会被以**新的会话 id** 重新登记到另一个 uid 的目录下，若只做文件内
        // 去重会整份多计（实测本机 609/4123 行、约 16.7% 的 IDE 源用量）。
        let entry = if kind == ScanKind::CodebuddyIndex && !entry.ids.is_empty() {
            let dup = entry
                .ids
                .iter()
                .filter(|id| counted_ids.contains(id.as_str()))
                .count();
            if dup == entry.ids.len() {
                // 整份重复：整文件跳过（其全部请求都已在别的文件计入）
                parse_errors = parse_errors.saturating_add(entry.parse_errors);
                continue;
            }
            let mut filtered: Option<FileCacheEntry> = None;
            if dup > 0 {
                // 部分重复：按「本文件之前已计 id」过滤后重算。过滤结果依赖扫描顺序，
                // 故不进增量缓存（缓存只存与顺序无关的全量聚合）
                let t_reparse = Instant::now();
                filtered = Some(parse_codebuddy_index(path, &mut memo, Some(&counted_ids)));
                parsed += 1;
                parse_ms = parse_ms.saturating_add(t_reparse.elapsed().as_millis() as u64);
            }
            for id in &entry.ids {
                counted_ids.insert(id.clone());
            }
            filtered.unwrap_or(entry)
        } else {
            entry
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
        // 性能指标（2026-10-07）：命中数与解析数分开计数（原 cache_hit_files 误为
        // 「扫到的文件数」）；parse_ms 为本源解析耗时合计，walk_ms 由 scan_* 回填
        "cache_hit_files": hits,
        "files_parsed": parsed,
        "parse_ms": parse_ms,
    })
}

/// 本地 Token 统计（F-26/F-57）：合并三路来源，固定回看 365 天（热力图数据源）；
/// 时间/模型/范围筛选由前端从 daily_by_model 派生。
///
/// **有效覆盖范围（2026-10-06 实测校正）**：
/// ① `~/.workbuddy/projects`——WorkBuddy 桌面端会话 JSONL，本机真实产出（164 项目目录）；
/// ② `~/.codebuddy/projects`——CodeBuddy CLI 会话 JSONL，本机实测仅有 agent 的
///    memory/*.md、零 jsonl（扫描保留以兼容其他环境）；
/// ③ `%LOCALAPPDATA%\CodeBuddyExtension\Data\<uid>\CodeBuddyIDE\<uid>\history\...\`——
///    **CodeBuddy IDE 侧**，用量在会话级 `index.json` 的 `requests[]`（见
///    `parse_codebuddy_index`）。此项为本轮新增覆盖：此前误判为「IDE 侧无本地用量」
///    （当时只查了 messages/*.json，那里确实只有正文）。
/// 故「本地源」现为「WorkBuddy 桌面端 + CodeBuddy IDE」口径；未走本地记录的部分
/// （如经 API 网关转发的流量）仍由网关源（api_usage bucket=wb）单列，二者不合并。
///
/// 性能（F-59）：按文件增量缓存（mtime+size 不变零解析）+ 结果级 10 分钟缓存；
/// fresh=true（前端「重扫」按钮）跳过结果缓存强制重扫（仍享受增量缓存）。
#[tauri::command(async)]
pub fn workbuddy_token_stats(state: State<AppState>, fresh: Option<bool>) -> Value {
    workbuddy_token_stats_impl(&state, fresh.unwrap_or(false))
}

/// 实现（本命令与调度器 wb-credits-snapshot Token 同步共用）：
/// fresh=true（前端「重扫」/调度同步）跳过结果缓存强制重扫（仍享受增量缓存）
pub(crate) fn workbuddy_token_stats_impl(state: &AppState, fresh: bool) -> Value {
    let t_impl = Instant::now();
    if !fresh {
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

    // 文件级增量缓存（键=文件路径，值=按日聚合）——整块读写，故单独计时（持全局
    // DB 锁期间会阻塞 App 其他读写，2026-10-07 埋点纳入观察）
    let t_db_read = Instant::now();
    let mut cache: HashMap<String, FileCacheEntry> =
        crate::store::db(&state.data_dir).kv_get("token_stats_files");
    let db_read_ms = t_db_read.elapsed().as_millis() as u64;
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
    // 第三路：CodeBuddy IDE 会话索引 requests[]（%LOCALAPPDATA%\CodeBuddyExtension\Data）
    let third = codebuddy_ide_root()
        .map(|root| scan_codebuddy_ide(&root, &cutoff, &mut cache, &mut seen));

    // 已删除文件的缓存条目清理（须在三路扫描都完成后执行）
    cache.retain(|k, _| seen.contains(k));
    let t_db_write = Instant::now();
    let _ = crate::store::db(&state.data_dir).kv_set("token_stats_files", &cache);
    let db_write_ms = t_db_write.elapsed().as_millis() as u64;

    // 合并三路来源：summary/daily/models/projects/daily_by_model 累加
    let ide = third.as_ref();
    merge_totals(merged.get_mut("summary"), second.get("summary"));
    merge_group_arrays(merged.get_mut("daily"), second.get("daily"), "date");
    merge_group_arrays(merged.get_mut("models"), second.get("models"), "key");
    merge_group_arrays(merged.get_mut("projects"), second.get("projects"), "key");
    merge_daily_by_model(
        merged.get_mut("daily_by_model"),
        second.get("daily_by_model"),
    );
    merge_totals(merged.get_mut("summary"), ide.and_then(|v| v.get("summary")));
    merge_group_arrays(
        merged.get_mut("daily"),
        ide.and_then(|v| v.get("daily")),
        "date",
    );
    merge_group_arrays(
        merged.get_mut("models"),
        ide.and_then(|v| v.get("models")),
        "key",
    );
    merge_group_arrays(
        merged.get_mut("projects"),
        ide.and_then(|v| v.get("projects")),
        "key",
    );
    merge_daily_by_model(
        merged.get_mut("daily_by_model"),
        ide.and_then(|v| v.get("daily_by_model")),
    );
    // 三路指标汇总（含 2026-10-07 新增的命中/解析分离计数与分段耗时）
    let pick = |v: Option<&Value>, key: &str| -> u64 {
        v.and_then(|x| x.get(key)).and_then(Value::as_u64).unwrap_or(0)
    };
    let first = Some(&merged);
    let sec = Some(&second);
    let files = pick(first, "files_scanned") + pick(sec, "files_scanned") + pick(ide, "files_scanned");
    let errs = pick(first, "parse_errors") + pick(sec, "parse_errors") + pick(ide, "parse_errors");
    let hits = pick(first, "cache_hit_files") + pick(sec, "cache_hit_files") + pick(ide, "cache_hit_files");
    let parsed = pick(first, "files_parsed") + pick(sec, "files_parsed") + pick(ide, "files_parsed");
    let parse_ms = pick(first, "parse_ms") + pick(sec, "parse_ms") + pick(ide, "parse_ms");
    let walk_ms = pick(first, "walk_ms") + pick(sec, "walk_ms") + pick(ide, "walk_ms");
    merged["files_scanned"] = json!(files);
    merged["parse_errors"] = json!(errs);
    merged["generated_at"] = json!(now_ms);
    merged["window_days"] = json!(WINDOW_DAYS);
    // 修正语义（原为 seen.len()＝扫到的文件数，与字段名不符）
    merged["cache_hit_files"] = json!(hits);
    merged["files_parsed"] = json!(parsed);
    merged["scan_ms"] = json!(t_impl.elapsed().as_millis() as u64);
    merged["fresh"] = json!(fresh);

    // 分段耗时埋点（2026-10-07 性能优化配套）：用于对比优化前后与定位回退
    // （walk=遍历+stat，parse=逐文件解析，db=文件级缓存整块读写）
    crate::fs_utils::app_log(
        &state.data_dir,
        &format!(
            "[wb-token] 扫描 {:.2}s：文件 {}（命中 {} / 解析 {}），walk {}ms / parse {}ms / db 读 {}ms 写 {}ms，缓存条目 {}，解析错误 {}",
            t_impl.elapsed().as_millis() as f64 / 1000.0,
            files,
            hits,
            parsed,
            walk_ms,
            parse_ms,
            db_read_ms,
            db_write_ms,
            cache.len(),
            errs
        ),
    );

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
        // 中间坏行（含 usage 锚点，换行结尾）→ 计数；末行半截 JSON（无换行）→ 视为写入中，忽略
        std::fs::write(&path, "{\"usage\":{broken}\n{\"timestamp\":1,\"usage\":{\"in").unwrap();
        let e = parse_file(&path, "p");
        assert_eq!(e.parse_errors, 1);
        let _ = std::fs::remove_file(&path);
    }

    /// 2026-10-07 性能预筛：不含 `"usage"` 字面量的行不解析（会话 jsonl 里多数行是
    /// 用户/工具消息），故这类坏行也不再计入 parse_errors——语义由本测锁定。
    #[test]
    fn parse_file_skips_lines_without_usage_anchor() {
        let dir = std::env::temp_dir().join(format!("wb_stats_p3_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        std::fs::write(
            &path,
            "{broken-no-usage}\n{\"role\":\"user\",\"content\":\"hi\"}\n",
        )
        .unwrap();
        let e = parse_file(&path, "p");
        assert_eq!(e.parse_errors, 0, "无 usage 锚点的行不解析、不计数");
        assert_eq!(e.days.len(), 0);
        let _ = std::fs::remove_file(&path);
    }

    // ── CodeBuddy IDE 会话索引（requests[]）────────────────────────────────

    #[test]
    fn codebuddy_usage_maps_ide_fields_and_aliases() {
        let usage_object = json!({
            "inputTokens": 1000,
            "outputTokens": 50,
            "cacheTokens": 800,
            "cachedWriteTokens": 12,
            "cachedMissTokens": 200,
            "credit": 1.67
        });
        let u = codebuddy_usage(usage_object.as_object().unwrap());
        assert_eq!(u, Usage { input: 1000, output: 50, read: 800, write: 12 });
        // 别名回退（cache_write 只认显式别名；cachedMissTokens 是新增输入，不是写入）
        let aliased = json!({
            "input_tokens": 5,
            "output_tokens": 2,
            "cache_read_input_tokens": 3
        });
        assert_eq!(
            codebuddy_usage(aliased.as_object().unwrap()),
            Usage { input: 5, output: 2, read: 3, write: 0 }
        );
    }

    #[test]
    fn ms_date_normalizes_seconds_and_millis() {
        let ms = json!(1_757_000_000_000i64);
        let secs = json!(1_757_000_000i64);
        assert_eq!(ms_date(&ms), ms_date(&secs));
        assert!(ms_date(&json!("不是时间")).is_none());
        assert!(ms_date(&json!({})).is_none());
    }

    /// 跨文件/跨 uid 的同一请求只计一次（2026-10-08 实测缺陷回归：同一批请求会以**新的
    /// 会话 id** 落到另一个 uid 的目录下）——整份重复的文件整份跳过、部分重复只补新 id，
    /// 且二次扫描（全命中增量缓存）结果一致（去重只作用于聚合层，不污染缓存）。
    #[test]
    fn codebuddy_index_cross_file_request_id_dedup() {
        let root = std::env::temp_dir().join(format!("wb_stats_dedup_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let ws = "d41d8cd98f00b204e9800998ecf8427e";
        let stamp = 1_757_000_000_000i64;
        let req = |id: &str, input: u64| {
            json!({
                "id": id, "type": "craft", "state": "complete", "startedAt": stamp,
                "usage": { "inputTokens": input, "outputTokens": 1 }
            })
        };
        let write_session = |uid: &str, conv: &str, requests: Vec<Value>| {
            let dir = root
                .join(uid)
                .join("CodeBuddyIDE")
                .join(uid)
                .join("history")
                .join(ws)
                .join(conv);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("index.json"),
                json!({ "requests": requests }).to_string(),
            )
            .unwrap();
        };
        // A：r1/r2；B：完全相同的一批（另一 uid、新会话 id）；C：r2 重复 + r3 新增
        write_session("uA", "conv-a", vec![req("r1", 100), req("r2", 200)]);
        write_session("uB", "conv-b", vec![req("r1", 100), req("r2", 200)]);
        write_session("uC", "conv-c", vec![req("r2", 200), req("r3", 400)]);

        let mut paths = Vec::new();
        collect_codebuddy_indexes(&root, &mut paths);
        assert_eq!(paths.len(), 3, "三个会话索引都应被收集: {paths:?}");

        let mut cache = HashMap::new();
        let mut seen = HashSet::new();
        let view = aggregate_files(
            ScanKind::CodebuddyIndex,
            &root,
            "codebuddy-ide",
            paths,
            "1970-01-01",
            &mut cache,
            &mut seen,
        );
        // 唯一请求 3 条：100 + 200 + 400
        assert_eq!(view["summary"]["input"], json!(700));
        assert_eq!(view["summary"]["calls"], json!(3));
        assert_eq!(view["files_scanned"], json!(3));

        // 二次扫描：三个文件全部命中增量缓存，去重仍须得到同样结果
        let mut paths2 = Vec::new();
        collect_codebuddy_indexes(&root, &mut paths2);
        let mut cache2 = cache.clone();
        let mut seen2 = HashSet::new();
        let view2 = aggregate_files(
            ScanKind::CodebuddyIndex,
            &root,
            "codebuddy-ide",
            paths2,
            "1970-01-01",
            &mut cache2,
            &mut seen2,
        );
        assert_eq!(view2["summary"]["input"], json!(700));
        assert_eq!(view2["summary"]["calls"], json!(3));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 手动诊断（默认忽略）：在**真实** CodeBuddy IDE 目录上对比「跨文件去重前/后」，
    /// 用于确认重复计入的规模：
    /// `cargo test probe_codebuddy_ide_dedup -- --ignored --nocapture`
    #[test]
    #[ignore = "本机诊断：需真实 CodeBuddy IDE 历史目录"]
    fn probe_codebuddy_ide_dedup() {
        let Some(root) = codebuddy_ide_root() else {
            println!("[probe] 未找到 CodeBuddy IDE 数据目录，跳过");
            return;
        };
        let mut paths = Vec::new();
        collect_codebuddy_indexes(&root, &mut paths);
        if paths.is_empty() {
            println!("[probe] 未收集到会话索引，跳过");
            return;
        }
        let mut cache = HashMap::new();
        let mut seen = HashSet::new();
        let t0 = Instant::now();
        let view = aggregate_files(
            ScanKind::CodebuddyIndex,
            &root,
            "codebuddy-ide",
            paths.clone(),
            "1970-01-01",
            &mut cache,
            &mut seen,
        );
        let elapsed = t0.elapsed().as_millis();
        let dedup_total = view["summary"]["total"].as_u64().unwrap_or(0);
        // 对照：不做跨文件去重（各文件全量聚合并列相加，即修复前的口径）
        let raw_total: u64 = cache
            .values()
            .map(|e| {
                e.days
                    .values()
                    .map(|d| d.input.saturating_add(d.output).saturating_add(d.write))
                    .sum::<u64>()
            })
            .sum();
        let dup = raw_total.saturating_sub(dedup_total);
        let pct = if raw_total > 0 {
            dup as f64 / raw_total as f64 * 100.0
        } else {
            0.0
        };
        println!(
            "[probe] 会话索引 {} 个：去重前 total={raw_total}，去重后 total={dedup_total}，\
             多计 {dup}（{pct:.1}%）；聚合耗时 {elapsed}ms",
            paths.len()
        );
    }

    /// 端到端：夹具目录 → 计入 complete/canceled、跳过 running 与全 0、同 id 去重、
    /// 模型取工作区索引 modelMap[type]、项目维度记常量。
    #[test]
    fn parse_codebuddy_index_aggregates_requests_by_model_and_day() {
        let stamp = 1_757_000_000_000i64;
        let expected_day = chrono::DateTime::from_timestamp_millis(stamp)
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d")
            .to_string();
        let uid = "3e385602-a4bb-4f24-b788-a7fc3c367269";
        let ws = "01535c200188b8815071c3182ebb5942";
        let conv = "0fee43607ac04566816c4be1edf4d516";
        let root = std::env::temp_dir().join(format!("wb_stats_cb_{}", std::process::id()));
        let history = root
            .join(uid)
            .join("CodeBuddyIDE")
            .join(uid)
            .join("history")
            .join(ws);
        let session = history.join(conv);
        std::fs::create_dir_all(&session).unwrap();
        std::fs::write(
            history.join("index.json"),
            json!({
                "conversations": [{
                    "id": conv,
                    "type": "craft",
                    "modelMap": { "craft": "deepseek-v4-flash", "plan": "deepseek-v4-pro" }
                }],
                "current": conv
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            session.join("index.json"),
            json!({
                "messages": [],
                "requests": [
                    { "id": "r1", "type": "craft", "state": "complete", "startedAt": stamp,
                      "usage": { "inputTokens": 1000, "outputTokens": 50,
                                 "cacheTokens": 800, "cachedWriteTokens": 10 } },
                    // 同 id 重复（会话内去重）
                    { "id": "r1", "type": "craft", "state": "complete", "startedAt": stamp,
                      "usage": { "inputTokens": 1000, "outputTokens": 50 } },
                    // 进行中 → 跳过
                    { "id": "r2", "type": "plan", "state": "running", "startedAt": stamp,
                      "usage": { "inputTokens": 9, "outputTokens": 9 } },
                    // 四类 token 全 0 → 跳过
                    { "id": "r3", "type": "craft", "state": "complete", "startedAt": stamp,
                      "usage": { "inputTokens": 0, "outputTokens": 0 } },
                    // canceled 仍消费 → 计入（plan 走 modelMap.plan）
                    { "id": "r4", "type": "plan", "state": "canceled", "startedAt": stamp,
                      "usage": { "inputTokens": 7, "outputTokens": 0 } }
                ]
            })
            .to_string(),
        )
        .unwrap();

        let mut memo: HashMap<String, Value> = HashMap::new();
        let entry = parse_codebuddy_index(&session.join("index.json"), &mut memo, None);
        assert_eq!(entry.parse_errors, 0);
        // 只登记**实际计入**的 id（r1 会话内重复只算一次、r2 running 与 r3 全 0 跳过）
        assert_eq!(entry.ids.len(), 2, "应只登记 r1/r4: {:?}", entry.ids);
        assert!(entry.ids.iter().any(|id| id == "r1") && entry.ids.iter().any(|id| id == "r4"));
        let day = entry.days.get(&expected_day).expect("应有当日聚合");
        assert_eq!(day.calls, 2);
        assert_eq!(day.input, 1007);
        assert_eq!(day.output, 50);
        assert_eq!(day.read, 800);
        assert_eq!(day.write, 10);
        let craft = entry
            .by_model
            .get("deepseek-v4-flash")
            .and_then(|days| days.get(&expected_day))
            .expect("craft 请求应归到 modelMap.craft");
        assert_eq!(craft.calls, 1);
        let plan = entry
            .by_model
            .get("deepseek-v4-pro")
            .and_then(|days| days.get(&expected_day))
            .expect("plan 请求应归到 modelMap.plan");
        assert_eq!(plan.input, 7);
        let project = entry
            .by_project
            .get(CODEBUDDY_IDE_PROJECT)
            .and_then(|days| days.get(&expected_day))
            .expect("项目维度记常量");
        assert_eq!(project.calls, 2);

        // 收集器按结构只收**会话级** index.json（2026-10-07 性能优化：
        // 不再从 Data 整树递归）；工作区级索引由 conversation_model 按需直读，不进列表。
        // 产品子树不写死：`VSCode` 布局与 `CodeBuddyIDE` 同构且含真实用量，
        // 只看 CodeBuddyIDE 会静默漏计（本机实测 45 个 VSCode 会话索引中 4 个有用量）。
        let vscode_session = root
            .join(uid)
            .join("VSCode")
            .join(uid)
            .join("history")
            .join(ws)
            .join("vscode-conv");
        std::fs::create_dir_all(&vscode_session).unwrap();
        std::fs::write(
            vscode_session.join("index.json"),
            json!({ "messages": [], "requests": [] }).to_string(),
        )
        .unwrap();

        let dir_name = |p: &PathBuf| -> Option<String> {
            p.parent()
                .and_then(|d| d.file_name())
                .and_then(|n| n.to_str())
                .map(str::to_string)
        };
        let mut paths = Vec::new();
        collect_codebuddy_indexes(&root, &mut paths);
        assert_eq!(paths.len(), 2, "两套产品布局的会话级索引都应收集：{paths:?}");
        assert!(
            paths.iter().any(|p| dir_name(p).as_deref() == Some(conv)),
            "CodeBuddyIDE 会话索引应命中：{paths:?}"
        );
        assert!(
            paths.iter().any(|p| dir_name(p).as_deref() == Some("vscode-conv")),
            "VSCode 布局的会话索引也应命中：{paths:?}"
        );
        // 同结构下再次调用应稳定（stale 自愈路径不触发）
        let mut again = Vec::new();
        collect_codebuddy_indexes(&root, &mut again);
        assert_eq!(again.len(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 兜底按**产品子树**判零：某产品布局漂移（此处 history\<ws> 下多出一层）
    /// 而其余产品仍正常命中时，漂移子树由限深兜底补收、不再静默漏收，
    /// 正常子树仍走结构化快路径（评审 #1 修复的回归锁定）。
    #[test]
    fn collect_codebuddy_indexes_falls_back_per_product_on_layout_drift() {
        let root = std::env::temp_dir().join(format!("wb_stats_p4_{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let uid = "u1";
        let ws = "d41d8cd98f00b204e9800998ecf8427e";

        // VSCode 子树：正常布局（结构化下探命中）
        let vscode_ok = root
            .join(uid)
            .join("VSCode")
            .join(uid)
            .join("history")
            .join(ws)
            .join("vscode-conv");
        std::fs::create_dir_all(&vscode_ok).unwrap();
        std::fs::write(
            vscode_ok.join("index.json"),
            json!({ "messages": [], "requests": [] }).to_string(),
        )
        .unwrap();

        // CodeBuddyIDE 子树：布局漂移——history\<ws> 与会话目录之间多了一层 v2，
        // 结构化下探（只认 history\<ws>\<session>\index.json）在该子树零命中
        let drifted = root
            .join(uid)
            .join("CodeBuddyIDE")
            .join(uid)
            .join("history")
            .join(ws)
            .join("v2")
            .join("drifted-conv");
        std::fs::create_dir_all(&drifted).unwrap();
        std::fs::write(
            drifted.join("index.json"),
            json!({ "messages": [], "requests": [] }).to_string(),
        )
        .unwrap();

        let dir_name = |p: &PathBuf| -> Option<String> {
            p.parent()
                .and_then(|d| d.file_name())
                .and_then(|n| n.to_str())
                .map(str::to_string)
        };
        let mut paths = Vec::new();
        collect_codebuddy_indexes(&root, &mut paths);
        assert_eq!(
            paths.len(),
            2,
            "正常子树快路径 + 漂移子树兜底都应收集：{paths:?}"
        );
        assert!(
            paths
                .iter()
                .any(|p| dir_name(p).as_deref() == Some("vscode-conv")),
            "VSCode 正常布局应结构化命中：{paths:?}"
        );
        assert!(
            paths
                .iter()
                .any(|p| dir_name(p).as_deref() == Some("drifted-conv")),
            "CodeBuddyIDE 漂移布局应由子树级兜底补收，不静默漏收：{paths:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
