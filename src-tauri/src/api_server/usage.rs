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
/// 兼容 OpenAI / Anthropic 两种字段命名；缺失时返回 0。
/// M2 修复（审查）：as_u64 窄口径对字符串/浮点数字段形态（"123"、123.0）静默
/// 得 0——委托 wb_sse::u64_pair 宽口径（u64 → i64 → f64 非负 → 字符串 parse），
/// 别名键集一致，签名不变
pub fn extract_tokens(u: &serde_json::Value) -> (u64, u64) {
    super::wb_sse::u64_pair(u)
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

/// 桶键（审查 Low 修复）：模型名来自请求体（客户端可控），截断防键空间放大
fn bucket_key(s: &str) -> String {
    s.chars().take(200).collect()
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
    /// 缓存读 tokens 累计（M4 审查修复：上游 usage 的 cache_read_input_tokens
    /// 等别名宽口径提取；上游未下发不入账，与 credits「不得伪造」口径一致）
    #[serde(default)]
    pub cached_tokens: u64,
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
    /// 网关测量的累计消耗积分（issue #82 T2：Qoder 上游 usage.credits 求和；
    /// 上游未下发 usage 的请求不入账）
    #[serde(default)]
    pub credits: f64,
    /// 上游计费原值（original_credits，峰谷折扣前），用于对账
    #[serde(default)]
    pub original_credits: f64,
    /// 按模型的积分测量（issue #82 T2.4：仅 Qoder 桶非零，供积分页模型排行）
    #[serde(default)]
    pub model_credits: HashMap<String, f64>,
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
        // 审查 Low 修复：u64 累加一律 saturating——恶意/异常上游回传超大 usage
        // 累加溢出会回绕出小值，统计失真且难察觉
        self.prompt_tokens = self.prompt_tokens.saturating_add(prompt_tokens);
        self.completion_tokens = self.completion_tokens.saturating_add(completion_tokens);
        self.duration_ms_total = self.duration_ms_total.saturating_add(duration_ms);
        // 审查 Low 修复：模型名来自请求体（客户端可控），无上限会撑爆 HashMap
        // 键空间（每请求一个新键）——截断作桶键，防键空间放大
        let model_key = bucket_key(model);
        self.models.entry(model_key.clone()).or_default().add(ok);
        self.accounts.entry(uid.to_string()).or_default().add(ok);
        self.keys.entry(key_id.to_string()).or_default().add(ok);
        // 延迟样本（F-76）：总耗时全量采样，TTFT 仅在有值时采样
        self.latency.push_total(duration_ms, SAMPLE_CAP_DAY);
        let m = self.model_latency.entry(model_key).or_default();
        m.push_total(duration_ms, SAMPLE_CAP_MODEL);
        if let Some(t) = ttfb_ms {
            self.latency.push_ttfb(t, SAMPLE_CAP_DAY);
            m.push_ttfb(t, SAMPLE_CAP_MODEL);
        }
        // 按 Key 的 token 用量记账（审查修复：原实现（含参考分支）遗漏此写入，
        // 导致前端 Key 表「今日已用(次/tok)」的 token 部分恒为空）
        {
            let kt = self.key_tokens.entry(key_id.to_string()).or_insert((0, 0));
            kt.0 = kt.0.saturating_add(prompt_tokens);
            kt.1 = kt.1.saturating_add(completion_tokens);
        }
    }

    /// 记录一次积分测量（issue #82 T2：仅 Qoder 网关路径使用）。
    /// 任一值缺失（上游未下发 usage）则不入账——与「上游未下发 usage 不得伪造」口径一致；
    /// 负值防御性截断为 0；有值时按模型累加（T2.4 模型粒度排行数据源）。
    /// 截断后为 0 的值不入模型榜（负值防御 + 上游真实 0 消耗都不产生 0 值假条目）。
    pub fn record_credits(&mut self, model: &str, credits: Option<f64>, original: Option<f64>) {
        if let Some(c) = credits {
            let c = c.max(0.0);
            self.credits += c;
            if c > 0.0 {
                *self.model_credits.entry(bucket_key(model)).or_default() += c;
            }
        }
        if let Some(o) = original {
            self.original_credits += o.max(0.0);
        }
    }

    /// 记录一次缓存读 tokens 测量（M4 审查修复）：与 record 分离的旁路入口——
    /// record/record_usage 系列签名被 15+ 调用点依赖，不扩参；上游未下发
    /// usage 的调用方不调用本方法（不伪造）
    pub fn record_cached(&mut self, cached: u64) {
        self.cached_tokens = self.cached_tokens.saturating_add(cached);
    }
}

/// 用量分桶（资源池维度分账）：Trae = Trae 模型请求；Wb = WB 上游请求；
/// Custom = 自定义模型（custom_models.json 上游组，统一调度尾部兜底源）请求；
/// Qoder = Qoder 上游请求；
/// BuddyAi = Buddy 国际版（buddy-ai 独立池）请求（§3.1 记账分区，与国内混计禁止）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UsageBucket {
    Trae,
    Wb,
    Custom,
    Qoder,
    BuddyAi,
}

/// 用量数据根结构：日期 → 单日统计。
/// 按资源池分桶：days = Trae 模型请求；wb_days = WB 上游请求；custom_days = 自定义模型请求；
/// qoder_days = Qoder 上游请求；buddy_ai_days = buddy-ai 国际池请求
/// （serde default，旧文件无对应桶时视为空——历史混入数据无法追溯分离，从启用时点起分账）。
#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct UsageFile {
    #[serde(default)]
    pub days: HashMap<String, DayStats>,
    #[serde(default)]
    pub wb_days: HashMap<String, DayStats>,
    #[serde(default)]
    pub custom_days: HashMap<String, DayStats>,
    #[serde(default)]
    pub qoder_days: HashMap<String, DayStats>,
    #[serde(default)]
    pub buddy_ai_days: HashMap<String, DayStats>,
}

impl UsageFile {
    fn bucket_mut(&mut self, b: UsageBucket) -> &mut HashMap<String, DayStats> {
        match b {
            UsageBucket::Trae => &mut self.days,
            UsageBucket::Wb => &mut self.wb_days,
            UsageBucket::Custom => &mut self.custom_days,
            UsageBucket::Qoder => &mut self.qoder_days,
            UsageBucket::BuddyAi => &mut self.buddy_ai_days,
        }
    }

    /// 按桶只读取指定日的统计（记账后持久化当日行用）
    pub fn day_stats(&self, bucket: UsageBucket, day: &str) -> Option<&DayStats> {
        let b = match bucket {
            UsageBucket::Trae => &self.days,
            UsageBucket::Wb => &self.wb_days,
            UsageBucket::Custom => &self.custom_days,
            UsageBucket::Qoder => &self.qoder_days,
            UsageBucket::BuddyAi => &self.buddy_ai_days,
        };
        b.get(day)
    }

    /// 记录一次请求（无 TTFT 的简写，仅测试用；生产路径一律走 record_in_ttfb_on）
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

    /// 记录一次请求（带 TTFT，F-76）：流式路径已知首字耗时时使用。
    /// 仅测试链路可达（生产路径入口取定 day 直走 record_in_ttfb_on，见 G3）
    #[cfg(test)]
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

    /// 按桶记录一次请求（custom_route 自定义池路径使用；仅测试链路可达，同上）
    #[cfg(test)]
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

    /// 按桶记录一次请求（带 TTFT，F-76；仅测试链路可达，同上）
    #[cfg(test)]
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
        self.record_in_ttfb_on(bucket, &today_key(), model, uid, key_id, ok, is_stream, duration_ms, prompt_tokens, completion_tokens, ttfb_ms);
    }

    /// 指定日期入账版（审查修复清单 v1.4.9 G3）：`day` 由调用方取定——
    /// record_usage_qoder 在入口处取定 day 后，积分/流水/请求统计（本函数）
    /// 落同一日期行，杜绝请求处理跨零点时 ttfb 统计自行重取日期分裂到次日行
    #[allow(clippy::too_many_arguments)]
    pub fn record_in_ttfb_on(
        &mut self,
        bucket: UsageBucket,
        day: &str,
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
        let day = self.bucket_mut(bucket).entry(day.to_string()).or_default();
        day.record(
            model, uid, key_id, ok, is_stream, duration_ms, prompt_tokens,
            completion_tokens, ttfb_ms,
        );
    }

    /// 记录一次积分测量到 Qoder 桶当日 entry（issue #82 T2；
    /// `day` 由调用方在持有 usage 锁期间取定，保证与请求统计落同一 entry）
    pub fn record_credits_qoder(&mut self, model: &str, day: &str, credits: Option<f64>, original: Option<f64>) {
        let d = self.qoder_days.entry(day.to_string()).or_default();
        d.record_credits(model, credits, original);
    }

    /// 按桶记录一次缓存读 tokens 测量到指定日期 entry（M4 审查修复：
    /// DayStats.cached_tokens 维度的旁路入口，不扩 record 系列签名）。
    /// 审查修复（用量跨日归行）：`day` 由调用方在持锁期间取定并传入，
    /// 写入路径内不再自行取 today——杜绝跨零点时 cached_tokens 与调用方
    /// 脏标记/同一请求其他统计落盘行分裂到不同日期行、旧行修改永不被
    /// flush（重启丢账；对齐 record_in_ttfb_on 的「调用方定日」语义）
    pub fn record_cached_in(&mut self, bucket: UsageBucket, day: &str, cached: u64) {
        self.bucket_mut(bucket)
            .entry(day.to_string())
            .or_default()
            .record_cached(cached);
    }

    /// 裁剪保留期之外的历史日期（各桶同规则）
    pub fn trim(&mut self, keep_days: i64) {
        let cutoff = chrono::Local::now().date_naive() - chrono::Duration::days(keep_days);
        let cutoff_str = cutoff.format("%Y-%m-%d").to_string();
        for bucket in [&mut self.days, &mut self.wb_days, &mut self.custom_days, &mut self.qoder_days, &mut self.buddy_ai_days] {
            bucket.retain(|d, _| d.as_str() >= cutoff_str.as_str());
        }
    }

    /// 按桶取最近 N 个日历天（含今日）窗口内有数据的日期；按日期升序。
    /// 审查修复（用量统计天数选择不对应）：原实现按「有条目的天数」计数
    /// （skip = len - days）——数据稀疏时「最近 7 天」会把跨月的历史条目也
    /// 收进来（7 条 ≠ 7 天）；改为日历窗口过滤，7/14/30 天选择与图表严格对应
    pub fn recent_in(&self, days: u32, bucket: UsageBucket) -> Vec<(&String, &DayStats)> {
        let b = match bucket {
            UsageBucket::Trae => &self.days,
            UsageBucket::Wb => &self.wb_days,
            UsageBucket::Custom => &self.custom_days,
            UsageBucket::Qoder => &self.qoder_days,
            UsageBucket::BuddyAi => &self.buddy_ai_days,
        };
        // 窗口含今日：days=7 → 今日与此前 6 个日历天（字符串比较口径对齐 trim）
        let cutoff = (chrono::Local::now().date_naive()
            - chrono::Duration::days(days.saturating_sub(1) as i64))
            .format("%Y-%m-%d")
            .to_string();
        let mut sorted: Vec<(&String, &DayStats)> = b
            .iter()
            .filter(|(d, _)| d.as_str() >= cutoff.as_str())
            .collect();
        sorted.sort_by(|a, b| a.0.cmp(b.0));
        sorted
    }
}

/// 当日日期键（本机时区，YYYY-MM-DD）
pub fn today_key() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// Qoder 请求级会话上下文（issue #82 问题5：qoder_sessions 流水行）。
/// request_id 为幂等键——成功路径取自上游信封（converted JSON 顶层），
/// 失败早退点（无 converted）以 uuid 占位；ts = Unix 秒；preview = 末条 user 消息截断。
pub struct QoderSessionCtx {
    pub request_id: String,
    pub session_id: String,
    /// 轮键（信封 chat_record_id；同一段消息+模型+输出上限派生同值，
    /// 「展开会话看各轮明细」分组用；失败占位恒空）
    pub record_id: String,
    pub ts: i64,
    pub preview: String,
}

impl QoderSessionCtx {
    /// 占位上下文（失败早退/无信封可解析时使用：新 uuid + 空 session_id/record_id）
    pub fn placeholder(preview: String) -> Self {
        Self {
            request_id: uuid::Uuid::new_v4().to_string(),
            session_id: String::new(),
            record_id: String::new(),
            ts: chrono::Local::now().timestamp(),
            preview,
        }
    }
}

/// Custom 网关请求级会话上下文（issue #149 P2：custom_sessions 流水行）。
/// request_id 由记账入口内部生成 uuid（custom 上游无信封可取，纯 INSERT 无幂等诉求）；
/// session_id = conversation_key（前 2 条消息指纹，"cv|" 前缀，跨轮稳定）；
/// stage = 请求阶段（main_first / main / tool / unknown）。
pub struct CustomSessionCtx {
    pub session_id: String,
    pub stage: &'static str,
    /// 请求所用 API Key 的 key_id（custom 桶用量 uid 固定 "custom"，流水行落真实 key_id）
    pub uid: String,
    pub key_name: String,
    pub preview: String,
}

/// 从存储加载用量数据（缺失/损坏回退空结构），并裁剪过期日期。
/// SQLite 化 P3：api_usage 表；P7 修订：记账改为当日单行 upsert 后，
/// 存储侧保留期裁剪在启动 load 时一次性执行。
pub fn load(data_dir: &Path) -> UsageFile {
    let mut f = crate::store::docs::api_usage_load(&crate::store::db(data_dir));
    f.trim(RETENTION_DAYS);
    let _ = crate::store::docs::api_usage_prune(&crate::store::db(data_dir), RETENTION_DAYS);
    // issue #82 问题5：Qoder 会话流水与 api_usage 同口径保留期裁剪（启动一次）
    let _ = crate::store::docs::qoder_sessions_prune(&crate::store::db(data_dir), RETENTION_DAYS);
    // issue #149 P2：Custom 会话流水同口径保留期裁剪（启动一次）
    let _ = crate::store::docs::custom_sessions_prune(&crate::store::db(data_dir), RETENTION_DAYS);
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
        UsageBucket::Qoder => "qoder",
        UsageBucket::BuddyAi => "buddy-ai",
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
    /// 网关测量的累计消耗积分（issue #82 T2.4：仅 Qoder 桶 models 非零）
    pub credits: f64,
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
    /// 缓存读 tokens 累计（M4 审查修复：上游未下发时为 0）
    pub cached_tokens: u64,
    /// 网关测量的累计消耗积分（issue #82 T2：仅 Qoder 桶非零）
    pub credits: f64,
    /// 上游计费原值（original_credits，峰谷折扣前）
    pub original_credits: f64,
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
            .map(|(k, c)| view(k, c, d.model_credits.get(k).copied().unwrap_or(0.0)))
            .collect();
        // 仅记积分、无请求计数的模型（record_credits_qoder 不写 models 计数）也并入视图，
        // 前端模型粒度积分才能取到（issue #82 T2.4）
        for (k, credits) in &d.model_credits {
            if !d.models.contains_key(k) {
                models.push(CounterView {
                    name: k.clone(),
                    requests: 0,
                    ok: 0,
                    errors: 0,
                    credits: *credits,
                });
            }
        }
        models.sort_by(|a, b| b.requests.cmp(&a.requests).then(a.name.cmp(&b.name)));
        let mut accounts: Vec<CounterView> = d.accounts.iter().map(|(k, c)| view(k, c, 0.0)).collect();
        accounts.sort_by(|a, b| b.requests.cmp(&a.requests).then(a.name.cmp(&b.name)));
        let mut keys: Vec<CounterView> = d.keys.iter().map(|(k, c)| view(k, c, 0.0)).collect();
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
            cached_tokens: d.cached_tokens,
            credits: d.credits,
            original_credits: d.original_credits,
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

fn view(name: &str, c: &Counter, credits: f64) -> CounterView {
    CounterView {
        name: name.to_string(),
        requests: c.requests,
        ok: c.ok,
        errors: c.errors,
        credits,
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
    fn record_credits_by_model_aggregates() {
        let mut f = UsageFile::default();
        let day = today_key();
        // 同模型累加 + 不同模型分账；None（上游未下发 usage）不入账；负值截断
        f.record_credits_qoder("m1", &day, Some(2.5), Some(3.0));
        f.record_credits_qoder("m1", &day, Some(1.5), None);
        f.record_credits_qoder("m2", &day, Some(4.0), Some(5.0));
        f.record_credits_qoder("m2", &day, None, None);
        f.record_credits_qoder("m2", &day, Some(-1.0), Some(-2.0));
        let d = f.qoder_days.get(&day).expect("当日统计应存在");
        assert!((d.credits - 8.0).abs() < 1e-9, "credits 日合计 = 2.5+1.5+4.0");
        assert!((d.original_credits - 8.0).abs() < 1e-9, "original_credits = 3.0+5.0（None/负值不入账）");
        assert!((d.model_credits.get("m1").copied().unwrap_or(0.0) - 4.0).abs() < 1e-9);
        assert!((d.model_credits.get("m2").copied().unwrap_or(0.0) - 4.0).abs() < 1e-9);
        // 视图层：models[].credits 透出模型粒度积分
        let v = UsageDayView::from_day(&day, d);
        let m1 = v.models.iter().find(|m| m.name == "m1").expect("m1 应在视图");
        assert!((m1.credits - 4.0).abs() < 1e-9);
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
    fn recent_in_filters_by_calendar_window_not_entry_count() {
        // 稀疏数据：今日与 10 天前各一条——「最近 7 天」只应含今日。
        // 原实现按条目计数（7 条 ≠ 7 天），会把 10 天前的条目收进 7 天窗口，
        // 前端 7/14/30 天切换图表与选择不对应
        let mut f = UsageFile::default();
        let old = (chrono::Local::now().date_naive() - chrono::Duration::days(10))
            .format("%Y-%m-%d")
            .to_string();
        f.days.insert(old.clone(), DayStats::default());
        f.days.insert(today_key(), DayStats::default());
        let got = f.recent_in(7, UsageBucket::Trae);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0.as_str(), today_key().as_str());
        // 30 天窗口两条都在，升序：旧日期在前、今日在后
        let got30 = f.recent_in(30, UsageBucket::Trae);
        assert_eq!(got30.len(), 2);
        assert_eq!(got30[0].0.as_str(), old.as_str());
        assert_eq!(got30.last().unwrap().0.as_str(), today_key().as_str());
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
