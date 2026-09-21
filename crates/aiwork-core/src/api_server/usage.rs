//! API 网关用量统计：按日聚合请求数/错误数/token 用量，落盘 `data/api_usage.json`。
//!
//! 维度：日期（本机时区）/ 模型 / 上游账号 / API Key / 流式与非流式 / 耗时。
//! 每次请求完成即原子写盘（个人使用频率低，写放大可接受）；
//! 启动时加载并裁剪超过保留期的历史数据（默认 90 天）。

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};


/// 用量数据文件名（位于 data/ 目录）
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

/// 延迟样本（F-76 TTFT 分维统计）：滚动保留最近 N 条总耗时与首字耗时样本，
/// 供用量页计算 P50/P95/最大值——均值会被少数超长请求拉偏，分位数才能区分
/// 「普遍慢」与「少数超大请求慢」。样本仅存数值（8 字节/条），量级可忽略
#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct LatencyAgg {
    /// 总耗时样本（毫秒，含失败请求）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub total_ms: Vec<u64>,
    /// 首字耗时（TTFT）样本（毫秒，仅流式成功请求）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ttfb_ms: Vec<u64>,
}

impl LatencyAgg {
    /// 追加总耗时样本（超过容量丢最旧）
    fn push_total(&mut self, ms: u64, cap: usize) {
        if self.total_ms.len() >= cap {
            self.total_ms.remove(0);
        }
        self.total_ms.push(ms);
    }

    /// 追加 TTFT 样本（超过容量丢最旧）
    fn push_ttfb(&mut self, ms: u64, cap: usize) {
        if self.ttfb_ms.len() >= cap {
            self.ttfb_ms.remove(0);
        }
        self.ttfb_ms.push(ms);
    }
}

/// 单日样本容量（天级/模型级）：个人使用频率下 512/256 条足够覆盖全天高峰
const SAMPLE_CAP_DAY: usize = 512;
const SAMPLE_CAP_MODEL: usize = 256;

/// 分位数（P50/P95 等）：空样本返回 None；索引取 ceil(p%·n)-1（最近邻上取整，
/// 与常见监控口径一致）
fn percentile(samples: &[u64], p: f64) -> Option<u64> {
    if samples.is_empty() {
        return None;
    }
    let mut v = samples.to_vec();
    v.sort_unstable();
    let idx = (((p / 100.0) * v.len() as f64).ceil() as usize).saturating_sub(1);
    Some(v[idx.min(v.len() - 1)])
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
    /// 天级延迟样本（F-76：P50/P95/最大值 + TTFT）
    #[serde(default)]
    pub latency: LatencyAgg,
    /// 按模型延迟样本（F-76：按模型分桶的分位数统计）
    #[serde(default)]
    pub model_latency: HashMap<String, LatencyAgg>,
}

impl DayStats {
    /// 记录一次请求（`ttfb_ms`：流式请求的首字耗时，非流式/未知传 None）
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
        ttfb_ms: Option<u64>,
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
        // 延迟样本（F-76）：总耗时全量采样，TTFT 仅在有值时采样
        self.latency.push_total(duration_ms, SAMPLE_CAP_DAY);
        let m = self.model_latency.entry(model.to_string()).or_default();
        m.push_total(duration_ms, SAMPLE_CAP_MODEL);
        if let Some(t) = ttfb_ms {
            self.latency.push_ttfb(t, SAMPLE_CAP_DAY);
            m.push_ttfb(t, SAMPLE_CAP_MODEL);
        }
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

    /// 按桶只读取指定日的统计（记账后持久化当日行用）
    pub fn day_stats(&self, bucket: UsageBucket, day: &str) -> Option<&DayStats> {
        let b = match bucket {
            UsageBucket::Trae => &self.days,
            UsageBucket::Wb => &self.wb_days,
            UsageBucket::Custom => &self.custom_days,
        };
        b.get(day)
    }

    /// 记录一次请求（无 TTFT 的简写，仅测试用；生产路径一律走 record_ttfb/record_in）
    #[cfg(test)]
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
        self.record_ttfb(
            is_wb, model, uid, key_id, ok, is_stream, duration_ms, prompt_tokens,
            completion_tokens, None,
        );
    }

    /// 记录一次请求（带 TTFT，F-76）：流式路径已知首字耗时时使用
    #[allow(clippy::too_many_arguments)]
    pub fn record_ttfb(
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
        ttfb_ms: Option<u64>,
    ) {
        self.record_in_ttfb(
            if is_wb { UsageBucket::Wb } else { UsageBucket::Trae },
            model, uid, key_id, ok, is_stream, duration_ms, prompt_tokens,
            completion_tokens, ttfb_ms,
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
        self.record_in_ttfb(
            bucket, model, uid, key_id, ok, is_stream, duration_ms, prompt_tokens,
            completion_tokens, None,
        );
    }

    /// 按桶记录一次请求（带 TTFT，F-76）
    #[allow(clippy::too_many_arguments)]
    pub fn record_in_ttfb(
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
        ttfb_ms: Option<u64>,
    ) {
        let day = self.bucket_mut(bucket).entry(today_key()).or_default();
        day.record(
            model, uid, key_id, ok, is_stream, duration_ms, prompt_tokens,
            completion_tokens, ttfb_ms,
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

/// 从存储加载用量数据（缺失/损坏回退空结构），并裁剪过期日期。
/// SQLite 化 P3：api_usage 表；P7 修订：记账改为当日单行 upsert 后，
/// 存储侧保留期裁剪在启动 load 时一次性执行。
pub fn load(data_dir: &Path) -> UsageFile {
    let mut f = crate::store::docs::api_usage_load(&crate::store::db(data_dir));
    f.trim(RETENTION_DAYS);
    let _ = crate::store::docs::api_usage_prune(&crate::store::db(data_dir), RETENTION_DAYS);
    f
}

/// 持久化当日单行（flusher 记账削峰路径：单行 UPSERT 替代原整表 DELETE+重插）。
/// 内存 `UsageFile`（RuntimeState.usage）为权威态，启动时由 load 全量回读。
/// 返回落盘是否成功（失败由调用方恢复脏标记重试，R3）
pub fn save_day(data_dir: &Path, bucket: UsageBucket, day: &str, stats: &DayStats) -> bool {
    let text = match serde_json::to_string(stats) {
        Ok(t) => t,
        Err(_) => return false,
    };
    let b = match bucket {
        UsageBucket::Trae => "trae",
        UsageBucket::Wb => "wb",
        UsageBucket::Custom => "custom",
    };
    crate::store::docs::api_usage_upsert_day(&crate::store::db(data_dir), b, day, &text).is_ok()
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

/// 按模型的延迟分位视图（F-76：P50/P95/最大值 + TTFT 分桶）
#[derive(Serialize, Clone)]
pub struct ModelLatencyView {
    pub model: String,
    /// 样本数（总耗时）
    pub samples: usize,
    pub p50_duration_ms: Option<u64>,
    pub p95_duration_ms: Option<u64>,
    pub max_duration_ms: Option<u64>,
    pub avg_ttfb_ms: Option<u64>,
    pub p95_ttfb_ms: Option<u64>,
}

impl ModelLatencyView {
    fn from_agg(model: &str, agg: &LatencyAgg) -> Self {
        Self {
            model: model.to_string(),
            samples: agg.total_ms.len(),
            p50_duration_ms: percentile(&agg.total_ms, 50.0),
            p95_duration_ms: percentile(&agg.total_ms, 95.0),
            max_duration_ms: agg.total_ms.iter().copied().max(),
            avg_ttfb_ms: if agg.ttfb_ms.is_empty() {
                None
            } else {
                Some(agg.ttfb_ms.iter().sum::<u64>() / agg.ttfb_ms.len() as u64)
            },
            p95_ttfb_ms: percentile(&agg.ttfb_ms, 95.0),
        }
    }
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
    /// 总耗时 P50/P95/最大值（F-76；样本不足时为 None）
    pub p50_duration_ms: Option<u64>,
    pub p95_duration_ms: Option<u64>,
    pub max_duration_ms: Option<u64>,
    /// 首字延迟（TTFT）均值 / P95 / 样本数（F-76；仅流式成功请求有样本）
    pub avg_ttfb_ms: Option<u64>,
    pub p95_ttfb_ms: Option<u64>,
    pub ttfb_samples: usize,
    pub models: Vec<CounterView>,
    pub accounts: Vec<CounterView>,
    pub keys: Vec<CounterView>,
    /// 按 Key 的 token 用量（与 keys 对应）
    pub key_tokens: Vec<KeyTokenView>,
    /// 按模型的延迟分位（F-76，按请求数降序）
    pub model_latency: Vec<ModelLatencyView>,
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
        let mut model_latency: Vec<ModelLatencyView> = d
            .model_latency
            .iter()
            .map(|(k, agg)| ModelLatencyView::from_agg(k, agg))
            .collect();
        model_latency.sort_by(|a, b| b.samples.cmp(&a.samples).then(a.model.cmp(&b.model)));
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
            p50_duration_ms: percentile(&d.latency.total_ms, 50.0),
            p95_duration_ms: percentile(&d.latency.total_ms, 95.0),
            max_duration_ms: d.latency.total_ms.iter().copied().max(),
            avg_ttfb_ms: if d.latency.ttfb_ms.is_empty() {
                None
            } else {
                Some(d.latency.ttfb_ms.iter().sum::<u64>() / d.latency.ttfb_ms.len() as u64)
            },
            p95_ttfb_ms: percentile(&d.latency.ttfb_ms, 95.0),
            ttfb_samples: d.latency.ttfb_ms.len(),
            models,
            accounts,
            keys,
            key_tokens,
            model_latency,
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
    fn latency_percentiles_and_ttfb() {
        let mut f = UsageFile::default();
        // 5 个请求：100/200/300/400/5000ms（5000 模拟少数超大请求拉偏均值）；
        // 前两个流式请求带 TTFT 80/120ms
        f.record_ttfb(false, "m1", "u1", "k", true, true, 100, 0, 0, Some(80));
        f.record_ttfb(false, "m1", "u1", "k", true, true, 200, 0, 0, Some(120));
        f.record(false, "m1", "u1", "k", true, true, 300, 0, 0);
        f.record(false, "m1", "u1", "k", true, true, 400, 0, 0);
        f.record(false, "m1", "u1", "k", true, true, 5000, 0, 0);
        let today = today_key();
        let d = f.days.get(&today).expect("当日统计应存在");
        assert_eq!(d.latency.total_ms.len(), 5);
        assert_eq!(d.latency.ttfb_ms, vec![80, 120]);
        assert_eq!(d.model_latency.get("m1").unwrap().total_ms.len(), 5);
        let view = UsageDayView::from_day(&today, d);
        // 排序后 [100,200,300,400,5000]：P50=300（ceil(0.5*5)=3 → 索引 2），
        // P95=5000（ceil(0.95*5)=5 → 索引 4）
        assert_eq!(view.p50_duration_ms, Some(300));
        assert_eq!(view.p95_duration_ms, Some(5000));
        assert_eq!(view.max_duration_ms, Some(5000));
        assert_eq!(view.avg_ttfb_ms, Some(100));
        assert_eq!(view.p95_ttfb_ms, Some(120));
        assert_eq!(view.ttfb_samples, 2);
        let ml = view
            .model_latency
            .iter()
            .find(|m| m.model == "m1")
            .expect("模型延迟分桶应存在");
        assert_eq!(ml.p50_duration_ms, Some(300));
        assert_eq!(ml.p95_ttfb_ms, Some(120));
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
        let day = today_key();
        let stats = f.day_stats(UsageBucket::Trae, &day).unwrap().clone();
        save_day(&dir, UsageBucket::Trae, &day, &stats);
        let loaded = load(&dir);
        let d = loaded.days.get(&day).expect("应能读回当日数据");
        assert_eq!(d.total.requests, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// save_day 单行 upsert 后，启动 load 应裁剪保留期之外的存储行
    #[test]
    fn load_prunes_rows_beyond_retention() {
        let dir = std::env::temp_dir().join(format!("twa_usage_prune_{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("data"));
        // 种一条 91 天前的旧行 + 当日行
        let old_day = (chrono::Local::now().date_naive() - chrono::Duration::days(RETENTION_DAYS + 1))
            .format("%Y-%m-%d")
            .to_string();
        let today = today_key();
        save_day(&dir, UsageBucket::Trae, &old_day, &DayStats::default());
        save_day(&dir, UsageBucket::Trae, &today, &DayStats::default());
        let loaded = load(&dir);
        assert!(loaded.days.contains_key(&today), "当日行应保留");
        assert!(!loaded.days.contains_key(&old_day), "保留期外行应被裁剪");
        // 存储侧确认已删（下次 load 不再读回）
        let again = load(&dir);
        assert!(!again.days.contains_key(&old_day));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
