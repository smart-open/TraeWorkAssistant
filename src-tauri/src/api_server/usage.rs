//! API 网关用量统计：按日聚合请求数/错误数/token 用量，落盘 `data/api_usage.json`。
//!
//! 维度：日期（本机时区）/ 模型 / 上游账号 / API Key / 流式与非流式 / 耗时。
//! 每次请求完成即原子写盘（个人使用频率低，写放大可接受）；
//! 启动时加载并裁剪超过保留期的历史数据（默认 90 天）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fs_utils;

/// 用量数据文件名（位于 data/ 目录）
pub const USAGE_FILE: &str = "api_usage.json";
/// 历史数据保留天数（超出部分启动时裁剪）
pub const RETENTION_DAYS: i64 = 90;

/// 请求命中的 API Key 标识（鉴权中间件解析后插入 request extensions，
/// handler 取出用于按 Key 维度记账；服务未启用鉴权时为 "anonymous"）
#[derive(Clone, Debug)]
pub struct KeyId(pub String);

/// 从上游 token_usage 事件中提取 (prompt_tokens, completion_tokens)，
/// 兼容 OpenAI / Anthropic 两种字段命名；缺失时返回 0
pub fn extract_tokens(u: &serde_json::Value) -> (u64, u64) {
    let get = |keys: &[&str]| -> u64 {
        keys.iter()
            .find_map(|k| u.get(*k).and_then(|v| v.as_u64()))
            .unwrap_or(0)
    };
    (
        get(&["prompt_tokens", "input_tokens"]),
        get(&["completion_tokens", "output_tokens"]),
    )
}

/// 通用计数器：请求数 / 成功数 / 失败数
#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
pub struct Counter {
    #[serde(default)]
    pub requests: u64,
    #[serde(default)]
    pub ok: u64,
    #[serde(default)]
    pub errors: u64,
}

impl Counter {
    /// 记一次请求（ok 决定计入成功或失败）
    pub fn add(&mut self, ok: bool) {
        self.requests += 1;
        if ok {
            self.ok += 1;
        } else {
            self.errors += 1;
        }
    }
}

/// 单日统计：汇总 + 分维度计数
#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct DayStats {
    #[serde(default)]
    pub total: Counter,
    /// 流式请求数（含其中的失败数）
    #[serde(default)]
    pub stream: Counter,
    /// 非流式请求数
    #[serde(default)]
    pub non_stream: Counter,
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    /// 累计耗时（毫秒），用于计算平均耗时
    #[serde(default)]
    pub duration_ms_total: u64,
    /// 按模型统计
    #[serde(default)]
    pub models: HashMap<String, Counter>,
    /// 按上游账号统计（"none" 表示未取到账号，如无健康账号）
    #[serde(default)]
    pub accounts: HashMap<String, Counter>,
    /// 按 API Key 统计（"anonymous" 表示服务未启用鉴权）
    #[serde(default)]
    pub keys: HashMap<String, Counter>,
    /// 按 API Key 统计 token 用量：(prompt, completion)
    #[serde(default)]
    pub key_tokens: HashMap<String, (u64, u64)>,
}

impl DayStats {
    /// 记录一次请求
    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &mut self,
        model: &str,
        uid: &str,
        key_id: &str,
        ok: bool,
        is_stream: bool,
        duration_ms: u64,
        prompt_tokens: u64,
        completion_tokens: u64,
    ) {
        self.total.add(ok);
        if is_stream {
            self.stream.add(ok);
        } else {
            self.non_stream.add(ok);
        }
        self.prompt_tokens += prompt_tokens;
        self.completion_tokens += completion_tokens;
        self.duration_ms_total += duration_ms;
        self.models.entry(model.to_string()).or_default().add(ok);
        self.accounts.entry(uid.to_string()).or_default().add(ok);
        self.keys.entry(key_id.to_string()).or_default().add(ok);
        // 按 Key 的 token 用量记账（审查修复：原实现（含参考分支）遗漏此写入，
        // 导致前端 Key 表「今日已用(次/tok)」的 token 部分恒为空）
        {
            let kt = self.key_tokens.entry(key_id.to_string()).or_insert((0, 0));
            kt.0 += prompt_tokens;
            kt.1 += completion_tokens;
        }
    }
}

/// 用量分桶（资源池维度分账）：Trae = Trae 模型请求；Wb = WB 上游请求；
/// Custom = 自定义模型（custom_models.json 命中直达）请求
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageBucket {
    Trae,
    Wb,
    Custom,
}

/// 用量数据根结构：日期 → 单日统计。
/// 按资源池分桶：days = Trae 模型请求；wb_days = WB 上游请求；custom_days = 自定义模型请求
/// （serde default，旧文件无对应桶时视为空——历史混入数据无法追溯分离，从启用时点起分账）。
#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct UsageFile {
    #[serde(default)]
    pub days: HashMap<String, DayStats>,
    #[serde(default)]
    pub wb_days: HashMap<String, DayStats>,
    #[serde(default)]
    pub custom_days: HashMap<String, DayStats>,
}

impl UsageFile {
    fn bucket_mut(&mut self, b: UsageBucket) -> &mut HashMap<String, DayStats> {
        match b {
            UsageBucket::Trae => &mut self.days,
            UsageBucket::Wb => &mut self.wb_days,
            UsageBucket::Custom => &mut self.custom_days,
        }
    }

    /// 记录一次请求并返回是否需要写盘（总是 true，留给调用方统一处理）
    /// `is_wb`：WB 上游路由的请求记入 wb_days 桶，与 Trae 侧分账
    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &mut self,
        is_wb: bool,
        model: &str,
        uid: &str,
        key_id: &str,
        ok: bool,
        is_stream: bool,
        duration_ms: u64,
        prompt_tokens: u64,
        completion_tokens: u64,
    ) {
        self.record_in(
            if is_wb { UsageBucket::Wb } else { UsageBucket::Trae },
            model, uid, key_id, ok, is_stream, duration_ms, prompt_tokens, completion_tokens,
        );
    }

    /// 按桶记录一次请求（custom_route 自定义池路径使用）
    #[allow(clippy::too_many_arguments)]
    pub fn record_in(
        &mut self,
        bucket: UsageBucket,
        model: &str,
        uid: &str,
        key_id: &str,
        ok: bool,
        is_stream: bool,
        duration_ms: u64,
        prompt_tokens: u64,
        completion_tokens: u64,
    ) {
        let day = self.bucket_mut(bucket).entry(today_key()).or_default();
        day.record(
            model, uid, key_id, ok, is_stream, duration_ms, prompt_tokens, completion_tokens,
        );
    }

    /// 裁剪保留期之外的历史日期（各桶同规则）
    pub fn trim(&mut self, keep_days: i64) {
        let cutoff = chrono::Local::now().date_naive() - chrono::Duration::days(keep_days);
        let cutoff_str = cutoff.format("%Y-%m-%d").to_string();
        for bucket in [&mut self.days, &mut self.wb_days, &mut self.custom_days] {
            bucket.retain(|d, _| d.as_str() >= cutoff_str.as_str());
        }
    }

    /// 按桶取最近 N 天（含今日），不足 N 天只返回已有的；按日期升序
    pub fn recent_in(&self, days: u32, bucket: UsageBucket) -> Vec<(&String, &DayStats)> {
        let b = match bucket {
            UsageBucket::Trae => &self.days,
            UsageBucket::Wb => &self.wb_days,
            UsageBucket::Custom => &self.custom_days,
        };
        let mut sorted: Vec<(&String, &DayStats)> = b.iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(b.0));
        let skip = sorted.len().saturating_sub(days as usize);
        sorted.into_iter().skip(skip).collect()
    }
}

/// 当日日期键（本机时区，YYYY-MM-DD）
pub fn today_key() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// 用量文件路径：data_dir/data/api_usage.json
pub fn usage_path(data_dir: &Path) -> PathBuf {
    let dir = data_dir.join("data");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(USAGE_FILE)
}

/// 从磁盘加载用量数据（缺失/损坏回退空结构）
pub fn load(data_dir: &Path) -> UsageFile {
    let mut f: UsageFile = fs_utils::read_json(&usage_path(data_dir));
    f.trim(RETENTION_DAYS);
    f
}

/// 原子写盘
pub fn save(data_dir: &Path, usage: &UsageFile) {
    let _ = fs_utils::write_json(&usage_path(data_dir), usage);
}

// ==================== 命令返回结构 ====================

/// 计数视图（前端友好：具名字段）
#[derive(Serialize, Clone)]
pub struct CounterView {
    pub name: String,
    pub requests: u64,
    pub ok: u64,
    pub errors: u64,
}

/// 按 Key 的 token 用量视图
#[derive(Serialize, Clone)]
pub struct KeyTokenView {
    pub name: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

/// 单日统计视图
#[derive(Serialize, Clone)]
pub struct UsageDayView {
    pub date: String,
    pub total_requests: u64,
    pub ok: u64,
    pub errors: u64,
    pub stream_requests: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// 平均耗时（毫秒），无请求时为 0
    pub avg_duration_ms: u64,
    pub models: Vec<CounterView>,
    pub accounts: Vec<CounterView>,
    pub keys: Vec<CounterView>,
    /// 按 Key 的 token 用量（与 keys 对应）
    pub key_tokens: Vec<KeyTokenView>,
}

impl UsageDayView {
    fn from_day(date: &str, d: &DayStats) -> Self {
        let mut models: Vec<CounterView> = d
            .models
            .iter()
            .map(|(k, c)| view(k, c))
            .collect();
        models.sort_by(|a, b| b.requests.cmp(&a.requests).then(a.name.cmp(&b.name)));
        let mut accounts: Vec<CounterView> = d.accounts.iter().map(|(k, c)| view(k, c)).collect();
        accounts.sort_by(|a, b| b.requests.cmp(&a.requests).then(a.name.cmp(&b.name)));
        let mut keys: Vec<CounterView> = d.keys.iter().map(|(k, c)| view(k, c)).collect();
        keys.sort_by(|a, b| b.requests.cmp(&a.requests).then(a.name.cmp(&b.name)));
        let mut key_tokens: Vec<KeyTokenView> = d
            .key_tokens
            .iter()
            .map(|(k, (p, c))| KeyTokenView {
                name: k.clone(),
                prompt_tokens: *p,
                completion_tokens: *c,
            })
            .collect();
        key_tokens.sort_by(|a, b| {
            (b.prompt_tokens + b.completion_tokens)
                .cmp(&(a.prompt_tokens + a.completion_tokens))
                .then(a.name.cmp(&b.name))
        });
        Self {
            date: date.to_string(),
            total_requests: d.total.requests,
            ok: d.total.ok,
            errors: d.total.errors,
            stream_requests: d.stream.requests,
            prompt_tokens: d.prompt_tokens,
            completion_tokens: d.completion_tokens,
            avg_duration_ms: if d.total.requests > 0 {
                d.duration_ms_total / d.total.requests as u64
            } else {
                0
            },
            models,
            accounts,
            keys,
            key_tokens,
        }
    }
}

fn view(name: &str, c: &Counter) -> CounterView {
    CounterView {
        name: name.to_string(),
        requests: c.requests,
        ok: c.ok,
        errors: c.errors,
    }
}

/// 查询最近 N 天统计（按日期升序），供 `api_usage_stats` / `api_wb_usage_stats` 命令使用。
/// `is_wb`：查 WB 上游桶（Buddy 页）；false 查 Trae 桶（Trae 页）。
/// 直接读盘，服务未运行时也可查询。
pub fn query_recent(data_dir: &Path, days: u32, is_wb: bool) -> Vec<UsageDayView> {
    query_recent_in(
        data_dir,
        days,
        if is_wb { UsageBucket::Wb } else { UsageBucket::Trae },
    )
}

/// 按桶查询最近 N 天统计（`api_custom_usage_stats` 命令使用）
pub fn query_recent_in(data_dir: &Path, days: u32, bucket: UsageBucket) -> Vec<UsageDayView> {
    let mut usage: UsageFile = load(data_dir);
    usage.trim(RETENTION_DAYS);
    usage
        .recent_in(days, bucket)
        .into_iter()
        .map(|(date, d)| UsageDayView::from_day(date, d))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_aggregates_dimensions() {
        let mut f = UsageFile::default();
        f.record(false, "m1", "u1", "master", true, true, 100, 10, 20);
        f.record(false, "m1", "u1", "master", false, true, 300, 0, 0);
        f.record(false, "m2", "u2", "k2", true, false, 50, 5, 8);
        let today = today_key();
        let d = f.days.get(&today).expect("当日统计应存在");
        assert_eq!(d.total.requests, 3);
        assert_eq!(d.total.ok, 2);
        assert_eq!(d.total.errors, 1);
        assert_eq!(d.stream.requests, 2);
        assert_eq!(d.non_stream.requests, 1);
        assert_eq!(d.prompt_tokens, 15);
        assert_eq!(d.completion_tokens, 28);
        assert_eq!(d.models.get("m1").unwrap().requests, 2);
        assert_eq!(d.models.get("m1").unwrap().errors, 1);
        assert_eq!(d.accounts.get("u2").unwrap().requests, 1);
        assert_eq!(d.keys.get("master").unwrap().requests, 2);
        assert_eq!(d.keys.get("k2").unwrap().requests, 1);
        // 按 Key 的 token 用量记账（m1/u1/master 两请求：10+0 prompt、20+0 completion）
        assert_eq!(d.key_tokens.get("master"), Some(&(10, 20)));
        assert_eq!(d.key_tokens.get("k2"), Some(&(5, 8)));
        // 平均耗时 = (100+300+50)/3 = 150
        let view = UsageDayView::from_day(&today, d);
        assert_eq!(view.avg_duration_ms, 150);
    }

    #[test]
    fn wb_requests_go_to_separate_bucket() {
        let mut f = UsageFile::default();
        f.record(false, "glm-5.3", "u1", "k", true, true, 100, 10, 20);
        f.record(true, "hy4", "wb-1", "k", true, true, 100, 7, 9);
        let today = today_key();
        // Trae 桶只有 Trae 请求；WB 请求进 wb_days 桶
        let trae = f.days.get(&today).expect("Trae 当日统计应存在");
        assert_eq!(trae.total.requests, 1);
        assert_eq!(trae.models.get("glm-5.3").unwrap().requests, 1);
        let wb = f.wb_days.get(&today).expect("WB 当日统计应存在");
        assert_eq!(wb.total.requests, 1);
        assert_eq!(wb.models.get("hy4").unwrap().requests, 1);
        assert_eq!(f.recent_in(7, UsageBucket::Trae).len(), 1);
        assert_eq!(f.recent_in(7, UsageBucket::Wb).len(), 1);
    }

    #[test]
    fn custom_requests_go_to_custom_bucket() {
        let mut f = UsageFile::default();
        f.record(false, "glm-5.3", "u1", "k", true, true, 100, 10, 20);
        f.record(true, "hy4", "wb-1", "k", true, true, 90, 7, 8);
        f.record_in(
            UsageBucket::Custom, "my-model", "custom", "k", true, true, 80, 5, 6,
        );
        let today = today_key();
        // 三桶各记各的：custom 请求不串入 Trae/WB 桶
        assert_eq!(f.days.get(&today).map(|d| d.total.requests), Some(1));
        assert_eq!(f.wb_days.get(&today).map(|d| d.total.requests), Some(1));
        let custom = f.custom_days.get(&today).expect("自定义当日统计应存在");
        assert_eq!(custom.total.requests, 1);
        assert_eq!(custom.models.get("my-model").unwrap().requests, 1);
        assert_eq!(custom.accounts.get("custom").unwrap().requests, 1);
        assert_eq!(f.recent_in(7, UsageBucket::Custom).len(), 1);
        assert_eq!(f.recent_in(7, UsageBucket::Trae).len(), 1);
        assert_eq!(f.recent_in(7, UsageBucket::Wb).len(), 1);
        // 裁剪覆盖 custom_days
        let old = (chrono::Local::now().date_naive() - chrono::Duration::days(120))
            .format("%Y-%m-%d")
            .to_string();
        f.custom_days.insert(old, DayStats::default());
        f.trim(RETENTION_DAYS);
        assert_eq!(f.custom_days.len(), 1);
    }

    #[test]
    fn trim_drops_old_days() {
        let mut f = UsageFile::default();
        let old = (chrono::Local::now().date_naive() - chrono::Duration::days(120))
            .format("%Y-%m-%d")
            .to_string();
        f.days.insert(old.clone(), DayStats::default());
        f.days.insert(today_key(), DayStats::default());
        f.wb_days.insert(old, DayStats::default());
        f.wb_days.insert(today_key(), DayStats::default());
        f.trim(RETENTION_DAYS);
        assert_eq!(f.days.len(), 1);
        assert!(f.days.contains_key(&today_key()));
        assert_eq!(f.wb_days.len(), 1);
    }

    #[test]
    fn recent_days_sorted_and_limited() {
        let mut f = UsageFile::default();
        for i in 1..=5 {
            let d = (chrono::Local::now().date_naive() - chrono::Duration::days(i))
                .format("%Y-%m-%d")
                .to_string();
            f.days.insert(d, DayStats::default());
        }
        f.days.insert(today_key(), DayStats::default());
        let got = f.recent_in(3, UsageBucket::Trae);
        assert_eq!(got.len(), 3);
        // 升序：最后一天应为今日
        assert_eq!(got.last().unwrap().0.as_str(), today_key().as_str());
        // 第一天应早于倒数第二天
        assert!(got[0].0 < got[1].0);
    }

    #[test]
    fn usage_file_roundtrip() {
        let dir = std::env::temp_dir().join(format!("twa_usage_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("data"));
        let mut f = UsageFile::default();
        f.record(false, "m", "u", "k", true, false, 10, 1, 2);
        save(&dir, &f);
        let loaded = load(&dir);
        let d = loaded.days.get(&today_key()).expect("应能读回当日数据");
        assert_eq!(d.total.requests, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
