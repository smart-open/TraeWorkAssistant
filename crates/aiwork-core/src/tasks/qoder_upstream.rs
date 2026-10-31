//! Qoder 上游接入层（p3-3）：catalog / 协议 / 错误分类 / 传输+翻译 / 流式聚合入口。
//!
//! 移植蓝本：aimod-cc/agent2api（MIT）的 `models.rs` / `errors.rs` / `stream.rs` /
//! `protocol.rs`（THINK_TAGS）；传输与首字超时对齐本网关 wb 蓝本
//! （`api_server::wb_upstream`），SSE 出口复用 `api_server::wb_sse`。
//!
//! ── 上游的对话流长什么样 ──────────────────────────────────
//! `gateway.qoder.com.cn` 的 agent_chat_generation 端点回的 SSE **再包一层**：
//! 外层信封 `{statusCodeValue, body}`，真正响应在 `body` 里且是**内层 JSON 字符串**
//! （OpenAI chunk 形状）；业务错误不体现在 HTTP 状态码上（恒 200），放在信封的
//! `statusCodeValue`。`QoderTranslate` 负责拆信封 + 思考标签拆解，产出标准
//! OpenAI chunk 行（`data: {...}` / `data: [DONE]`），下游即可直接复用
//! `wb_sse::stream_forward_ex / aggregate` 的全部协议投影（OpenAI/text/Anthropic/
//! Responses）与聚合逻辑。
//!
//! ── 分层（§定稿管线）─────────────────────────────────────
//! ① catalog：双区（Global/CN）兜底表 + 远程清单采纳（`adopt_remote`），
//!    `list()` 并集去重供聚合目录，`resolve()` 请求时定位 upstreamKey；
//!    产品决策：Auto/Ultimate/Performance/Efficient/Sonus/Cantus 六模型下线
//!    （`REMOVED_MODEL_IDS`，兜底表已删 + 远程带回过滤）；`Step 5 Preview`
//!    为阶跃星辰（StepFun）模型，厂商标注见 `MODEL_VENDORS`；
//! ② 协议常量：对话 URL / Agent 超时（connect 10s + read 300s + write 30s）；
//! ③ 错误分类：`classify_upstream_error`（排队→业务码→额度→状态码判定链），
//!    `to_err_kind` 映射到账号池 `ErrKind`；
//! ④ 传输+翻译：`make_qoder_request`（COSY 签名 + x-model 头 + 四分型传输错误）
//!    与 `QoderTranslate`（信封拆解 + ThinkingParser）；
//! ⑤ 流式/聚合入口：`open_qoder_stream`（首字超时→翻译→可中断行源）；非流式
//!    聚合在 qoder_route::qoder_aggregate_chat 内联实现（QoderTranslate +
//!    `wb_sse::aggregate`，独立 aggregate 入口已删——审查清理无人调用的僵尸代码）。
//!
//! ── 硬约束 ────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件非测试代码零 unwrap/expect/panic；
//! 锁一律 `lock().unwrap_or_else(|e| e.into_inner())` 毒锁恢复；
//! 错误约定 `Result<_, String>` / `UpstreamErr`；凭证不入日志。

use std::collections::VecDeque;
use std::io::Read;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use serde_json::{json, Value};

use crate::api_server::wb_upstream::{lines_with_first_byte_timeout, InterruptibleLines};
use crate::tasks::qoder_common::{self, QoderCreds};
use crate::tasks::qoder_sign::{self, CosyIdentity};

// ==================== ② 协议常量 ====================

/// Qoder 对话上游（CN 推理网关；签名路径会剥去 `/algo` 前缀与查询串，
/// 见 `qoder_sign::signature_path` 的测试背书）
pub const QODER_CHAT_URL: &str = "https://gateway.qoder.com.cn/algo/api/v2/service/pro/sse/agent_chat_generation?FetchKeys=llm_model_result&AgentId=agent_common&Encode=1";
/// 对话上游 host（传输错误文案用）
pub const QODER_CHAT_HOST: &str = "https://gateway.qoder.com.cn";

/// Qoder SSE Agent：连接 10s / 写 30s / 空闲读 300s（对齐 wb_agent）。
/// 进程级共享（审查 P2：原每请求新建 Agent，连接池配置随临时实例丢弃，
/// 网关热路径每次对话全新 TCP+TLS 握手）。ureq::Agent 线程安全；HTTP 层
/// 每请求独立携带凭证头，连接复用仅限传输层，无跨账号侧信道。
pub fn qoder_agent() -> ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT
        .get_or_init(|| {
            ureq::AgentBuilder::new()
                .timeout_read(std::time::Duration::from_secs(300))
                .timeout_write(std::time::Duration::from_secs(30))
                .timeout_connect(std::time::Duration::from_secs(10))
                .max_idle_connections(20)
                .max_idle_connections_per_host(20)
                .build()
        })
        .clone()
}

/// 上游请求错误：(HTTP 状态 | 502 传输错误, body 摘要, Retry-After 秒)（同 wb_upstream）
pub type UpstreamErr = (u16, String, Option<u64>);

// ==================== ① 模型目录 ====================

/// Qoder 账号地区（国际版 / 中国版：两边目录不是同一份，缓存与解析按区分开）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum QoderRegion {
    Global,
    Cn,
}

impl QoderRegion {
    pub fn id(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Cn => "cn",
        }
    }

    /// 该地区的推理网关基址（`model/list` 刷新与对话共用域名口径）
    pub fn gateway(self) -> &'static str {
        match self {
            Self::Global => "https://api3.qoder.sh/",
            Self::Cn => "https://gateway.qoder.com.cn/",
        }
    }
}

/// 输出上限固定 32768。
///
/// 蓝本实测：超过约 32K 后上游行为退化（关闭思考时仍输出思考内容，
/// 约 49K 起返回空内容甚至直接断连）。这是上游的实际约束，不是保守取值。
pub const MAX_OUTPUT_TOKENS: i64 = 32_768;

/// 目录条目没给上下文长度时的兜底
const DEFAULT_CONTEXT_WINDOW: i64 = 200_000;

/// 模型标识归一（下线名单 / 厂商匹配 / 目录去重 共用口径）：
/// 去空白 + 去连字符 + 小写。蓝本 toModelId 只去空白（对外 id 形态不变，
/// 见 `parse_catalog`），匹配层额外折叠连字符——远程 display_name 风格漂移
/// （如 `Step-5-Preview`）仍能命中厂商映射与下线过滤。
fn norm_model_id(s: &str) -> String {
    s.chars()
        .filter(|ch| !ch.is_whitespace() && *ch != '-')
        .collect::<String>()
        .to_lowercase()
}

/// 产品决策下线的模型（不进目录、不参与路由）：远程清单带回也丢弃。
///
/// 匹配规则：目录 id 经 [`norm_model_id`] 归一后**全等**——按全等而非前缀，
/// 避免误伤名称漂移后的其它模型（如 `Auto Coder` 之类）。
const REMOVED_MODEL_IDS: &[&str] = &[
    "auto", "cantus", "efficient", "performance", "sonus", "ultimate",
];

/// 模型厂商标注（id 经 [`norm_model_id`] 归一 → 厂商；目录展示用，不参与路由）。
///
/// 来源：产品确认 —— `Step 5 Preview` 为阶跃星辰（StepFun）公司的模型，
/// 并非 Qoder 自研；目录条目经 `entry()` 注入 `vendor` 键，未命中不给键
/// （前端未命中显示 `—`）。
const MODEL_VENDORS: &[(&str, &str)] = &[
    ("step5preview", "阶跃星辰"),
    // Space-Bunny（太空兔）：2026-10-10 产品确认为 MiniMax 模型（此前标注「未知」）
    ("spacebunny", "MiniMax"),
    // MiMo：小米自研大模型系列（2026-10-10 产品确认）
    ("mimo", "小米"),
    // 其余按需补充：Qwen 系→阿里 / GLM 系→智谱 / Kimi 系→月之暗面 /
    // DeepSeek 系→深度求索 / MiniMax 系→MiniMax
];

/// 模型 id → 厂商标注（未命中返回 None）
fn vendor_of(id: &str) -> Option<&'static str> {
    let normalized = norm_model_id(id);
    MODEL_VENDORS
        .iter()
        .find(|(name, _)| *name == normalized)
        .map(|(_, vendor)| *vendor)
}

/// 内置兜底清单（蓝本静态快照）：(id, key, reasoning, supports_effort, efforts,
/// vision, enabled, price_factor)。
///
/// `enabled` 只是「免费档通常可用」的保守猜测；真实可用性以远程清单的
/// `enable` 为准。**不过滤不可用模型**：用户需要看到完整清单（分清
/// 「模型不存在」与「没权限」，也看得到升级能解锁什么）。
///
/// 倍率取**正常价**：断网/未登录才用这张表，折扣由远程刷新如实带回。
fn fallback(region: QoderRegion) -> Vec<Value> {
    let global: &[(&str, &str, bool, bool, &[&str], bool, bool, &str)] = &[
        ("Qwen3.8-Flash", "qfmodel", true, true, &["low", "medium", "xhigh"], true, true, "0.08"),
        ("Qwen3.8-Max", "qmodel_38max", true, true, &["low", "medium", "xhigh"], true, true, "0.5"),
        // 产品决策下线的模型（Auto/Ultimate/Performance/Efficient/Sonus/Cantus）
        // 不进兜底表——远程清单带回也会被 `REMOVED_MODEL_IDS` 过滤（parse_catalog）
        ("Qwen3.7-Max", "qmodel_latest", true, true, &[], true, false, "0.5"),
        // `Qwen3.7-Plus` 带连字符：上游 display_name 就是这个形态（远程刷新走
        // display_name 去空白）；少连字符会让同一模型产出两种 id
        ("Qwen3.7-Plus", "qmodel", false, false, &[], true, false, "0.1"),
        ("Kimi-K3", "kmodel_latest", false, false, &[], true, false, "0.8"),
        ("Kimi-K2.8-Preview", "kmodel", false, false, &[], true, false, "0.3"),
        ("GLM-5.3", "gmodel", true, true, &[], true, false, "0.6"),
        ("GLM-5.3-Flash", "gfmodel", true, true, &[], true, false, "0.1"),
        ("DeepSeek-V4-Pro", "dmodel", true, true, &[], true, false, "0.8"),
        ("DeepSeek-Flash", "dfmodel", true, true, &[], true, false, "0.2"),
        // 名称漂移修正（2026-10-01 CN 实抓）：上游 mmodel 现为 MiniMax-M2.7
        ("MiniMax-M2.7", "mmodel", false, false, &[], true, false, "0.2"),
    ];
    // CN 表对齐 2026-10-01 model/list 全量实抓（temp/qoder_model_list_full.json，
    // 67856B 真签名直通）——倍率/名称/efforts/is_vl 全按上游实抓值：
    // GLM-5.3 0.6→0.8、Kimi-K3 0.8→1.4、Kimi-K2.8 0.3→0.8、dmodel 0.8→0.5、
    // qfmodel 0.1→0.0（2026-10 初活动免费）→0.08（2026-10-16 远程实抓，
    // App 端同值）：免费判定 entry_is_free 读运行期目录自动跟随，兜底表
    // 仅断网/未同步降级用；MiniMax-M3 实为
    // MiniMax-M2.7；补 Qwen3.7-Flash(q37fmodel)/GLM-5.2(gm51model)。
    // Auto(auto 1→0.5) 已按产品决策下线（见 REMOVED_MODEL_IDS）。
    // thinking 档位白名单：Qwen3.8 [xhigh,low,medium]、GLM-5.3 系 [high,low,max]、
    // DeepSeek 系 [high,max(,low)]、Kimi 系 [high,low,max]；Qwen3.7 系 tc 有但
    // efforts 空 = 不支持档位；MiniMax-M2.7 无 thinking_config。
    let cn: &[(&str, &str, bool, bool, &[&str], bool, bool, &str)] = &[
        ("Qwen3.8-Max", "qmodel_38max", true, true, &["xhigh", "low", "medium"], true, false, "0.5"),
        ("Qwen3.8-Flash", "qfmodel", true, true, &["xhigh", "low", "medium"], true, false, "0.08"),
        ("Qwen3.7-Max", "qmodel_latest", true, false, &[], true, false, "0.5"),
        ("Qwen3.7-Plus", "qmodel", true, false, &[], true, false, "0.1"),
        ("Qwen3.7-Flash", "q37fmodel", true, false, &[], true, false, "0.1"),
        ("DeepSeek-V4-Pro", "dmodel", true, true, &["high", "max"], true, false, "0.5"),
        ("DeepSeek-Flash", "dfmodel", true, true, &["high", "max", "low"], true, false, "0.1"),
        ("GLM-5.3", "gmodel", true, true, &["high", "low", "max"], true, false, "0.8"),
        ("GLM-5.3-Flash", "gfmodel", true, true, &["high", "max"], true, false, "0.1"),
        ("GLM-5.2", "gm51model", true, true, &["high", "max"], true, false, "0.6"),
        ("Kimi-K3", "kmodel_latest", true, true, &["high", "low", "max"], true, false, "1.4"),
        ("Kimi-K2.8-Preview", "kmodel", true, true, &["high", "low", "max"], true, false, "0.8"),
        ("MiniMax-M2.7", "mmodel", false, false, &[], false, false, "0.2"),
    ];
    let rows = if region == QoderRegion::Cn { cn } else { global };
    rows.iter()
        .map(|(id, key, reasoning, supports_effort, efforts, vision, enabled, factor)| {
            entry(
                id,
                key,
                *reasoning,
                if *supports_effort { efforts } else { &[] },
                *vision,
                *enabled,
                DEFAULT_CONTEXT_WINDOW,
                "system",
                &credits_of_text(factor),
            )
        })
        .collect()
}

/// 倍率数值 → `credits` 展示文本（`x{n} credits`）。
///
/// 跨 provider 共用列：WB 的 credits 就是 `"x0.16 credits"` 形态，前端用同一
/// 正则渲染成 `0.16x`。不用 `format!("{:.1}")`：倍率有 `0.04` 档，一位小数会
/// 把它四舍五入成 `0.0`——一个「免费」的错误暗示。
fn credits_of_text(plain: &str) -> String {
    if plain.trim().is_empty() {
        return String::new();
    }
    format!("x{} credits", plain.trim())
}

/// 上游 `price_factor` → 展示文本（远程路径）。
///
/// `price_factor = 0` 是**合法值**（免费档），不能当缺失去掉；返回**字符串**
/// 而非数字，避免下游真值判定把数字 0 当假值丢键（「免费」变「未知」）。
fn credits_of(item: &Value) -> String {
    let factor = match item.get("price_factor") {
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(text)) => text.trim().parse::<f64>().ok(),
        _ => None,
    };
    let Some(factor) = factor else {
        return String::new();
    };
    if !factor.is_finite() || factor < 0.0 {
        return String::new();
    }
    credits_of_text(&format_factor(factor))
}

/// 倍率数字 → 紧凑文本（整数不带小数点，其余最多两位小数、裁掉尾零）
fn format_factor(value: f64) -> String {
    let rounded = (value * 100.0).round() / 100.0;
    if (rounded - rounded.trunc()).abs() < 1e-9 {
        return format!("{}", rounded.trunc() as i64);
    }
    let text = format!("{rounded:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// credits 展示文本 → 倍率数值（"x0.1 credits" → 0.1；0 = 免费合法值）。
/// 聚合目录与智能调度共用（调度排序键需要数值形态的倍率）
pub fn credits_rate_of(credits: &str) -> Option<f64> {
    let trimmed = credits.trim();
    let inner = trimmed
        .strip_prefix('x')
        .or_else(|| trimmed.strip_prefix('X'))?
        .trim();
    let inner = inner.strip_suffix("credits")?.trim();
    inner.parse::<f64>().ok()
}

/// 目录条目是否为免费档（倍率 0）：issue #111 零积分豁免的判定依据——
/// 免费模型零消耗，积分维度过滤（零积分/保留阈值/积分过期）对其整体失义。
/// credits 键缺失（上游没给倍率）按非免费处理（fail-closed，维持既有过滤口径）
pub fn entry_is_free(entry: &Value) -> bool {
    entry
        .get("credits")
        .and_then(Value::as_str)
        .and_then(credits_rate_of)
        .is_some_and(|rate| rate == 0.0)
}

/// 构造一条目录条目（聚合目录与请求构造共用同一形状）。
///
/// `config` 保存请求侧素材（key/is_reasoning/is_vl/source），不会被聚合目录
/// 带进 `/v1/models`（那里只挑认识的键）。`credits` 只进清单展示。
fn entry(
    id: &str,
    upstream_key: &str,
    reasoning: bool,
    efforts: &[&str],
    vision: bool,
    enabled: bool,
    context_window: i64,
    source: &str,
    credits: &str,
) -> Value {
    let mut model = json!({
        "id": id,
        "name": id,
        "upstreamKey": upstream_key,
        "enabled": enabled,
        "reasoning": reasoning,
        "supportsReasoning": reasoning,
        "supportsImages": vision,
        // Qoder 上游是 agent 形态，工具调用是固有能力
        "supportsToolCall": true,
        "efforts": efforts,
        "maxInputTokens": context_window,
        "maxOutputTokens": MAX_OUTPUT_TOKENS,
        "isDefault": false,
        "kind": "chat",
        "config": {
            "key": upstream_key,
            "is_reasoning": reasoning,
            "is_vl": vision,
            "source": source,
        },
    });
    // 倍率键只在有值时插入：让「上游没给」与「给了空串」可区分
    if !credits.is_empty() {
        if let Some(object) = model.as_object_mut() {
            object.insert("credits".to_string(), Value::String(credits.to_string()));
        }
    }
    // 厂商标注只在命中映射时插入（如 Step 5 Preview → 阶跃星辰）
    if let Some(vendor) = vendor_of(id) {
        if let Some(object) = model.as_object_mut() {
            object.insert("vendor".to_string(), Value::String(vendor.to_string()));
        }
    }
    model
}

/// 内部状态：每个地区一份远程清单（空 = 回落静态兜底）
#[derive(Default, Clone)]
struct CatalogState {
    global: Vec<Value>,
    cn: Vec<Value>,
}

/// 进程级目录句柄（OnceLock + RwLock，与 wb/聚合目录同模式）
fn catalog() -> &'static RwLock<CatalogState> {
    static CATALOG: OnceLock<RwLock<CatalogState>> = OnceLock::new();
    CATALOG.get_or_init(|| RwLock::new(CatalogState::default()))
}

fn read_state() -> CatalogState {
    match catalog().read() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

// ── 测试专用（cfg(test)）：目录状态互斥与复位 ──
// 目录是进程级单例，而 catalog_for 在远程目录非空时完全替换静态兜底——
// 并行测试中一方 adopt_remote 注入微型目录，会让另一方依赖兜底表的断言
// （如 list 去重测试）读到被污染的状态。约定：凡测试中调用 adopt_remote，
// 必须持有 catalog_test_guard()，持有期间独占、丢弃时（含 panic 展开）
// 自动复位目录为「全空回落兜底」的初始态。

/// 复位目录为初始态（双区远程清空 → 全部回落静态兜底表）
#[cfg(test)]
pub(crate) fn catalog_test_reset() {
    let apply = |state: &mut CatalogState| {
        state.global = Vec::new();
        state.cn = Vec::new();
    };
    match catalog().write() {
        Ok(mut guard) => apply(&mut guard),
        Err(poisoned) => apply(&mut poisoned.into_inner()),
    }
}

/// RAII 复位标记：丢弃时（含 panic 展开）调用 catalog_test_reset
#[cfg(test)]
pub(crate) struct CatalogTestGuard;

#[cfg(test)]
impl Drop for CatalogTestGuard {
    fn drop(&mut self) {
        catalog_test_reset();
    }
}

/// 测试互斥入口：返回 (锁守卫, 复位守卫)，绑定到一个 let 即可
#[cfg(test)]
pub(crate) fn catalog_test_guard(
) -> (std::sync::MutexGuard<'static, ()>, CatalogTestGuard) {
    let lock = catalog_test_lock().lock().unwrap_or_else(|p| p.into_inner());
    (lock, CatalogTestGuard)
}

#[cfg(test)]
fn catalog_test_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

/// 某地区当前生效的清单（远程优先，否则静态兜底）
fn catalog_for(state: &CatalogState, region: QoderRegion) -> Vec<Value> {
    let remote = match region {
        QoderRegion::Global => &state.global,
        QoderRegion::Cn => &state.cn,
    };
    if remote.is_empty() {
        fallback(region)
    } else {
        remote.clone()
    }
}

/// 接收远程目录响应（`model/list` 的信封 JSON）：解析并替换对应地区缓存。
///
/// 网络请求由接线层的刷新器发起（带 COSY 签名的 GET），本函数只做
/// 「信封校验 + 解析 + 原子替换」。返回采纳的条目数。
pub fn adopt_remote(region: QoderRegion, payload: &Value) -> Result<usize, String> {
    fn apply(state: &mut CatalogState, region: QoderRegion, models: Vec<Value>) {
        match region {
            QoderRegion::Global => state.global = models,
            QoderRegion::Cn => state.cn = models,
        }
    }
    if let Some(message) = envelope_error(payload) {
        return Err(message);
    }
    let models = parse_catalog(payload);
    if models.is_empty() {
        return Err("上游返回的模型目录为空".to_string());
    }
    let count = models.len();
    // 读-改-写全程持写锁（P2 原子性）：原实现 read_state 克隆 → 修改 → write_state
    // 整体替换，双区并发采纳（Global/CN 同时刷新完成）会互相覆盖丢一区数据
    match catalog().write() {
        Ok(mut guard) => apply(&mut guard, region, models),
        Err(poisoned) => apply(&mut poisoned.into_inner(), region, models),
    }
    Ok(count)
}

/// 当前清单（两个地区的**并集**，按 id 去重、global 优先，附 region 键）。
///
/// 聚合目录与路由判定读这里——它们只有「这个模型名认不认识」一个问题；
/// 账号在哪一边由请求时的 `resolve` 收窄。
#[allow(dead_code)] // p3-3-wire（unified_catalog）消费
pub fn list() -> Vec<Value> {
    let state = read_state();
    let mut out: Vec<Value> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for region in [QoderRegion::Global, QoderRegion::Cn] {
        for model in catalog_for(&state, region) {
            let id = text_of(&model, "id");
            if id.is_empty() {
                continue;
            }
            let key = id.to_lowercase();
            if seen.iter().any(|known| known == &key) {
                continue;
            }
            seen.push(key);
            let mut model = model;
            if let Some(object) = model.as_object_mut() {
                object.insert("region".to_string(), Value::String(region.id().to_string()));
            }
            out.push(model);
        }
    }
    out
}

/// 把客户端的模型名解析成目录条目（含 upstreamKey/config/region）。
///
/// 匹配序 id → name → upstreamKey（trim lowercase 全等）；先账号所属地区、
/// 再另一地区、最后兜底表双区——「国际版账号请求中国版专属模型」会拿到
/// 明确的解析结果，由上游报出真实原因，而不是网关谎报「模型不存在」。
#[allow(dead_code)] // p3-3-wire（dispatch/routes）消费
pub fn resolve(model_id: &str, region: QoderRegion) -> Option<Value> {
    let wanted = model_id.trim().to_lowercase();
    if wanted.is_empty() {
        return None;
    }
    // 审查 #5（热路径克隆评估）：resolve 在网关每请求调用一次——锁内按引用
    // 查找（远程目录非空时零克隆），不再经 read_state 整目录克隆。目录量级小、
    // 锁内仅字符串匹配，持锁开销可忽略；list 场景仍走 read_state 克隆。
    let guard = catalog().read().unwrap_or_else(|p| p.into_inner());
    let other = if region == QoderRegion::Cn {
        QoderRegion::Global
    } else {
        QoderRegion::Cn
    };
    for candidate_region in [region, other] {
        let remote = match candidate_region {
            QoderRegion::Global => &guard.global,
            QoderRegion::Cn => &guard.cn,
        };
        let found = if remote.is_empty() {
            find_in(&fallback(candidate_region), &wanted)
        } else {
            find_in(remote, &wanted)
        };
        if let Some(hit) = found {
            return Some(hit);
        }
    }
    // 远程目录在一边有、另一边没有的交错情形：兜底表也双区查一遍。
    // 本区优先（审查 P3）：原序 [other, region] 与远程目录序相反，同名条目会
    // 解析到对区兜底（倍率/档位不同）；对齐远程目录的 [region, other] 顺序
    for candidate_region in [region, other] {
        let models = fallback(candidate_region);
        if let Some(found) = find_in(&models, &wanted) {
            return Some(found);
        }
    }
    None
}

/// 免费活动模型集合（norm_model_id 归一比较，容忍命名漂移）。
/// issue #156：Qwen3.8-Flash 之外新增 DeepSeek-Flash——issue 贴图证实其免费活动
/// 期 usage.billable=false 实际不扣分，与 #111 同形态；目录价随低峰折扣漂移
/// （CN 0.1/Global 0.2）不反映免费，需与 Qwen3.8-Flash 同走倍率覆盖通道
pub const FLASH_FREE_MODELS: &[&str] = &["Qwen3.8-Flash", "DeepSeek-Flash"];

/// 免费活动模型判定（归一比较）
pub fn is_flash_free_model(model_id: &str) -> bool {
    FLASH_FREE_MODELS
        .iter()
        .any(|m| norm_model_id(m) == norm_model_id(model_id))
}

/// 免费活动倍率覆盖（issue #111 / #156，资源调度页开关）：
/// 上游免费活动期 usage.billable=false 实际不扣分，但 model/list 目录价随
/// 低峰折扣漂移（Qwen3.8-Flash 0.1/0.08、DeepSeek-Flash 0.1/0.2），不反映
/// 免费——开关开启时把免费活动模型条目倍率改写为 0，使 entry_is_free 判真：
/// 取号豁免零积分过滤 + /v1/models 展示 0×。只改调用方持有的条目副本，
/// 进程目录缓存不受影响；上游活动结束后关闭开关即恢复官方同步倍率。
pub fn apply_flash_free_override(entry: &mut Value, enabled: bool) {
    if !enabled {
        return;
    }
    let id = entry.get("id").and_then(Value::as_str).unwrap_or("");
    if is_flash_free_model(id) {
        if let Some(object) = entry.as_object_mut() {
            object.insert(
                "credits".to_string(),
                Value::String("x0.0 credits".to_string()),
            );
        }
    }
}

/// 条目副本上的上下文长度目标键（网关内部键，不进上游 model_config / 目录）：
/// prepare_qoder_body 读取后写入 `parameters.context_length`
pub const CONTEXT_TARGET_KEY: &str = "contextLengthTarget";

/// 上下文长度覆盖（Qoder 页单模型设置，api_server::context_window::qoder_target）：
/// 只改调用方持有的条目副本；None = 未设置，条目不变（请求体零变化）。
/// 对冲路径克隆条目即随带，无需另行传参
pub fn apply_context_override(entry: &mut Value, target: Option<u64>) {
    if let (Some(n), Some(object)) = (target, entry.as_object_mut()) {
        object.insert(CONTEXT_TARGET_KEY.to_string(), Value::from(n));
    }
}

/// 上游 400 且本请求带了上下文长度覆盖 → 错误信息尾部追加恢复指引（否则空串）。
/// 上游档位白名单未知，覆盖值不被接受时 400 是最可能的表现，须让调用方看得出原因
pub fn context_override_hint(entry: &Value, status: u16) -> String {
    match entry.get(CONTEXT_TARGET_KEY).and_then(Value::as_u64) {
        Some(n) if status == 400 => format!(
            "（本请求按 Qoder 页设置下发了 parameters.context_length={n}；若上游不接受该值，\
             请在 Qoder 资源调度页清空该模型的上下文长度，恢复跟随上游）"
        ),
        _ => String::new(),
    }
}

/// [`resolve`] + 免费活动覆盖一步到位（取号豁免/预检口径统一走此入口）
pub fn resolve_overridden(model_id: &str, region: QoderRegion, flash_free: bool) -> Option<Value> {
    let mut entry = resolve(model_id, region)?;
    apply_flash_free_override(&mut entry, flash_free);
    Some(entry)
}

/// [`list`] + 免费活动覆盖一步到位（/v1/models 聚合与排序口径统一走此入口）
pub fn list_overridden(flash_free: bool) -> Vec<Value> {
    list()
        .into_iter()
        .map(|mut m| {
            apply_flash_free_override(&mut m, flash_free);
            m
        })
        .collect()
}

/// 在一个清单里按 id → name → upstreamKey 的顺序找（蓝本同序）
fn find_in(models: &[Value], wanted: &str) -> Option<Value> {
    let matches = |value: &Value, key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(|text| text.trim().to_lowercase() == wanted)
            .unwrap_or(false)
    };
    for key in ["id", "name", "upstreamKey"] {
        if let Some(found) = models.iter().find(|model| matches(model, key)) {
            return Some(found.clone());
        }
    }
    None
}

/// 上游目录响应里的业务错误（HTTP 200 也可能带错误：statusCodeValue / code）
fn envelope_error(payload: &Value) -> Option<String> {
    let code = payload
        .get("statusCodeValue")
        .and_then(Value::as_i64)
        .or_else(|| payload.get("code").and_then(Value::as_i64));
    match code {
        Some(200) | None => None,
        Some(code) => Some(format!(
            "上游返回业务错误 {code}: {}",
            payload
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| payload.get("body").and_then(Value::as_str))
                .unwrap_or("")
                .chars()
                .take(200)
                .collect::<String>()
        )),
    }
}

/// 解析目录响应：`{ chat: [ { key, display_name, enable, is_vl, ... } ] }`。
///
/// 只收 `chat` 数组；缺 `key` 或缺 `display_name` 的条目丢弃；对外 id 用
/// `display_name` **去空白**（蓝本 toModelId）——名称可读，请求时映射回 key。
fn parse_catalog(payload: &Value) -> Vec<Value> {
    let Some(chat) = payload.get("chat").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut models: Vec<Value> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for item in chat {
        let key = item.get("key").and_then(Value::as_str).unwrap_or("").trim();
        let display = item
            .get("display_name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if key.is_empty() || display.is_empty() {
            continue;
        }
        let id: String = display.chars().filter(|ch| !ch.is_whitespace()).collect();
        if id.is_empty() {
            continue;
        }
        let lowered = norm_model_id(&id);
        // 产品决策下线的模型：远程清单带回也丢弃（不进目录、不参与路由）
        if REMOVED_MODEL_IDS.contains(&lowered.as_str()) {
            continue;
        }
        if seen.iter().any(|known| known == &lowered) {
            continue;
        }
        seen.push(lowered);

        let vision = item.get("is_vl").map(truthy).unwrap_or(false);
        let reasoning = item.get("is_reasoning").map(truthy).unwrap_or(false)
            || item.get("thinking_config").map(truthy).unwrap_or(false);
        // 上游在该模型条目里声明支持的思考档位（键名即档位值），请求时白名单用
        let efforts: Vec<Value> = item
            .pointer("/thinking_config/enabled/efforts")
            .and_then(Value::as_object)
            .map(|map| map.keys().map(|k| Value::String(k.clone())).collect())
            .unwrap_or_default();
        let context_window = item
            .get("max_input_tokens")
            .and_then(Value::as_i64)
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_CONTEXT_WINDOW);
        let source = item.get("source").and_then(Value::as_str).unwrap_or("system");
        // 倍率：上游在条目顶层给 `price_factor`（0~3.2），折扣时段本身就是折后价
        let credits = credits_of(item);

        let mut model = entry(
            &id,
            key,
            reasoning,
            &[],
            vision,
            item.get("enable").map(truthy).unwrap_or(false),
            context_window,
            source,
            &credits,
        );
        if let Some(object) = model.as_object_mut() {
            object.insert("name".to_string(), Value::String(display.to_string()));
            object.insert("efforts".to_string(), Value::Array(efforts));
            if let Some(format) = item.get("format") {
                if !format.is_null() {
                    if let Some(config) = object.get_mut("config").and_then(Value::as_object_mut) {
                        config.insert("format".to_string(), format.clone());
                    }
                }
            }
            // 上游当前选中的上下文档位，升档判据要与它比（不能与最大档比）
            if let Some(current) = item.get("max_input_tokens").and_then(Value::as_i64) {
                if current > 0 {
                    if let Some(config) = object.get_mut("config").and_then(Value::as_object_mut) {
                        config.insert("max_input_tokens".to_string(), Value::from(current));
                    }
                }
            }
        }
        models.push(model);
    }
    models
}

/// 条目里某个键的文本形态（数字/布尔折叠为字符串，缺失为空串）
fn text_of(value: &Value, key: &str) -> String {
    match value.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// chunk `created` 字段用的秒级时间戳
fn now_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ==================== ③ 错误分类 ====================

/// 上游状态码/错误文本 → 语义分类（蓝本 `classifyUpstreamError` 判定链，逐条对应）。
///
/// ── 顺序很重要 ────────────────────────────────────────────
/// **先看响应体里的语义特征，再看状态码**：上游用 403 同时表达「排队」「额度
/// 不足」「登录态失效」三种完全不同的处置——只按状态码判断必然误判
/// （这正是业务码 10605 被当成鉴权失败那条回归的根因）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UpstreamKind {
    /// 额度/套餐不足（可换账号重试）
    Quota,
    /// 触发限流（可换账号重试）
    Rate,
    /// 鉴权失败（**明确的**：HTTP 401 或业务码 105）
    Auth,
    /// 上游**拒绝访问**（裸 403，且无排队/额度特征）：刷凭证是白刷，不触发续期
    Forbidden,
    /// 上游服务异常（可重试）
    Server,
    /// **模型排队中**（业务码 10605）：不是错误，是「暂时排不上号」。
    /// 不冷却不换号（换谁都一样在排队），也不该给「登录态已失效」误导用户
    Queued,
    /// **传输层掐断**（issue #158 P2-2）：上游 h2 RST_STREAM/连接重置掐断长流，
    /// 行源 relay 线程上抛 io::Error 被 EOF 折叠。账号无关的传输故障：
    /// 不冷却账号/模型、不解绑粘性、不计空完成指纹，按是否已产出内容分流
    ///（未流出 → 换号重试；已流出 → 就地收尾落账失败，与 Trae 池 sse.rs 同口径）
    TransportCut,
    /// 其它（换账号也没用）
    Unknown,
}

impl UpstreamKind {
    /// → 网关账号池错误分类（api_server::ErrKind）。
    ///
    /// Queued → None（不冷却不换号）；Quota → HardCredit（pool::note_error
    /// 特殊处理为次日 04:00 恢复探测）；Forbidden → 直接禁用待人工确认；
    /// Unknown 按 Server 熔断。
    #[allow(dead_code)] // p3-3-wire（dispatch/routes）消费
    pub fn to_err_kind(self) -> crate::api_server::ErrKind {
        match self {
            Self::Quota => crate::api_server::ErrKind::HardCredit,
            Self::Rate => crate::api_server::ErrKind::SoftRate,
            Self::Auth => crate::api_server::ErrKind::SessionDead,
            Self::Forbidden => crate::api_server::ErrKind::Forbidden,
            Self::Server | Self::Unknown => crate::api_server::ErrKind::Server,
            Self::Queued => crate::api_server::ErrKind::None,
            // 传输层掐断与账号健康无关，冷却只会误伤（同 Queued 口径）
            Self::TransportCut => crate::api_server::ErrKind::None,
        }
    }
}

/// 上游排队态的信号（业务码 10605 那一族字段）
#[derive(Clone, Debug, Default)]
pub struct QueueInfo {
    /// 上游建议的重试间隔（秒；`retryAfterSeconds` 优先，其次 `waitTime`）
    pub retry_after_secs: Option<u64>,
    /// 队列档位（`queueType`，实测 "p3"），排障用
    pub queue_type: Option<String>,
    /// 上游声称模型服务是否可用（`serviceAvailable`）
    pub service_available: Option<bool>,
    /// 队列里有多少条（`queueCount`），排障用
    #[allow(dead_code)] // 仅排障透传用，路由层读
    pub queue_count: Option<i64>,
}

/// 分类结果（文案 + 可重试性 + 附带信号）
pub struct ClassifiedError {
    pub kind: UpstreamKind,
    /// 面向客户端的人话
    pub message: String,
    /// 额度类错误带出的定价页链接
    pub pricing_url: Option<String>,
    /// 排队信号（kind == Queued 时有值）
    pub queue: Option<QueueInfo>,
}

impl ClassifiedError {
    fn new(kind: UpstreamKind, message: impl Into<String>, pricing_url: Option<String>) -> Self {
        Self { kind, message: message.into(), pricing_url, queue: None }
    }
}

/// 流内错误元数据（QoderTranslate 写入 err_slot，路由层流结束后读取决定
/// 换号/退避/透传——比 wb_sse 的 (code, msg) 多带分类与排队信号）
#[derive(Clone, Debug)]
#[allow(dead_code)] // 字段由 p3-3-wire 路由层读取
pub struct ErrMeta {
    pub kind: UpstreamKind,
    /// 信封里的业务状态码（不是 HTTP 状态码——那个恒 200）
    pub status: u16,
    pub message: String,
    /// 上游原文（截断后），排障用
    pub raw: String,
    pub pricing_url: Option<String>,
    pub queue: Option<QueueInfo>,
}

// ─── 上游业务码表（官方 SDK 错误码 + 参考实现实测）───────────────
//
// 这些码**不体现在 HTTP 状态码上**：上游把业务错误放在 SSE 信封的
// `statusCodeValue` 里（或与 403 一起放在响应体里），正文常常是**多层嵌套的
// JSON 字符串**。取值要顺着 message / body / data 一路钻进去（scan_signals）。

/// 排队中：官方错误码表 "10605 Model request is queued"
const QUEUE_CODES: &[&str] = &["10605"];
/// 登录态失效：官方错误码表 "105 Login or access token expired"
const AUTH_CODES: &[&str] = &["105"];
/// 额度类：110 每日用量上限、112 额度耗尽、113 用量配额耗尽、114 试用额度用完、
/// 115 免费用户配额用完、116/117/118 团队/成员/个人 Credits 用完、
/// 119 所选模型的免费额度用完、122 计费组上限
const QUOTA_CODES: &[&str] =
    &["110", "112", "113", "114", "115", "116", "117", "118", "119", "122"];

/// 上下文长度参数的正文特征（小写比对；contextlength 覆盖 camelCase 字段名）
const CONTEXT_PARAM_SIGNALS: &[&str] = &["context_length", "contextlength", "context length"];

/// 嵌套 JSON 的钻取深度上限（防病态输入下的环；实测两层就到底）
const SIGNAL_SCAN_DEPTH: usize = 5;

/// 扫出来的判定信号
#[derive(Default)]
struct ErrorSignals {
    /// 各层 `code` 字段的原样文本（数字与字符串两种形态都有）
    codes: Vec<String>,
    queue: QueueInfo,
    /// 见过 `"isQueued":true`
    is_queued: bool,
}

impl ErrorSignals {
    fn has_code(&self, table: &[&str]) -> bool {
        self.codes.iter().any(|code| table.contains(&code.as_str()))
    }

    /// 是不是排队态：业务码 10605 是硬信号；`isQueued` 为真；
    /// 或「声明服务不可用 + 带了队列档位」这一组合（上游换码时的兜底）
    fn queued(&self) -> bool {
        self.has_code(QUEUE_CODES)
            || self.is_queued
            || (self.queue.queue_type.is_some() && self.queue.service_available == Some(false))
    }
}

/// 顺着嵌套 JSON 抠出业务码与排队信号。
///
/// 解析失败不再往下钻：能拿多少算多少，最后由 `classify_upstream_error`
/// 的裸文本兜底（`raw_has_code`）兜住「正文不是 JSON」的情形。
fn scan_signals(raw: &str) -> ErrorSignals {
    let mut signals = ErrorSignals::default();
    let mut current = Some(raw.to_string());
    let mut depth = 0usize;
    while let Some(text) = current.take() {
        depth += 1;
        if depth > SIGNAL_SCAN_DEPTH {
            break;
        }
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            break;
        };
        if let Some(code) = value.get("code") {
            match code {
                Value::String(text) => signals.codes.push(text.trim().to_string()),
                Value::Number(number) => signals.codes.push(number.to_string()),
                _ => {}
            }
        }
        if value.get("isQueued").map(truthy).unwrap_or(false) {
            signals.is_queued = true;
        }
        if signals.queue.retry_after_secs.is_none() {
            signals.queue.retry_after_secs = ["retryAfterSeconds", "waitTime"]
                .iter()
                .find_map(|key| value.get(*key).and_then(seconds_of));
        }
        if signals.queue.queue_type.is_none() {
            signals.queue.queue_type = value
                .get("queueType")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        if signals.queue.service_available.is_none() {
            signals.queue.service_available = value.get("serviceAvailable").map(truthy);
        }
        if signals.queue.queue_count.is_none() {
            signals.queue.queue_count = value.get("queueCount").and_then(Value::as_i64);
        }
        // 下一层：内嵌的 JSON 字符串（上游一层套一层地放 message / body / data）
        current = ["message", "body", "data", "error"]
            .iter()
            .find_map(|key| value.get(*key).and_then(Value::as_str))
            .map(str::to_string)
            .filter(|text| text.trim_start().starts_with('{'));
    }
    signals
}

/// 秒数的两种形态（数字 / 数字字符串）
fn seconds_of(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.trim().parse::<u64>().ok(),
        _ => None,
    }
}

/// JS 真值判定（`Boolean(x)`；蓝本 errors.rs 引 super::protocol::truthy，本文件
/// 无该模块自带同语义实现）：null/false/0/空串为假，数组/对象恒真。
/// 目录解析（is_vl/is_reasoning/enable）与错误分类层共用（审查 P3 去重）
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().map(|item| item != 0.0).unwrap_or(false),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// 裸文本兜底：正文不是 JSON 时也能认出业务码（`\"code\":\"10605\"` 这类
/// 转义形态把反斜杠去掉就与普通形态同形）
fn raw_has_code(raw: &str, code: &str) -> bool {
    let flat: String = raw.chars().filter(|ch| *ch != '\\').collect();
    // 字符串形态：闭合引号即天然边界，无前缀误匹配面
    if flat.contains(&format!("\"code\":\"{code}\""))
        || flat.contains(&format!("\"code\": \"{code}\""))
    {
        return true;
    }
    // 数字形态需边界校验（P2 前缀误匹配：`"code":1053` 含 `"code":105` 子串，
    // 使 1053 被误判为 105）
    raw_contains_numeric_code(&flat, &format!("\"code\":{code}"))
        || raw_contains_numeric_code(&flat, &format!("\"code\": {code}"))
}

/// 数字形态码的边界匹配：命中位置的后随字符若仍是 ASCII 数字，说明命中的只是
/// 更长业务码的前缀（如 1053 之于 105），跳过继续找下一处；后随字符为
/// JSON 分隔符（, } ] 空白等）才算真命中
fn raw_contains_numeric_code(flat: &str, needle: &str) -> bool {
    let mut from = 0usize;
    while let Some(pos) = flat[from..].find(needle) {
        let after = from + pos + needle.len();
        if !flat[after..].chars().next().is_some_and(|ch| ch.is_ascii_digit()) {
            return true;
        }
        from = after;
    }
    false
}

/// 从文本里抠出定价页链接（蓝本正则 `/https?:\/\/[^"\\]*\/pricing[^"\\]*/i` 的无正则版）。
///
/// 与蓝本的差别：`to_lowercase` 对个别 Unicode（如 'İ'）会改变字节长度，导致
/// lowered 与 raw 的下标错位——切片前做 `is_char_boundary` 防越界 panic
///（release 是 panic=abort，这里必须硬保证）。
fn pricing_url_of(raw: &str) -> Option<String> {
    let lowered = raw.to_lowercase();
    let mut search_from = 0usize;
    while search_from < lowered.len() {
        let Some(offset) = lowered[search_from..].find("http") else {
            break;
        };
        let start = search_from + offset;
        if !raw.is_char_boundary(start) {
            search_from = start + 1;
            continue;
        }
        let rest = &raw[start..];
        let end = rest
            .find(|ch: char| ch == '"' || ch == '\\' || ch.is_whitespace())
            .unwrap_or(rest.len());
        let candidate = &rest[..end];
        // 审查 P3：候选先过协议白名单——正文任意含 "/pricing" 的片段
        // （如 "http://evil/x/pricing"）不该被当作升级链接展示给用户
        let lc = candidate.to_lowercase();
        if (lc.starts_with("http://") || lc.starts_with("https://")) && lc.contains("/pricing") {
            return Some(candidate.to_string());
        }
        search_from = start + 4;
    }
    None
}

/// 分类上游错误（判定链：排队 → 业务码 → 额度关键词 → 状态码）
pub fn classify_upstream_error(status: u16, text: &str) -> ClassifiedError {
    let body = text.to_lowercase();
    let pricing = pricing_url_of(text);
    let signals = scan_signals(text);

    // ① 排队中（10605）：既不是额度也不是鉴权，只有「等一会儿再发」一个动作。
    //    排队特征必须前置于额度泛词——排队正文里带 serviceAvailable 等字段，
    //    且 quota 泛词（plan/trial/credit…）太宽，先判才不会被截胡
    if signals.queued() || raw_has_code(text, QUEUE_CODES[0]) {
        let queue = signals.queue;
        return ClassifiedError {
            kind: UpstreamKind::Queued,
            message: queued_message(&queue),
            pricing_url: None,
            queue: Some(queue),
        };
    }
    // ② 业务码：登录态失效（105）与额度类（110/112/113…）
    if signals.has_code(AUTH_CODES) || AUTH_CODES.iter().any(|code| raw_has_code(text, code)) {
        return ClassifiedError::new(UpstreamKind::Auth, "登录态已失效，请重新登录", None);
    }
    if signals.has_code(QUOTA_CODES) || QUOTA_CODES.iter().any(|code| raw_has_code(text, code)) {
        return ClassifiedError::new(
            UpstreamKind::Quota,
            "当前账号额度不足或套餐不支持该模型",
            pricing,
        );
    }
    // ③ 状态码特判（审查 P1：前置于泛词链）——401/429 的语义由协议保证，
    // 优先于文本猜测。此前泛词在前："credit" 子串命中 401 正文 "invalid
    // credentials" 会误判 Quota（HardCredit 次日 04:00 才恢复探测，真鉴权失败
    // 被封号一整天且不触发重登标记）；"exceeded" 命中 429 "rate limit exceeded"
    // 同理把限流误判为额度耗尽
    if status == 401 {
        return ClassifiedError::new(UpstreamKind::Auth, "登录态已失效，请重新登录", None);
    }
    if status == 429 {
        return ClassifiedError::new(UpstreamKind::Rate, "请求过于频繁，请稍后重试", None);
    }
    // ③' 上下文参数被拒（400 + 正文点名 context_length）：前置于额度泛词——
    // "context_length exceeded the model limit" 含 "exceeded" 会被误判 Quota，
    // 健康账号被 HardCredit 冷却到次日 04:00，且路由层「上下文被拒」终局分支
    // （仅接 Unknown）永不命中。排队/鉴权/额度业务码与 401/429 已在上方先判，
    // 不会被吞；仅限 400，403 等仍走原链
    if status == 400 && CONTEXT_PARAM_SIGNALS.iter().any(|signal| body.contains(signal)) {
        return ClassifiedError::new(
            UpstreamKind::Unknown,
            "上游拒绝了上下文长度参数（context_length）",
            None,
        );
    }
    // ④ 额度关键词 / 定价页链接（蓝本判定链；401/429 已前置特判，
    // 此处只兜 403/4xx 正文携带额度语义的形态）
    let quota_signals = [
        "pricingurl",
        "insufficient",
        "no_quota",
        "quota_exceed",
        "exceed_quota",
        "exceeded",
        "credit",
        "upgrade",
        "subscription",
        "plan",
        "trial",
    ];
    if pricing.is_some() || quota_signals.iter().any(|signal| body.contains(signal)) {
        return ClassifiedError::new(
            UpstreamKind::Quota,
            "当前账号额度不足或套餐不支持该模型",
            pricing,
        );
    }
    if body.contains("rate limit") || body.contains("too many") {
        return ClassifiedError::new(UpstreamKind::Rate, "请求过于频繁，请稍后重试", None);
    }
    if status == 403 {
        // 走到这里没有排队、没有额度特征：按权限问题处理（**不**触发刷凭证）
        return ClassifiedError::new(
            UpstreamKind::Forbidden,
            "上游拒绝访问，可能是登录态失效或权限不足",
            None,
        );
    }
    if status >= 500 {
        return ClassifiedError::new(UpstreamKind::Server, "上游服务异常", None);
    }
    // 审查 P2：Unknown 兜底不再空文案——错误帧/last_error 面向用户与排障，
    // 空串信息量为零（400 等无特征错误原样透传状态码）
    ClassifiedError::new(
        UpstreamKind::Unknown,
        format!("上游返回未分类错误（HTTP {status}）"),
        None,
    )
}

/// 排队态面向客户端的文案（说明「不是登录态/额度问题」是关键：写成
/// 「登录态已失效」会让用户去重新登录一个完全正常的账号）
fn queued_message(queue: &QueueInfo) -> String {
    match queue.retry_after_secs {
        Some(seconds) => format!(
            "上游模型排队中（模型暂不可服务，上游建议 {seconds} 秒后重试）：这不是登录态或额度问题"
        ),
        None => "上游模型排队中（模型暂不可服务）：这不是登录态或额度问题".to_string(),
    }
}

// ==================== ④ 传输 + 翻译管线 ====================

/// 发起 Qoder 对话请求，返回 SSE 流 reader。
///
/// 顺序红线（与 qoder_sign 模块头一致）：**先 `encode_body` 再签名**——
/// 请求体参与签名，顺序颠倒会得到「Signature invalid」。
/// `model_key`/`model_source` 由本层直接 `.set()`（签名层不带 x-model 头）。
#[allow(dead_code)] // p3-3-wire（dispatch/routes）消费
pub fn make_qoder_request(
    creds: &QoderCreds,
    body_bytes: &[u8],
    model_key: &str,
    model_source: &str,
) -> Result<Box<dyn Read + Send>, UpstreamErr> {
    let encoded = qoder_sign::encode_body(body_bytes);
    let identity = CosyIdentity {
        user_id: &creds.uid,
        auth_token: &creds.access_token,
        name: &creds.nickname,
        email: "",
        machine_id: &creds.machine_id,
    };
    let headers = match qoder_sign::build_cosy_headers(Some(&encoded), QODER_CHAT_URL, &identity) {
        Ok(headers) => headers,
        // 签名失败 = 凭证素材缺失（uid/token 为空），按鉴权问题上报
        Err(e) => return Err((401, e, None)),
    };
    let mut req = qoder_agent()
        .post(QODER_CHAT_URL)
        .set("Content-Type", "application/json")
        .set("Accept", "text/event-stream")
        .set("User-Agent", qoder_common::CLIENT_USER_AGENT)
        .set("x-model-key", model_key)
        .set("x-model-source", model_source);
    for (k, v) in headers {
        req = req.set(&k, &v);
    }
    match req.send(&encoded[..]) {
        Ok(resp) => Ok(Box::new(resp.into_reader())),
        Err(ureq::Error::Status(code, resp)) => {
            let retry_after = resp
                .header("retry-after")
                .and_then(|v| v.trim().parse::<u64>().ok());
            // 红线#7：响应体读取失败须带错误标记——吞为空串会让错误分类
            //（classify_upstream_error）无特征可判，排障信息全失
            let body_text = resp
                .into_string()
                .unwrap_or_else(|e| format!("<响应体读取失败: {e}>"));
            Err((code, body_text, retry_after))
        }
        Err(e) => {
            // 四分型传输错误（与 make_wb_request 逐字对齐，仅 host 换成 Qoder）
            let s = format!("{}", e);
            let detail = if s.contains("dns")
                || s.contains("resolve")
                || s.contains("name resolution")
            {
                format!("DNS解析失败（{QODER_CHAT_HOST}），请检查网络: {}", e)
            } else if s.contains("timed out") || s.contains("timeout") {
                format!("连接超时（{QODER_CHAT_HOST} 10秒内未响应）: {}", e)
            } else if s.contains("tls") || s.contains("certificate") || s.contains("ssl") {
                format!("TLS证书验证失败: {}", e)
            } else {
                format!("传输错误: {}", e)
            };
            Err((502, detail, None))
        }
    }
}

/// 上游 SSE 里解析出的一条事件（蓝本 `parseSseLine` 的返回联合）
pub enum SseEvent {
    /// 与语义无关的行（空行、非 JSON 行）
    Skip,
    /// 流结束（`data: [DONE]`，含 body 为 "[DONE]" 的信封形态）
    Done,
    /// 一个正常的数据块（内层 JSON，已是 OpenAI chunk 形状）
    Chunk(Value),
    /// 上游业务错误（信封里的 statusCodeValue != 200）
    Error {
        status: u16,
        kind: UpstreamKind,
        raw: String,
        message: String,
        pricing_url: Option<String>,
        queue: Option<QueueInfo>,
    },
}

/// 解析一条 `data:` 之后的内容（蓝本 `parseSseLine` 移植）。
pub fn parse_sse_line(data: &str) -> SseEvent {
    let trimmed = data.trim();
    if trimmed.is_empty() {
        return SseEvent::Skip;
    }
    if trimmed == "[DONE]" {
        return SseEvent::Done;
    }
    let Ok(envelope) = serde_json::from_str::<Value>(trimmed) else {
        return SseEvent::Skip;
    };

    // 业务错误在信封的 statusCodeValue 里（HTTP 状态码始终是 200）
    if let Some(status) = envelope.get("statusCodeValue").and_then(Value::as_i64) {
        if status != 200 {
            let raw: String = match envelope.get("body") {
                Some(Value::String(text)) => text.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            };
            // 审查 P3：statusCodeValue 是 i64，`as u16` 直接截断会把越界值映射
            // 进合法状态码区间（如 65937 → 401 误入换号链）；越界一律按 502 网关错
            let code = if (100..=599).contains(&status) { status as u16 } else { 502 };
            let classified = classify_upstream_error(code, &raw);
            return SseEvent::Error {
                status: code,
                kind: classified.kind,
                raw: raw.chars().take(500).collect(),
                message: classified.message,
                pricing_url: classified.pricing_url,
                queue: classified.queue,
            };
        }
    }

    let Some(inner) = envelope.get("body") else {
        return SseEvent::Skip;
    };
    if inner.as_str() == Some("[DONE]") {
        return SseEvent::Done;
    }
    if inner.is_null() {
        return SseEvent::Skip;
    }
    let chunk = match inner {
        Value::String(text) => match serde_json::from_str::<Value>(text) {
            Ok(parsed) => parsed,
            Err(_) => return SseEvent::Skip,
        },
        other => other.clone(),
    };
    SseEvent::Chunk(chunk)
}

/// 思考标签族（开, 闭）：蓝本 protocol.rs 的 THINK_TAGS 原表
const THINK_TAGS: &[(&str, &str)] = &[
    ("<thinking>", "</thinking>"),
    ("<think>", "</think>"),
    ("<reasoning>", "</reasoning>"),
    ("<thought>", "</thought>"),
];

/// 拆解产出的一段内容
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThinkingPiece {
    pub text: String,
    /// 真 = 思考内容（下发 reasoning_content），假 = 正文
    pub is_thinking: bool,
}

/// 思考标签拆解器（蓝本 `ThinkingParser` 状态机移植）。
///
/// ── 为什么产出是「攒起来等取」而不是回调 ────────────────────
/// 解析器必须**跨片存活**（开标签被切成两半时状态要留到下一片），作为长驻
/// 字段挂在翻译器上；回调形态没法这样挂。改成「解析器持有输出队列，调用方
/// 取走」后就是一个普通结构体，随翻译器活到流结束。
///
/// ── 用法（**必须**调 `finish`）────────────────────────────
/// 内部保留可能是标签前缀的尾巴，不收尾的话最后几个字符会永远留在缓冲里
///（表现为回答末尾少字）。
pub struct ThinkingParser {
    /// 已拆解、待调用方取走的产出
    out: Vec<ThinkingPiece>,
    buffer: String,
    in_thinking: bool,
    /// 进入思考时用的那个闭标签（开标签可能是 <think> 或 <thinking>）
    active_close: &'static str,
    finished: bool,
}

impl Default for ThinkingParser {
    fn default() -> Self {
        Self::new()
    }
}

impl ThinkingParser {
    pub fn new() -> Self {
        Self {
            out: Vec::new(),
            buffer: String::new(),
            in_thinking: false,
            active_close: THINK_TAGS[0].1,
            finished: false,
        }
    }

    /// 喂入一段增量
    pub fn push(&mut self, chunk: &str) {
        if chunk.is_empty() {
            return;
        }
        self.buffer.push_str(chunk);
        self.drain(false);
    }

    /// 收尾：把残留内容按当前状态输出（幂等，重复调用不会有额外产出）
    pub fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.drain(true);
        if !self.buffer.is_empty() {
            let rest = std::mem::take(&mut self.buffer);
            self.emit(rest, self.in_thinking);
        }
    }

    /// 取走当前累积的产出
    pub fn take(&mut self) -> Vec<ThinkingPiece> {
        std::mem::take(&mut self.out)
    }

    /// 记一段产出（空串不入队）
    fn emit(&mut self, text: String, is_thinking: bool) {
        if text.is_empty() {
            return;
        }
        self.out.push(ThinkingPiece { text, is_thinking });
    }

    /// 这段文本的尾巴有多长可能是某个标签的前缀（需要留到下一片再判断）
    fn trailing_prefix_len(text: &str) -> usize {
        let mut max = 0usize;
        for (open, close) in THINK_TAGS {
            for tag in [*open, *close] {
                let limit = text.len().min(tag.len() - 1);
                // 只可能在**字符**边界上，从长到短试
                for len in (1..=limit).rev() {
                    if !text.is_char_boundary(text.len() - len) {
                        continue;
                    }
                    if text.ends_with(&tag[..len]) {
                        if len > max {
                            max = len;
                        }
                        break;
                    }
                }
            }
        }
        max
    }

    /// 由闭标签反查对应开标签的长度（进思考状态时要用它跳过开标签）
    fn open_len_of(close_tag: &str) -> usize {
        THINK_TAGS
            .iter()
            .find(|(_, close)| *close == close_tag)
            .map(|(open, _)| open.len())
            .unwrap_or(0)
    }

    /// 状态机主体：在正文里找开标签、在思考里找闭标签
    fn drain(&mut self, is_final: bool) {
        // 防病态输入下的无限循环（与蓝本 guard 同一目的）
        let mut guard = 0;
        while !self.buffer.is_empty() && guard < 1000 {
            guard += 1;

            if self.in_thinking {
                let close = self.active_close;
                if let Some(position) = self.buffer.find(close) {
                    if position > 0 {
                        let text = self.buffer[..position].to_string();
                        self.emit(text, true);
                    }
                    let rest = self.buffer[position + close.len()..].to_string();
                    self.buffer = strip_leading_newline(&rest);
                    self.in_thinking = false;
                    continue;
                }
                if is_final {
                    let text = std::mem::take(&mut self.buffer);
                    self.emit(text, true);
                    return;
                }
                // 留出可能是闭标签前缀的尾部
                let keep = Self::trailing_prefix_len(&self.buffer);
                let safe = self.buffer.len() - keep;
                if safe > 0 {
                    let text = self.buffer[..safe].to_string();
                    self.emit(text, true);
                    self.buffer = self.buffer[safe..].to_string();
                }
                return;
            }

            // 正文状态：找最早出现的开标签或闭标签
            let mut best_open: Option<(usize, &'static str)> = None;
            let mut best_close: Option<usize> = None;
            for (open, close) in THINK_TAGS {
                if let Some(position) = self.buffer.find(open) {
                    if best_open.map(|(at, _)| position < at).unwrap_or(true) {
                        best_open = Some((position, *close));
                    }
                }
                if let Some(position) = self.buffer.find(close) {
                    if best_close.map(|at| position < at).unwrap_or(true) {
                        best_close = Some(position);
                    }
                }
            }

            let take_open = match (best_open, best_close) {
                (Some((open_at, _)), Some(close_at)) => open_at < close_at,
                (Some(_), None) => true,
                _ => false,
            };

            if let Some((open_at, close_tag)) = best_open.filter(|_| take_open) {
                if open_at > 0 {
                    let text = self.buffer[..open_at].to_string();
                    self.emit(text, false);
                }
                let open_len = Self::open_len_of(close_tag);
                let rest = self.buffer[open_at + open_len..].to_string();
                self.buffer = rest;
                self.active_close = close_tag;
                self.in_thinking = true;
                continue;
            }

            if let Some(close_at) = best_close {
                // 单独的闭标签：丢弃它（蓝本同款处理）
                if close_at > 0 {
                    let text = self.buffer[..close_at].to_string();
                    self.emit(text, false);
                }
                let close_len = THINK_TAGS
                    .iter()
                    .filter(|(_, close)| self.buffer[close_at..].starts_with(*close))
                    .map(|(_, close)| close.len())
                    .max()
                    .unwrap_or(0);
                let rest = self.buffer[close_at + close_len..].to_string();
                self.buffer = strip_leading_newline(&rest);
                continue;
            }

            if is_final {
                let text = std::mem::take(&mut self.buffer);
                self.emit(text, false);
                return;
            }
            let keep = Self::trailing_prefix_len(&self.buffer);
            let safe = self.buffer.len() - keep;
            if safe > 0 {
                let text = self.buffer[..safe].to_string();
                self.emit(text, false);
                self.buffer = self.buffer[safe..].to_string();
            }
            return;
        }
    }
}

/// 吃掉闭标签后紧跟的换行（蓝本同款：先试 `\n\n` 两个，再试单个）。
///
/// 上游在闭标签后习惯留一个空行再写正文（`</thinking>\n\n正文`），
/// 只吃一个会让正文以空行开头。`\r\n` 分支是对 Windows 换行的加固。
fn strip_leading_newline(text: &str) -> String {
    if let Some(rest) = text.strip_prefix("\n\n") {
        return rest.to_string();
    }
    if let Some(rest) = text.strip_prefix("\r\n") {
        return rest.to_string();
    }
    if let Some(rest) = text.strip_prefix('\n') {
        return rest.to_string();
    }
    text.to_string()
}

/// 去掉文本里的思考标签。
///
/// 用途：上游在 `reasoning_content` 里偶带标签（剥掉后直发）。
pub fn strip_thinking_tags(text: &str) -> String {
    let mut out = text.to_string();
    for (open, close) in THINK_TAGS {
        out = out.replace(open, "").replace(close, "");
    }
    out
}

/// Qoder 上游行 → OpenAI chunk 行的翻译器（Iterator 适配器）。
///
/// 输入：上游原始 SSE 行（BufReader 行序）；输出：标准 OpenAI chunk 行
///（`data: {chunk}` / `data: [DONE]`），可直接喂 `wb_sse::aggregate`（泛型），
/// 或经 `InterruptibleLines::from_iterator` 桥接后喂 `wb_sse::stream_forward_ex`。
///
/// 分帧语义与 `WbSseParser::feed_line` 逐条对齐（trim_end → 空行分帧 →
/// `:` 注释跳过 → data: 拼接 + 紧凑流立即产出 → Done 置 done）。
///
/// 错误路径：写 `err_slot`（路由层流结束后读）+ 下发 OpenAI 错误帧 +
/// `done=true`（**不发 [DONE]**——让 stream_forward_ex 捕获 error_info 以换号）。
pub struct QoderTranslate<L: Iterator<Item = String>> {
    lines: L,
    /// 跨行 data 缓冲（SSE 多行拼接）
    data: String,
    /// 已翻译、待 Iterator::next 取走的输出帧
    queue: VecDeque<String>,
    done: bool,
    thinker: ThinkingParser,
    chat_id: String,
    model: String,
    /// 流内错误槽（Arc 共享：翻译器被 from_iterator 消费后路由层仍可读）
    err_slot: Arc<Mutex<Option<ErrMeta>>>,
    /// 底层读错误共享槽（issue #158 P2-2）：行源为 wb_upstream 竞速/首字超时
    /// 产物（InterruptibleLines 类型被 Box 擦拭，relay 线程的 io::Error 在
    /// chain_rest 折叠点写入共享槽）。EOF 时槽有错误 = 流被传输层掐断而非
    /// 正常结束——flush_finish 据此不补 [DONE] 并写入 TransportCut ErrMeta
    read_slot: Option<crate::api_server::wb_upstream::ReadErrSlot>,
}

impl<L: Iterator<Item = String>> QoderTranslate<L> {
    pub fn new(
        lines: L,
        err_slot: Arc<Mutex<Option<ErrMeta>>>,
        chat_id: &str,
        model: &str,
    ) -> Self {
        Self::with_read_slot(lines, err_slot, None, chat_id, model)
    }

    /// 带底层读错误槽构造（issue #158 P2-2）：路由层从竞速胜者
    ///（`QoderRaceWin.read_slot`）或 tracked 行源取槽传入；测试/内存切分
    /// 形态传 None（行为与 new 完全一致）
    pub fn with_read_slot(
        lines: L,
        err_slot: Arc<Mutex<Option<ErrMeta>>>,
        read_slot: Option<crate::api_server::wb_upstream::ReadErrSlot>,
        chat_id: &str,
        model: &str,
    ) -> Self {
        Self {
            lines,
            data: String::new(),
            queue: VecDeque::new(),
            done: false,
            thinker: ThinkingParser::new(),
            chat_id: chat_id.to_string(),
            model: model.to_string(),
            err_slot,
            read_slot,
        }
    }

    /// 单行喂入（分帧语义对齐 WbSseParser::feed_line，见结构体注释）
    fn feed_line(&mut self, line: &str) {
        let line = line.trim_end();
        if line.is_empty() {
            if self.data.is_empty() {
                return;
            }
            let payload = std::mem::take(&mut self.data);
            self.handle_payload(&payload);
            return;
        }
        if line.starts_with(':') {
            return; // SSE 注释（keep-alive 等）
        }
        if let Some(rest) = line.strip_prefix("data:") {
            self.data.push_str(rest.trim_start());
            // 紧凑流兼容（部分上游不按「空行分隔」发帧）：
            // 缓冲已是完整载荷（[DONE] 或完整 JSON）→ 立即产出
            let t = self.data.trim();
            let complete =
                t == "[DONE]" || (t.starts_with('{') && serde_json::from_str::<Value>(t).is_ok());
            if complete {
                let payload = std::mem::take(&mut self.data);
                self.handle_payload(&payload);
            }
        }
        // 其余行（event:/id:/retry:/未知）忽略：上游只用 data: 承载内容
    }

    /// 分发一条完整 data 载荷
    fn handle_payload(&mut self, payload: &str) {
        match parse_sse_line(payload) {
            SseEvent::Skip => {}
            SseEvent::Done => self.flush_finish(),
            SseEvent::Chunk(inner) => self.handle_chunk(inner),
            SseEvent::Error { status, kind, raw, message, pricing_url, queue } => {
                {
                    let mut slot = self.err_slot.lock().unwrap_or_else(|e| e.into_inner());
                    *slot = Some(ErrMeta {
                        kind,
                        status,
                        message: message.clone(),
                        raw,
                        pricing_url,
                        queue,
                    });
                }
                // OpenAI 形态错误帧：wb_sse 的 parse_data 识别 `{"error":{...}}`
                // → WbEvent::Error → stream_forward_ex 捕获 error_info。
                // 不发 [DONE]：错误即流终止，路由层据此换号重试
                let frame = json!({"error": {"code": status as i64, "message": message}});
                self.queue.push_back(format!("data: {}", frame));
                self.done = true;
            }
        }
    }

    /// 处理内层 OpenAI chunk（拆信封后的 body）
    fn handle_chunk(&mut self, inner: Value) {
        // usage 独立提取（上游可能只在收尾帧带 usage；null 视同没给）
        let usage = inner.get("usage").filter(|u| !u.is_null()).cloned();
        let choice = inner
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .cloned()
            .unwrap_or(Value::Null);
        let delta = choice.get("delta").cloned().unwrap_or_else(|| json!({}));
        let finish = choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let chunk_id = inner
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| self.chat_id.clone());

        // tool_calls：整体透传（标签拆解只针对文本内容，不拆工具调用）
        if delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(|a| !a.is_empty())
            .unwrap_or(false)
        {
            self.push_chunk(&chunk_id, delta, usage, &finish);
            return;
        }

        // 思考内容：上游已分流，不进拆解器；剥残留标签后直发
        if let Some(text) = delta
            .get("reasoning_content")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            let clean = strip_thinking_tags(text);
            self.emit_piece(&chunk_id, &clean, true);
        }

        // 正文：进拆解器（思考可能混在正文里，跨分片边界安全拆解）
        if let Some(text) = delta
            .get("content")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            self.thinker.push(text);
            for piece in self.thinker.take() {
                self.emit_piece(&chunk_id, &piece.text, piece.is_thinking);
            }
        }

        if !finish.is_empty() {
            // 收尾：先冲拆解器尾巴（幂等），再发 carry 帧（空 delta + finish + usage）
            self.thinker.finish();
            for piece in self.thinker.take() {
                self.emit_piece(&chunk_id, &piece.text, piece.is_thinking);
            }
            self.push_chunk(&chunk_id, json!({}), usage, &finish);
        } else if usage.is_some() {
            // 非 finish 但带 usage：独立 usage 帧（delta 空）
            self.push_chunk(&chunk_id, json!({}), usage, "");
        }
    }

    /// 发一个标准 OpenAI chunk 帧（`id/object/created/model/choices` 补全）
    fn push_chunk(&mut self, chunk_id: &str, delta: Value, usage: Option<Value>, finish: &str) {
        let mut frame = json!({
            "id": chunk_id,
            "object": "chat.completion.chunk",
            "created": now_ts(),
            "model": self.model.as_str(),
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        });
        if let Some(u) = usage {
            if let Some(object) = frame.as_object_mut() {
                object.insert("usage".to_string(), u);
            }
        }
        self.queue.push_back(format!("data: {}", frame));
    }

    /// 发一段拆解产出（is_thinking → reasoning_content，正文 → content）。
    /// 帧沿用来源 chunk 的 id（保流关联）；flush_finish 路径无来源 id 时传 chat_id。
    fn emit_piece(&mut self, chunk_id: &str, text: &str, is_thinking: bool) {
        if text.is_empty() {
            return;
        }
        let delta = if is_thinking {
            json!({"reasoning_content": text})
        } else {
            json!({"content": text})
        };
        self.push_chunk(chunk_id, delta, None, "");
    }

    /// 流收尾：冲拆解器尾巴 + 发 [DONE]（幂等；错误路径已置 done 时不再发）。
    ///
    /// 传输层掐断识别（issue #158 P2-2）：EOF 伴随读错误槽有值 = 流被上游
    /// 传输层掐断（h2 RST_STREAM/连接重置），不是正常结束——
    /// ① **不补 [DONE]**：下游 WbSseParser 视作 EOF 断流（流式按 EOF-有内容
    ///    补终止帧、聚合不得把部分响应当成功返回）；
    /// ② err_slot 尚无业务错误时写入 TransportCut ErrMeta（err_slot 已有
    ///    信封业务错误时保留之——业务错误更具诊断价值，业务错误帧路径本就
    ///    不发 [DONE]）。
    /// 上游自行下发 [DONE]（SseEvent::Done）走不到 EOF 分支，不受影响。
    fn flush_finish(&mut self) {
        if self.done {
            return;
        }
        let chat_id = self.chat_id.clone();
        self.thinker.finish();
        for piece in self.thinker.take() {
            self.emit_piece(&chat_id, &piece.text, piece.is_thinking);
        }
        let cut = self
            .read_slot
            .as_ref()
            .and_then(|s| crate::api_server::wb_upstream::take_read_error_slot(s));
        if let Some(e) = cut {
            {
                let mut slot = self.err_slot.lock().unwrap_or_else(|e| e.into_inner());
                if slot.is_none() {
                    *slot = Some(ErrMeta {
                        kind: UpstreamKind::TransportCut,
                        status: 0,
                        message: format!(
                            "{} {}",
                            crate::api_server::TRANSPORT_CUT_MSG_PREFIX,
                            e
                        ),
                        raw: String::new(),
                        pricing_url: None,
                        queue: None,
                    });
                }
            }
            self.done = true; // 流是断的，不是完的：不发 [DONE]
            return;
        }
        self.queue.push_back("data: [DONE]".to_string());
        self.done = true;
    }
}

impl<L: Iterator<Item = String>> Iterator for QoderTranslate<L> {
    type Item = String;

    fn next(&mut self) -> Option<String> {
        loop {
            if let Some(frame) = self.queue.pop_front() {
                return Some(frame);
            }
            if self.done {
                return None;
            }
            match self.lines.next() {
                Some(line) => self.feed_line(&line),
                None => self.flush_finish(),
            }
        }
    }
}

// ==================== ⑤ 流式 / 聚合入口 ====================

/// 流式入口（p3-3-wire 路由层调用）：reader → 首字超时（10s，Err(()) =
/// 首字超时/首行前失败，调用方按 Server 故障转移换号）→ QoderTranslate
/// 翻译为 OpenAI chunk 行 → 可中断行源（调用方喂 wb_sse::stream_forward_ex）。
///
/// `err_slot` 由调用方创建并持有克隆：翻译器被转发线程消费后，路由层在
/// 流结束后仍可读出 ErrMeta（分类/排队信号/定价页）决定换号与冷却。
#[allow(dead_code)] // p3-3-wire（dispatch/routes）消费
pub fn open_qoder_stream(
    reader: Box<dyn Read + Send>,
    err_slot: Arc<Mutex<Option<ErrMeta>>>,
    chat_id: &str,
    model: &str,
) -> Result<InterruptibleLines, ()> {
    let lines = lines_with_first_byte_timeout(reader)?;
    let translated = QoderTranslate::new(lines, err_slot, chat_id, model);
    Ok(InterruptibleLines::from_iterator(Box::new(translated)))
}

// ==================== ⑦ 请求体构造（OpenAI → Qoder agent 信封） ====================
//
// 移植蓝本：agent2api protocol.rs（build_upstream_body 一族）。下游按 OpenAI
// 习惯发 {model, messages, tools, stream}；上游要的是带会话/记录/业务上下文的
// **固定信封**。三个上游实际约束（见各函数注释）：
// system 提示必须进 messages；model_config 必须剥 thinking_config；
// 「关闭思考」降级为「不指定」（Qwen3.8 系列发 false 会断连）。

/// 上游要求的会话类型与 agent 标识（源实现常量）
const SESSION_TYPE: &str = "qodercli";
const AGENT_ID: &str = "agent_common";
const CHAT_TASK: &str = "FREE_INPUT";

/// 把任意形态的 content 压平成文本（源实现 contentToText；数组里的非文本块跳过）
pub fn content_to_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(items) => {
            let mut out = String::new();
            for item in items {
                match item {
                    Value::String(text) => out.push_str(text),
                    Value::Object(object) => {
                        if let Some(text) = object.get("text").and_then(Value::as_str) {
                            out.push_str(text);
                        } else if let Some(text) = object.get("content").and_then(Value::as_str) {
                            out.push_str(text);
                        }
                    }
                    _ => {}
                }
            }
            out
        }
        _ => String::new(),
    }
}

/// 抽出消息里的图片块（issue #154：三协议形态归一为 OpenAI image_url——
/// Anthropic image / Responses input_image 直入 chat completions 时转换；
/// 坏块（缺字段/空 data）丢弃；OpenAI 主形态原样保留）
fn images_of(content: &Value) -> Vec<Value> {
    content
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| match item.get("type").and_then(Value::as_str) {
                    Some("image_url") => Some(item.clone()),
                    Some("image") => anthropic_image_as_url(item),
                    Some("input_image") => responses_image_as_url(item),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Anthropic image block（source.type = base64/url）→ image_url 形态
///（自抄 api_server::payload::anthropic_image_to_openai：跨层私有不可复用）
fn anthropic_image_as_url(block: &Value) -> Option<Value> {
    let source = block.get("source")?;
    let url = match source.get("type").and_then(Value::as_str) {
        Some("base64") => {
            let media = source.get("media_type").and_then(Value::as_str)?;
            let data = source.get("data").and_then(Value::as_str)?;
            if data.is_empty() {
                return None;
            }
            format!("data:{media};base64,{data}")
        }
        Some("url") => source.get("url").and_then(Value::as_str)?.to_string(),
        _ => return None,
    };
    if url.is_empty() {
        return None;
    }
    Some(json!({ "type": "image_url", "image_url": { "url": url } }))
}

/// Responses input_image part（image_url/url 字段）→ image_url 形态
fn responses_image_as_url(part: &Value) -> Option<Value> {
    let url = part
        .get("image_url")
        .and_then(Value::as_str)
        .or_else(|| part.get("url").and_then(Value::as_str))?;
    if url.is_empty() {
        return None;
    }
    Some(json!({ "type": "image_url", "image_url": { "url": url } }))
}

/// 把 OpenAI 的 messages 规整成上游能吃的形状（源实现 normalizeMessages）：
/// 上次出错/中断的 assistant 轮次连同其 tool 结果一起丢弃（否则留下孤儿结果
/// 上游会拒）；assistant 只有工具调用时补非空 content；用户图片保持块形态。
pub fn normalize_messages(messages: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut dropped_tool_call_ids: Vec<String> = Vec::new();
    for message in messages {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        if role.is_empty() {
            continue;
        }
        if role == "assistant" {
            let failed = message.get("__failed").map(truthy).unwrap_or(false)
                || message.get("__aborted").map(truthy).unwrap_or(false);
            if failed {
                if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
                    for call in calls {
                        if let Some(id) = call.get("id").and_then(Value::as_str) {
                            dropped_tool_call_ids.push(id.to_string());
                        }
                    }
                }
                continue;
            }
        }
        if role == "tool" {
            let call_id = message
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            if dropped_tool_call_ids.iter().any(|known| known == call_id) {
                continue;
            }
        }
        match role {
            "user" => {
                let content = message.get("content").cloned().unwrap_or(Value::Null);
                let images = images_of(&content);
                if images.is_empty() {
                    out.push(json!({ "role": "user", "content": content_to_text(&content) }));
                } else {
                    let text = content_to_text(&content);
                    let mut parts: Vec<Value> = Vec::new();
                    if !text.is_empty() {
                        parts.push(json!({ "type": "text", "text": text }));
                    }
                    for image in images {
                        parts.push(json!({
                            "type": "image_url",
                            "image_url": image.get("image_url").cloned().unwrap_or(Value::Null),
                        }));
                    }
                    out.push(json!({ "role": "user", "content": parts }));
                }
            }
            "assistant" => {
                let text = content_to_text(message.get("content").unwrap_or(&Value::Null));
                let tool_calls: Vec<Value> = message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .map(|calls| {
                        calls
                            .iter()
                            .map(|call| {
                                let arguments = call
                                    .pointer("/function/arguments")
                                    .map(|value| match value {
                                        Value::String(text) => text.clone(),
                                        other => other.to_string(),
                                    })
                                    .unwrap_or_else(|| "{}".to_string());
                                json!({
                                    "id": call.get("id").and_then(Value::as_str).unwrap_or(""),
                                    "type": "function",
                                    "function": {
                                        "name": call
                                            .pointer("/function/name")
                                            .and_then(Value::as_str)
                                            .unwrap_or(""),
                                        "arguments": arguments,
                                    },
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let content = if text.is_empty() && !tool_calls.is_empty() {
                    " ".to_string()
                } else {
                    text
                };
                let mut mapped = json!({ "role": "assistant", "content": content });
                if !tool_calls.is_empty() {
                    if let Some(object) = mapped.as_object_mut() {
                        object.insert("tool_calls".to_string(), Value::Array(tool_calls));
                    }
                }
                out.push(mapped);
            }
            "tool" => out.push(json!({
                "role": "tool",
                "tool_call_id": message.get("tool_call_id").cloned().unwrap_or(Value::Null),
                "content": content_to_text(message.get("content").unwrap_or(&Value::Null)),
            })),
            "system" | "developer" => out.push(json!({
                "role": "system",
                "content": content_to_text(message.get("content").unwrap_or(&Value::Null)),
            })),
            _ => {}
        }
    }
    out
}

/// 把 OpenAI 的 tools 转成上游形态（源实现 normalizeTools）
pub fn normalize_tools(tools: Option<&Value>) -> Option<Vec<Value>> {
    let items = tools?.as_array()?;
    if items.is_empty() {
        return None;
    }
    let mapped: Vec<Value> = items
        .iter()
        .filter(|item| item.pointer("/function/name").and_then(Value::as_str).is_some())
        .map(|item| {
            let mut function = serde_json::Map::new();
            if let Some(name) = item.pointer("/function/name") {
                function.insert("name".to_string(), name.clone());
            }
            if let Some(description) = item.pointer("/function/description") {
                function.insert("description".to_string(), description.clone());
            }
            if let Some(parameters) = item.pointer("/function/parameters") {
                function.insert("parameters".to_string(), parameters.clone());
            }
            json!({ "type": "function", "function": Value::Object(function) })
        })
        .collect();
    if mapped.is_empty() {
        None
    } else {
        Some(mapped)
    }
}

/// 一次请求的思考设置（见「关闭思考」说明）
pub struct ThinkingChoice {
    /// 是否显式开启思考（None = 不指定参数，让上游走默认）
    pub enable: Option<bool>,
    /// 思考档位（仅当 enable == Some(true) 时才会被发出去）
    pub effort: Option<String>,
}

/// 下游请求体里表达思考档位的原始值（None = 三个键都不存在；取值链命中即停）
fn declared_reasoning(body: &Value) -> Option<Value> {
    body.get("reasoning_effort")
        .or_else(|| body.get("reasoning"))
        .or_else(|| body.get("thinking"))
        .cloned()
}

/// 把下游的思考强度映射成上游接受的值（源实现 resolveThinking）。
///
/// 「关闭思考」降级成「不指定」：Qwen3.8 系列**无法真正关闭思考**——传
/// enable_thinking=false 时上游要么把思考混进正文，要么直接断连。
/// 下游可能用 reasoning_effort / reasoning / thinking 任一字段表达。
pub fn resolve_thinking(body: &Value, model: &Value) -> ThinkingChoice {
    let raw = declared_reasoning(body).unwrap_or(Value::Null);

    let reasoning = model.get("reasoning").map(truthy).unwrap_or(false);
    // 模型不支持思考：直接不指定
    if !reasoning {
        return ThinkingChoice { enable: None, effort: None };
    }

    if matches!(raw, Value::Bool(false))
        || matches!(raw.as_str(), Some("off") | Some("none") | Some("disabled"))
    {
        return ThinkingChoice { enable: None, effort: None };
    }

    // 未指定（或显式 true）：沿用上游默认档位
    if raw.is_null() || matches!(raw, Value::Bool(true)) {
        return ThinkingChoice { enable: Some(true), effort: None };
    }

    let asked = match &raw {
        Value::String(text) => text.trim().to_lowercase(),
        other => other.to_string().trim().to_lowercase(),
    };

    let declared: Vec<String> = model
        .get("efforts")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let pool: Vec<String> = if declared.is_empty() {
        vec!["low".to_string(), "medium".to_string(), "xhigh".to_string()]
    } else {
        declared.clone()
    };
    // ① 白名单原值命中直用（审查修复）：目录声明的档位（如 GLM/Kimi 系的 max）
    // 必须可显式请求——别名映射先于白名单会把合法档位改写成不存在的值再静默回退
    if pool.iter().any(|known| known == &asked) {
        return ThinkingChoice { enable: Some(true), effort: Some(asked) };
    }
    // ② 常见别名归一：OpenAI 侧的 minimal/high 与上游档位对齐
    //（Qwen3.8 系无 high/max 档，需映射到 xhigh；GLM/Kimi 系已在 ① 直用）
    let wanted = match asked.as_str() {
        "minimal" | "min" => "low",
        "high" | "max" => "xhigh",
        other => other,
    };
    let effort = if pool.iter().any(|known| known == wanted) {
        wanted.to_string()
    } else {
        // 请求的档位模型不支持：退回该模型的默认档，而不是发一个无效值
        declared
            .iter()
            .find(|known| known.as_str() == "medium")
            .or_else(|| declared.first())
            .cloned()
            .unwrap_or_else(|| "medium".to_string())
    };
    ThinkingChoice { enable: Some(true), effort: Some(effort) }
}

/// 组装上游请求体（源实现 buildUpstreamBody）。
///
/// `session_seed`：「同一段对话复用同一 session」——下游给了 user 或
/// session_id 时用它派生，否则每次新建（一次性会话）。
#[allow(clippy::too_many_arguments)]
pub fn build_upstream_body(
    upstream_key: &str,
    model_config: &Value,
    messages: &[Value],
    tools: Option<&Vec<Value>>,
    max_tokens: Option<i64>,
    thinking: &ThinkingChoice,
    user_id: &str,
    session_seed: Option<&str>,
) -> Value {
    let limit = max_tokens
        .filter(|value| *value > 0)
        .map(|value| value.min(MAX_OUTPUT_TOKENS))
        .unwrap_or(MAX_OUTPUT_TOKENS);
    let record_id = record_id_for(upstream_key, messages, tools, limit);

    // 取最后一条用户文本：上游要求带在 context 与 business 里
    let mut last_user_text = String::new();
    for message in messages.iter().rev() {
        if message.get("role").and_then(Value::as_str) == Some("user") {
            last_user_text = content_to_text(message.get("content").unwrap_or(&Value::Null));
            break;
        }
    }

    let mut parameters = serde_json::Map::new();
    parameters.insert("max_tokens".to_string(), Value::from(limit));
    // thinking 为 None 表示「不指定」——不能落成 false（Qwen3.8 系列行为异常）
    match thinking.enable {
        Some(true) => {
            parameters.insert("enable_thinking".to_string(), Value::Bool(true));
            if let Some(effort) = &thinking.effort {
                parameters.insert("reasoning_effort".to_string(), Value::String(effort.clone()));
            }
        }
        Some(false) => {
            parameters.insert("enable_thinking".to_string(), Value::Bool(false));
        }
        None => {}
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let session_id = session_id_for(user_id, upstream_key, session_seed);

    json!({
        "request_id": request_id,
        "request_set_id": record_id,
        "chat_record_id": record_id,
        "session_id": session_id,
        "stream": true,
        "chat_task": CHAT_TASK,
        "is_reply": true,
        "is_retry": false,
        "source": 1,
        "version": "3",
        "session_type": SESSION_TYPE,
        "agent_id": AGENT_ID,
        "task_id": "common",
        "code_language": "",
        "chat_prompt": "",
        "image_urls": Value::Null,
        "aliyun_user_type": "",
        // 上游不看这个顶层字段，system 提示要放进 messages 里
        "system": "",
        "messages": messages,
        "tools": tools.cloned().unwrap_or_default(),
        "parameters": Value::Object(parameters),
        "chat_context": {
            "chatPrompt": "",
            "imageUrls": Value::Null,
            "extra": {
                "context": [],
                "modelConfig": {
                    "key": upstream_key,
                    "is_reasoning": model_config
                        .get("is_reasoning")
                        .map(truthy)
                        .unwrap_or(false),
                },
                "originalContent": last_user_text,
            },
            "features": [],
            "text": last_user_text,
        },
        "model_config": slim_model_config(model_config, upstream_key),
        "business": {
            "product": "cli",
            "version": "1.0.0",
            "type": "agent",
            "stage": "start",
            "id": uuid::Uuid::new_v4().to_string(),
            "name": last_user_text.chars().take(30).collect::<String>(),
            "begin_at": now_ms(),
        },
    })
}

/// 精简发往上游的 model_config（源实现 slimModelConfig）。
///
/// **必须剥掉 thinking_config**：目录条目原样回传会覆盖
/// parameters.enable_thinking，导致「关闭思考」失效。
pub fn slim_model_config(config: &Value, upstream_key: &str) -> Value {
    let mut slim = serde_json::Map::new();
    slim.insert("key".to_string(), Value::String(upstream_key.to_string()));
    for key in ["is_reasoning", "is_vl", "source", "format"] {
        if let Some(value) = config.get(key) {
            if !value.is_null() {
                slim.insert(key.to_string(), value.clone());
            }
        }
    }
    Value::Object(slim)
}

/// 会话 id：同一账号 + 同一模型 + 同一种子派生同一个（源实现 sessionIdFor）。
/// 种子为空时退化成一次性会话（每次请求新建），与源实现同一语义。
fn session_id_for(user_id: &str, upstream_key: &str, seed: Option<&str>) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"qoder-session");
    hasher.update([0u8]);
    hasher.update(user_id.as_bytes());
    hasher.update([0u8]);
    hasher.update(upstream_key.as_bytes());
    let base = format!("{:x}", hasher.finalize());
    let base: String = base.chars().take(16).collect();
    match seed.filter(|value| !value.is_empty()) {
        Some(seed) => format!("{base}-{seed}"),
        None => format!("{base}-{}", uuid::Uuid::new_v4()),
    }
}

/// 请求指纹（源实现 recordIdFor）：进 chat_record_id / request_set_id。
/// 同一段消息 + 同一个模型 + 同一个输出上限 → 同一个记录 id，
/// 上游据此识别「这是同一次对话的延续」。
fn record_id_for(
    upstream_key: &str,
    messages: &[Value],
    tools: Option<&Vec<Value>>,
    max_tokens: i64,
) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"qoder-record");
    hasher.update([0u8]);
    hasher.update(upstream_key.as_bytes());
    for message in messages {
        if let Some(role) = message.get("role").and_then(Value::as_str) {
            hasher.update([0u8]);
            hasher.update(role.as_bytes());
        }
        if let Some(content) = message.get("content").filter(|value| !value.is_null()) {
            hasher.update([0u8]);
            match content {
                Value::String(text) => hasher.update(text.as_bytes()),
                other => hasher.update(other.to_string().as_bytes()),
            }
        }
    }
    if let Some(tools) = tools {
        hasher.update([0u8]);
        hasher.update(Value::Array(tools.clone()).to_string().as_bytes());
    }
    hasher.update([0u8]);
    hasher.update(format!("mt={max_tokens}").as_bytes());
    format!("{:x}", hasher.finalize()).chars().take(16).collect()
}

/// 请求体构造总入口（p3-3-wire 路由层调用）：客户端 OpenAI body + 目录条目 +
/// 账号 uid → 上游 agent 信封字节。返回 Err 的唯一路径是序列化失败。
///
/// 会话种子取下游 body 的 session_id / user 字段（同段对话复用 session）。
#[allow(dead_code)] // p3-3-wire（qoder_route）消费
pub fn prepare_qoder_body(
    peek: &Value,
    entry: &Value,
    user_id: &str,
) -> Result<Vec<u8>, String> {
    let model_key = entry
        .get("upstreamKey")
        .and_then(Value::as_str)
        .unwrap_or("");
    if model_key.is_empty() {
        return Err("Qoder 目录条目缺少 upstreamKey".to_string());
    }
    let config = entry.get("config").cloned().unwrap_or_else(|| json!({}));
    let empty: Vec<Value> = Vec::new();
    let messages_v = peek
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or(empty);
    let messages = normalize_messages(&messages_v);
    let tools = normalize_tools(peek.get("tools"));
    let thinking = resolve_thinking(peek, entry);
    // 审查 P3：OpenAI 新客户端只发 max_completion_tokens（老键已弃用），
    // 对齐 wb_upstream payload.rs 的键序兼容
    let max_tokens = peek
        .get("max_tokens")
        .or_else(|| peek.get("max_completion_tokens"))
        .and_then(Value::as_i64);
    // 审查 P3：session_seed 进上游请求体，超长值放大请求体积——限长 128 字符
    //（按字符截断，防非 ASCII 字节切片 panic）
    let session_seed = peek
        .get("session_id")
        .and_then(Value::as_str)
        .or_else(|| peek.get("user").and_then(Value::as_str))
        .map(|s| s.chars().take(128).collect::<String>());
    let mut body = build_upstream_body(
        model_key,
        &config,
        &messages,
        tools.as_ref(),
        max_tokens,
        &thinking,
        user_id,
        session_seed.as_deref(),
    );
    // 上下文档位（官方客户端 `parameters.context_length` 数值字段，不选不发）：
    // 仅条目带覆盖目标时插入，未设置时请求体与改造前一致
    if let Some(n) = entry.get(CONTEXT_TARGET_KEY).and_then(Value::as_u64) {
        if let Some(params) = body.get_mut("parameters").and_then(Value::as_object_mut) {
            params.insert("context_length".to_string(), Value::from(n));
        }
    }
    serde_json::to_vec(&body).map_err(|e| format!("Qoder 请求体序列化失败: {e}"))
}



#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_server::wb_sse;

    // ── 工具 ──

    /// 构造一帧上游信封行（body 为内层 JSON 字符串）
    fn envelope(inner: &str) -> String {
        format!("data: {}", json!({"statusCodeValue": 200, "body": inner}))
    }

    /// 取一帧输出的内层 JSON（测试代码允许 unwrap 系）
    fn delta_of(frame: &str) -> Value {
        assert!(frame.starts_with("data: "), "帧必须以 data: 开头: {frame}");
        serde_json::from_str(&frame["data: ".len()..]).unwrap_or(Value::Null)
    }

    /// 取一帧里 choices[0].delta 某字段的文本（拷贝，避免临时值借用）
    fn delta_text(frame: &str, key: &str) -> Option<String> {
        let v = delta_of(frame);
        v["choices"][0]["delta"][key].as_str().map(str::to_string)
    }

    /// 喂一组上游行，收集全部输出帧 + 错误槽
    fn run(lines: Vec<String>) -> (Vec<String>, Arc<Mutex<Option<ErrMeta>>>) {
        let slot: Arc<Mutex<Option<ErrMeta>>> = Arc::new(Mutex::new(None));
        let t = QoderTranslate::new(lines.into_iter(), slot.clone(), "chat-1", "M");
        (t.collect(), slot)
    }

    // ── 翻译器：信封拆解 ──

    #[test]
    fn envelope_chunk_translated_to_openai_frame() {
        let (out, _) = run(vec![
            envelope(r#"{"id":"u1","choices":[{"delta":{"content":"你"}}]}"#),
            "data: [DONE]".to_string(),
        ]);
        assert_eq!(out.len(), 2, "应产出 1 chunk + [DONE]，实际: {out:?}");
        let v = delta_of(&out[0]);
        assert_eq!(v["object"], "chat.completion.chunk");
        assert_eq!(v["id"], "u1", "上游 id 非空时透传");
        assert_eq!(v["model"], "M");
        assert_eq!(v["choices"][0]["delta"]["content"], "你");
        assert_eq!(v["choices"][0]["finish_reason"], "");
        assert_eq!(out[1], "data: [DONE]");
    }

    #[test]
    fn chunk_id_falls_back_to_chat_id() {
        let (out, _) = run(vec![envelope(r#"{"choices":[{"delta":{"content":"x"}}]}"#)]);
        let v = delta_of(&out[0]);
        assert_eq!(v["id"], "chat-1", "上游无 id 时回退 chat_id");
    }

    #[test]
    fn multiline_data_assembly() {
        // SSE 规范多行 data 拼接：信封载荷被切成两行 data: 时不完整 → 等下一行拼齐
        //（真实上游恒发信封形态，裸内层 JSON 不是合法输入）
        let text = json!({
            "statusCodeValue": 200,
            "body": r#"{"choices":[{"delta":{"content":"split"}}]}"#,
        })
        .to_string();
        let half = text.len() / 2; // 信封 JSON 全 ASCII（内层已转义），字节切分安全
        let (a, b) = text.split_at(half);
        let (out, _) = run(vec![
            format!("data: {a}"),
            format!("data: {b}"),
            "data: [DONE]".to_string(),
        ]);
        assert_eq!(out.len(), 2, "两行 data 拼成一条 chunk + [DONE]");
        let v = delta_of(&out[0]);
        assert_eq!(v["choices"][0]["delta"]["content"], "split");
    }

    #[test]
    fn compact_stream_without_blank_lines() {
        // 上游不按空行分帧：每条 data 行载荷完整时立即产出（紧凑流）
        let (out, _) = run(vec![
            envelope(r#"{"choices":[{"delta":{"content":"a"}}]}"#),
            envelope(r#"{"choices":[{"delta":{"content":"b"}}]}"#),
            "data: [DONE]".to_string(),
        ]);
        assert_eq!(out.len(), 3);
        let c: String = out
            .iter()
            .filter_map(|f| delta_text(f, "content"))
            .collect();
        assert_eq!(c, "ab");
    }

    #[test]
    fn sse_comment_and_unknown_lines_ignored() {
        let (out, _) = run(vec![
            ": keep-alive".to_string(),
            "event: ping".to_string(),
            envelope(r#"{"choices":[{"delta":{"content":"a"}}]}"#),
            "data: [DONE]".to_string(),
        ]);
        assert_eq!(out.len(), 2, "注释与 event: 行不产出帧");
    }

    // ── 翻译器：错误路径 ──

    #[test]
    fn error_frame_writes_slot_and_omits_done() {
        let (out, slot) = run(vec![
            r#"data: {"statusCodeValue":110,"body":"{\"code\":\"110\",\"message\":\"no quota\"}"}"#
                .to_string(),
        ]);
        assert_eq!(out.len(), 1, "错误帧后不应有 [DONE]，实际: {out:?}");
        let v = delta_of(&out[0]);
        assert_eq!(v["error"]["code"], 110);
        assert!(v["error"]["message"].as_str().unwrap_or("").contains("额度"));
        let meta = slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .expect("err_slot 必须被写入");
        assert!(matches!(meta.kind, UpstreamKind::Quota));
        assert_eq!(meta.status, 110);
    }

    // ── 首帧换号无损语义（agent2api issue #8 同型保障的本地回归锁，2026-10-03）──
    //
    // 保障链：QoderTranslate 首帧即信封错误 → 单发错误帧、不发 [DONE] →
    // WbSseParser 解析为 WbEvent::Error → stream_forward_ex 在 !sent_any 时
    // 上抛 error_info → qoder_route break 换号。空 delta 帧（role-only 等）被
    // 翻译器整帧丢弃，不会把 sent_any 提前置 true——这是「首帧预读换号」在
    // 本地架构下的等价实现（错误帧本身就是预读结果，无需额外 prefetch 层）。

    /// 翻译 + 流式转发的端到端测试通道：返回 (error_info, sent_any, 已下发帧)
    fn forward_rot(
        lines: Vec<String>,
        proto: crate::api_server::routes::Protocol,
    ) -> (Option<(i64, String)>, bool, Vec<String>) {
        let slot: Arc<Mutex<Option<ErrMeta>>> = Arc::new(Mutex::new(None));
        let t = QoderTranslate::new(lines.into_iter(), slot.clone(), "chat-1", "M");
        let ilines = InterruptibleLines::from_iterator(Box::new(t));
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
        let (err, sent_any, _fi, _u) = wb_sse::stream_forward_ex(ilines, &tx, proto, "chat-1", "M", &std::collections::HashSet::new(), None);
        drop(tx);
        let mut sent = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            sent.push(String::from_utf8_lossy(&frame.unwrap()).to_string());
        }
        (err, sent_any, sent)
    }

    const ERR_ENVELOPE: &str =
        r#"data: {"statusCodeValue":110,"body":"{\"code\":\"110\",\"message\":\"no quota\"}"}"#;

    /// 信封错误为首帧：无字节下发、错误上抛 → 路由层换号无损
    #[test]
    fn envelope_error_before_content_uplifts_for_rotation() {
        let (err, sent_any, sent) =
            forward_rot(vec![ERR_ENVELOPE.to_string()], crate::api_server::routes::Protocol::OpenAi);
        assert!(err.is_some(), "错误必须上抛供路由层换号");
        assert!(!sent_any);
        assert!(sent.is_empty(), "不得向客户端下发任何帧: {sent:?}");
    }

    /// 空 delta（role-only）帧在错误之前到达：翻译器整帧丢弃，错误仍在
    /// 「首内容帧前」→ 换号无损（这正是 agent2api emit_chunk「空 delta 不发」
    /// 语义防住的坑：role 帧一旦下发，sent_any 提前置 true，换号永久失效）
    #[test]
    fn role_only_chunk_then_error_still_uplifts() {
        let (err, sent_any, sent) = forward_rot(
            vec![
                envelope(r#"{"choices":[{"delta":{"role":"assistant"}}]}"#),
                ERR_ENVELOPE.to_string(),
            ],
            crate::api_server::routes::Protocol::OpenAi,
        );
        assert!(err.is_some());
        assert!(!sent_any);
        assert!(sent.is_empty(), "role-only 帧不得下发: {sent:?}");
    }

    /// Responses 协议同型：错误先于任何 chunk → 不得发出 response.created
    ///（created 一旦发出即 sent_any=true，错误只能就地下发，换号失效）
    #[test]
    fn responses_protocol_error_first_sends_no_created() {
        let (err, sent_any, sent) = forward_rot(
            vec![ERR_ENVELOPE.to_string()],
            crate::api_server::routes::Protocol::Responses,
        );
        assert!(err.is_some());
        assert!(!sent_any);
        assert!(
            sent.iter().all(|f| !f.contains("response.created")),
            "错误首帧不得触发 response.created: {sent:?}"
        );
    }

    /// 对照组：真实内容帧已发出后再遇错误 → failed_inline 就地收尾（不换号，
    /// 防重复流）——保证上面的换号语义只作用于「首内容帧前」窗口
    #[test]
    fn error_after_content_is_inline_not_rotation() {
        let (err, sent_any, failed_inline, _u) = {
            let slot: Arc<Mutex<Option<ErrMeta>>> = Arc::new(Mutex::new(None));
            let t = QoderTranslate::new(
                vec![
                    envelope(r#"{"choices":[{"delta":{"content":"部分输出"}}]}"#),
                    ERR_ENVELOPE.to_string(),
                ]
                .into_iter(),
                slot.clone(),
                "chat-1",
                "M",
            );
            let ilines = InterruptibleLines::from_iterator(Box::new(t));
            let (tx, _rx) =
                tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
            wb_sse::stream_forward_ex(ilines, &tx, crate::api_server::routes::Protocol::OpenAi, "chat-1", "M", &std::collections::HashSet::new(), None)
        };
        assert!(err.is_none(), "内容已流出，错误不得上抛换号");
        assert!(sent_any);
        assert!(failed_inline, "内容后错误必须就地收尾");
    }

    // ── 传输层掐断（issue #158 P2-2）：EOF 伴随读错误 → 不补 [DONE] + TransportCut ──

    /// 预填读错误的共享槽
    fn cut_slot(msg: &str) -> crate::api_server::wb_upstream::ReadErrSlot {
        let s = crate::api_server::wb_upstream::new_read_err_slot();
        *s.lock().unwrap_or_else(|e| e.into_inner()) = Some(msg.to_string());
        s
    }

    fn meta_of(err_slot: &Arc<Mutex<Option<ErrMeta>>>) -> Option<ErrMeta> {
        err_slot.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// EOF 伴随读错误：不补 [DONE]（流是断的不是完的）+ 写 TransportCut ErrMeta；
    /// 已产出的内容帧保留（就地收尾口径）
    #[test]
    fn transport_cut_eof_skips_done_and_writes_errmeta() {
        let err_slot: Arc<Mutex<Option<ErrMeta>>> = Arc::new(Mutex::new(None));
        let t = QoderTranslate::with_read_slot(
            vec![envelope(r#"{"choices":[{"delta":{"content":"部分输出"}}]}"#)].into_iter(),
            err_slot.clone(),
            Some(cut_slot("h2 RST_STREAM pushed")),
            "chat-1",
            "M",
        );
        let frames: Vec<String> = t.collect();
        assert!(
            !frames.iter().any(|f| f.contains("[DONE]")),
            "掐断流不得补 [DONE]: {frames:?}"
        );
        assert!(frames.iter().any(|f| f.contains("部分输出")), "已产出内容帧保留");
        let meta = meta_of(&err_slot).expect("EOF 掐断必须写 ErrMeta");
        assert!(matches!(meta.kind, UpstreamKind::TransportCut));
        assert!(meta.message.starts_with("transport cut:"), "实际: {}", meta.message);
    }

    /// 对照组：正常 EOF（槽空）仍补 [DONE]，err_slot 保持 None
    #[test]
    fn clean_eof_still_emits_done() {
        let err_slot: Arc<Mutex<Option<ErrMeta>>> = Arc::new(Mutex::new(None));
        let t = QoderTranslate::with_read_slot(
            vec![envelope(r#"{"choices":[{"delta":{"content":"hi"}}]}"#)].into_iter(),
            err_slot.clone(),
            Some(crate::api_server::wb_upstream::new_read_err_slot()),
            "chat-1",
            "M",
        );
        let frames: Vec<String> = t.collect();
        assert!(frames.last().map(|f| f.contains("[DONE]")).unwrap_or(false));
        assert!(meta_of(&err_slot).is_none(), "正常完成不写 ErrMeta");
    }

    /// err_slot 已有业务错误时保留之（业务错误更具诊断价值），仍不补 [DONE]
    #[test]
    fn transport_cut_keeps_existing_business_errmeta() {
        let err_slot: Arc<Mutex<Option<ErrMeta>>> = Arc::new(Mutex::new(None));
        let t = QoderTranslate::with_read_slot(
            vec![ERR_ENVELOPE.to_string()].into_iter(),
            err_slot.clone(),
            Some(cut_slot("connection reset")),
            "chat-1",
            "M",
        );
        let frames: Vec<String> = t.collect();
        let meta = meta_of(&err_slot).expect("业务错误帧应写 ErrMeta");
        assert!(
            matches!(meta.kind, UpstreamKind::Quota),
            "业务错误不得被 TransportCut 覆盖"
        );
        assert!(!frames.iter().any(|f| f.contains("[DONE]")));
    }

    // ── 翻译器：思考拆解 ──

    #[test]
    fn thinking_split_across_chunks() {
        // 标签被上游切在任意位置：<thi + nking>abc</thi + nking>def
        let (out, _) = run(vec![
            envelope(r#"{"choices":[{"delta":{"content":"<thi"}}]}"#),
            envelope(r#"{"choices":[{"delta":{"content":"nking>abc</thi"}}]}"#),
            envelope(r#"{"choices":[{"delta":{"content":"nking>def"}}]}"#),
            "data: [DONE]".to_string(),
        ]);
        let mut reasoning = String::new();
        let mut content = String::new();
        for frame in &out {
            let v = delta_of(frame);
            if let Some(t) = v["choices"][0]["delta"]["reasoning_content"].as_str() {
                reasoning.push_str(t);
            }
            if let Some(t) = v["choices"][0]["delta"]["content"].as_str() {
                content.push_str(t);
            }
        }
        assert_eq!(reasoning, "abc", "拆出的思考进 reasoning_content");
        assert_eq!(content, "def", "拆出的正文进 content，碎片不漏");
    }

    #[test]
    fn reasoning_content_stripped_and_direct() {
        // reasoning_content 不进拆解器，剥残留标签后直发
        let (out, _) = run(vec![
            envelope(r#"{"choices":[{"delta":{"reasoning_content":"<think>deep</think>"}}]}"#),
            "data: [DONE]".to_string(),
        ]);
        let r: String = out
            .iter()
            .filter_map(|f| delta_text(f, "reasoning_content"))
            .collect();
        assert_eq!(r, "deep");
    }

    #[test]
    fn finish_chunk_carries_usage_and_flushes_tail() {
        let (out, _) = run(vec![
            envelope(r#"{"choices":[{"delta":{"content":"<think>abc"}}]}"#),
            envelope(
                r#"{"id":"u9","choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":5}}"#,
            ),
        ]);
        // 尾帧：空 delta + finish + usage（carry 帧）
        let last = out
            .iter()
            .rev()
            .find(|f| f.as_str() != "data: [DONE]")
            .expect("至少一个 chunk");
        let v = delta_of(last);
        assert_eq!(v["choices"][0]["finish_reason"], "stop");
        assert_eq!(v["usage"]["completion_tokens"], 5);
        assert_eq!(v["choices"][0]["delta"], json!({}));
        // 拆解器尾巴 abc 必须在收尾前以 reasoning_content 发出
        let reasoning: String = out
            .iter()
            .filter_map(|f| delta_text(f, "reasoning_content"))
            .collect();
        assert_eq!(reasoning, "abc");
    }

    #[test]
    fn usage_without_finish_emits_independent_frame() {
        let (out, _) = run(vec![
            envelope(r#"{"id":"u1","choices":[{"delta":{"content":"hi"}}]}"#),
            envelope(r#"{"choices":[{"delta":{}}],"usage":{"prompt_tokens":1,"completion_tokens":2}}"#),
            "data: [DONE]".to_string(),
        ]);
        let v = delta_of(&out[1]);
        assert_eq!(v["choices"][0]["delta"], json!({}));
        assert_eq!(v["choices"][0]["finish_reason"], "");
        assert_eq!(v["usage"]["prompt_tokens"], 1);
    }

    #[test]
    fn tool_calls_pass_whole_delta() {
        let (out, _) = run(vec![
            envelope(
                r#"{"id":"t1","choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"f","arguments":"{}"}}]}}]}"#,
            ),
            "data: [DONE]".to_string(),
        ]);
        let v = delta_of(&out[0]);
        let tc = v["choices"][0]["delta"]["tool_calls"]
            .as_array()
            .expect("tool_calls 必须整体透传");
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0]["function"]["name"], "f");
    }

    #[test]
    fn missing_done_still_flushes() {
        // 上游不发 [DONE]：行耗尽后 flush_finish 兜底
        let (out, _) = run(vec![envelope(r#"{"choices":[{"delta":{"content":"tail"}}]}"#)]);
        assert_eq!(out.last().map(String::as_str), Some("data: [DONE]"));
        let c: String = out
            .iter()
            .filter_map(|f| delta_text(f, "content"))
            .collect();
        assert_eq!(c, "tail");
    }

    #[test]
    fn bridge_to_aggregate_end_to_end() {
        // open_qoder_stream（首字超时 + from_iterator 桥接）→ wb_sse::aggregate
        let text = format!(
            "{}\n{}\ndata: [DONE]\n",
            envelope(r#"{"choices":[{"delta":{"content":"你好"}}]}"#),
            envelope(
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":2}}"#,
            ),
        );
        let slot: Arc<Mutex<Option<ErrMeta>>> = Arc::new(Mutex::new(None));
        let reader = std::io::Cursor::new(text.into_bytes());
        let ilines = open_qoder_stream(Box::new(reader), slot, "chat-1", "M")
            .expect("首字正常应成功");
        let (completion, err) = wb_sse::aggregate(ilines, "chat-1", None);
        assert!(err.is_none(), "无流内错误");
        let completion = completion.expect("必须聚合出 completion");
        assert_eq!(completion["choices"][0]["message"]["content"], "你好");
        assert_eq!(completion["choices"][0]["finish_reason"], "stop");
        assert_eq!(completion["usage"]["completion_tokens"], 2);
    }

    // ── 错误分类：判定链顺序 ──

    #[test]
    fn queued_10605_beats_quota_keywords() {
        // 排队正文里带 "credit" 泛词：排队特征必须前置于额度关键词（判定链①）
        let body = r#"{"code":"403","message":"{\"code\":\"10605\",\"isQueued\":true,\"serviceAvailable\":false,\"queueType\":\"p3\",\"retryAfterSeconds\":30,\"message\":\"credit exhausted\"}"}"#;
        let c = classify_upstream_error(403, body);
        assert!(matches!(c.kind, UpstreamKind::Queued), "实际: {:?}", c.kind);
        let queue = c.queue.expect("Queued 必须带队列信号");
        assert_eq!(queue.retry_after_secs, Some(30));
        assert_eq!(queue.queue_type.as_deref(), Some("p3"));
        assert!(c.message.contains("排队"));
        assert!(c.message.contains("不是登录态或额度问题"));
    }

    #[test]
    fn business_code_105_beats_http_status() {
        // 403 + 业务码 105 → Auth（业务码前置于状态码，判定链②）
        let c = classify_upstream_error(
            403,
            r#"{"code":"105","message":"Login or access token expired"}"#,
        );
        assert!(matches!(c.kind, UpstreamKind::Auth));
        assert_eq!(c.message, "登录态已失效，请重新登录");
    }

    #[test]
    fn quota_code_110_in_nested_403_body() {
        let c = classify_upstream_error(403, r#"{"code":"403","message":"{\"code\":\"110\"}"}"#);
        assert!(matches!(c.kind, UpstreamKind::Quota));
    }

    #[test]
    fn bare_403_is_forbidden() {
        let c = classify_upstream_error(403, "permission denied");
        assert!(matches!(c.kind, UpstreamKind::Forbidden));
    }

    #[test]
    fn pricing_url_flags_quota_and_extracts_link() {
        let c = classify_upstream_error(
            403,
            "please upgrade at https://qoder.com/pricing?plan=x today",
        );
        assert!(matches!(c.kind, UpstreamKind::Quota));
        assert_eq!(c.pricing_url.as_deref(), Some("https://qoder.com/pricing?plan=x"));
    }

    #[test]
    fn status_429_is_rate() {
        let c = classify_upstream_error(429, "slow down");
        assert!(matches!(c.kind, UpstreamKind::Rate));
    }

    /// 审查 P1 防回归：401 正文含 "credentials"（泛词 credit 的超串）不得误判
    /// Quota（HardCredit 封号一整天）——状态码特判须前置于额度泛词链
    #[test]
    fn status_401_with_credentials_word_is_auth_not_quota() {
        let c = classify_upstream_error(401, r#"{"error":"invalid credentials"}"#);
        assert!(matches!(c.kind, UpstreamKind::Auth), "实际: {:?}", c.kind);
    }

    /// 审查 P1 防回归：429 正文含 "rate limit exceeded"（泛词 exceeded 命中）
    /// 不得误判 Quota——限流按 SoftRate 冷却而非封号到次日
    #[test]
    fn status_429_with_exceeded_word_is_rate_not_quota() {
        let c = classify_upstream_error(429, "rate limit exceeded, please retry later");
        assert!(matches!(c.kind, UpstreamKind::Rate), "实际: {:?}", c.kind);
    }

    #[test]
    fn server_status_5xx() {
        let c = classify_upstream_error(500, "internal error");
        assert!(matches!(c.kind, UpstreamKind::Server));
    }

    #[test]
    fn err_kind_mapping() {
        assert!(matches!(
            UpstreamKind::Quota.to_err_kind(),
            crate::api_server::ErrKind::HardCredit
        ));
        assert!(matches!(
            UpstreamKind::Rate.to_err_kind(),
            crate::api_server::ErrKind::SoftRate
        ));
        assert!(matches!(
            UpstreamKind::Auth.to_err_kind(),
            crate::api_server::ErrKind::SessionDead
        ));
        assert!(matches!(
            UpstreamKind::Forbidden.to_err_kind(),
            crate::api_server::ErrKind::Forbidden
        ));
        assert!(matches!(
            UpstreamKind::Server.to_err_kind(),
            crate::api_server::ErrKind::Server
        ));
        assert!(matches!(
            UpstreamKind::Unknown.to_err_kind(),
            crate::api_server::ErrKind::Server
        ));
        assert!(matches!(
            UpstreamKind::Queued.to_err_kind(),
            crate::api_server::ErrKind::None
        ));
    }

    // ── 目录 ──

    /// p3-2d 真实样本回归（#[ignore]：依赖本地实抓文件）：
    /// parse_catalog 吃 2026-10-01 model/list 全量实抓 JSON（真 COSY 签名直通
    /// 探针落盘，67856B）→ 13 条目（实抓 14 条减去产品决策下线的 Auto）+
    /// MiniMax-M2.7/GLM-5.3 倍率关键字段验证。
    /// 运行：cargo test parse_catalog_eats_real -- --ignored --nocapture
    #[test]
    #[ignore]
    fn parse_catalog_eats_real_model_list_snapshot() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../temp/qoder_model_list_full.json");
        let raw = std::fs::read_to_string(&path)
            .expect("实抓文件缺失（temp/qoder_model_list_full.json）");
        let payload: Value = serde_json::from_str(&raw).expect("实抓 JSON 解析失败");
        let models = parse_catalog(&payload);
        assert_eq!(models.len(), 13, "chat 数组 14 条减下线的 Auto 应解析出 13 个模型");
        assert!(
            models.iter().all(|m| text_of(m, "id").to_lowercase() != "auto"),
            "下线模型 Auto 不得出现在解析结果" 
        );
        let minimax = models
            .iter()
            .find(|m| text_of(m, "id") == "MiniMax-M2.7")
            .unwrap_or_else(|| panic!("缺 MiniMax-M2.7（上游已改名）"));
        assert_eq!(text_of(minimax, "upstreamKey"), "mmodel");
        // GLM-5.3 倍率应如实抓 0.8（蓝本快照 0.6 已漂移，智能排序键消费此值）
        let glm = models
            .iter()
            .find(|m| text_of(m, "id") == "GLM-5.3")
            .expect("缺 GLM-5.3");
        assert_eq!(credits_rate_of(&text_of(glm, "credits")), Some(0.8));
        // qfmodel 免费 0.0（price_factor=0 是合法值不可当缺失）
        let flash = models
            .iter()
            .find(|m| text_of(m, "id") == "Qwen3.8-Flash")
            .expect("缺 Qwen3.8-Flash");
        assert_eq!(credits_rate_of(&text_of(flash, "credits")), Some(0.0));
    }

    #[test]
    fn resolve_fallback_by_id_name_and_key() {
        let hit_key = resolve("qfmodel", QoderRegion::Cn).expect("upstreamKey 应可命中");
        assert_eq!(hit_key["id"], "Qwen3.8-Flash");
        let hit_id = resolve("Qwen3.8-Flash", QoderRegion::Global).expect("id 应可命中");
        assert_eq!(hit_id["upstreamKey"], "qfmodel");
        // 大小写与首尾空白容错
        assert!(resolve("  QWEN3.8-FLASH ", QoderRegion::Cn).is_some());
        assert!(resolve("definitely-not-a-model", QoderRegion::Global).is_none());
        assert!(resolve("   ", QoderRegion::Global).is_none());
    }

    #[test]
    fn list_union_dedups_global_first() {
        // list() 在远程目录非空时替换兜底表：持测试锁避免读到并行
        // adopt_remote 测试注入的瞬时状态（resolve 类测试有兜底回退不受影响）
        let _catalog = catalog_test_guard();
        let items = list();
        let flash: Vec<&Value> = items.iter().filter(|m| m["id"] == "Qwen3.8-Flash").collect();
        assert_eq!(flash.len(), 1, "并集必须按 id 去重");
        assert_eq!(flash[0]["region"], "global", "去重时 global 优先");
        assert_eq!(flash[0]["credits"], "x0.08 credits");
        assert_eq!(flash[0]["config"]["key"], "qfmodel");
        assert_eq!(flash[0]["maxOutputTokens"], MAX_OUTPUT_TOKENS);
        assert_eq!(flash[0]["supportsToolCall"], true);
    }

    // ── issue #111：免费档判定（零积分豁免的判定依据） ──

    /// 倍率 0 是合法免费值；credits 键缺失（上游没给倍率）按非免费 fail-closed
    #[test]
    fn entry_is_free_by_zero_rate() {
        assert!(entry_is_free(&serde_json::json!({ "credits": "x0 credits" })));
        assert!(entry_is_free(&serde_json::json!({ "credits": "x0.0 credits" })));
        assert!(!entry_is_free(&serde_json::json!({ "credits": "x0.1 credits" })));
        assert!(!entry_is_free(&serde_json::json!({ "credits": "" })));
        assert!(!entry_is_free(&serde_json::json!({})));
    }

    /// 兜底目录实测：CN 区 Qwen3.8-Flash 现价 0.08 非免费（2026-10-16 远程
    /// 实抓对齐，10 初的活动免费 0.0 已结束）；其余模型非免费
    #[test]
    fn fallback_catalog_free_model_detection() {
        let flash = resolve("Qwen3.8-Flash", QoderRegion::Cn).expect("Qwen3.8-Flash 应在 CN 兜底表");
        assert!(
            !entry_is_free(&flash),
            "Qwen3.8-Flash 活动免费已结束（0.08）不应判免费"
        );
        let max = resolve("Qwen3.8-Max", QoderRegion::Cn).expect("Qwen3.8-Max 应在 CN 兜底表");
        assert!(!entry_is_free(&max), "Qwen3.8-Max 倍率 0.5 非免费");
    }

    /// 免费活动覆盖（issue #111）：开关开启时 Qwen3.8-Flash 条目倍率改写 0
    /// → entry_is_free 判真；关闭时保持官方同步倍率；其余模型不受影响；
    /// 命名漂移（连字符/大小写）经 norm_model_id 归一后同样命中
    #[test]
    fn flash_free_override_rewrites_credits_only_for_flash() {
        // 开启：flash 条目改写 0（目录价 0.1 覆盖），豁免判定联动为真
        let mut flash = resolve("Qwen3.8-Flash", QoderRegion::Cn).expect("flash 应在 CN 兜底表");
        apply_flash_free_override(&mut flash, true);
        assert_eq!(flash["credits"], "x0.0 credits", "开启时倍率应改写为 0");
        assert!(entry_is_free(&flash), "改写后应判免费（取号豁免联动）");
        assert_eq!(
            flash["id"], "Qwen3.8-Flash",
            "覆盖只改倍率，不动 id/其余字段"
        );
        // 关闭：保持官方同步倍率（上游活动结束后恢复计费口径）
        let mut flash_off = resolve("Qwen3.8-Flash", QoderRegion::Cn).expect("flash 应在 CN 兜底表");
        apply_flash_free_override(&mut flash_off, false);
        assert!(
            !entry_is_free(&flash_off),
            "关闭时保持官方倍率不判免费"
        );
        // 命名漂移（空格/大小写变体，norm_model_id 归一保留小数点）与非 flash 模型不受影响
        let mut drift = serde_json::json!({ "id": "Qwen 3.8 FLASH", "credits": "x0.1 credits" });
        apply_flash_free_override(&mut drift, true);
        assert_eq!(drift["credits"], "x0.0 credits", "归一后命名漂移同样命中");
        let mut other = serde_json::json!({ "id": "Qwen3.8-Max", "credits": "x0.5 credits" });
        apply_flash_free_override(&mut other, true);
        assert_eq!(other["credits"], "x0.5 credits", "非 flash 模型不改写");
        // 便捷组合：resolve_overridden 与 apply 语义一致
        let combined = resolve_overridden("Qwen3.8-Flash", QoderRegion::Cn, true)
            .expect("resolve_overridden 应命中");
        assert!(entry_is_free(&combined));
        assert!(
            resolve_overridden("Qwen3.8-Flash", QoderRegion::Cn, false)
                .map(|e| !entry_is_free(&e))
                .unwrap_or(false),
            "关闭时 resolve_overridden 保持非免费"
        );
        // list_overridden 全量覆盖：flash 改写、非 flash 保持
        let listed = list_overridden(true);
        let f = listed
            .iter()
            .find(|m| m.get("id").and_then(Value::as_str) == Some("Qwen3.8-Flash"))
            .expect("list 应含 flash");
        assert_eq!(f["credits"], "x0.0 credits");
        let mx = listed
            .iter()
            .find(|m| m.get("id").and_then(Value::as_str) == Some("Qwen3.8-Max"))
            .expect("list 应含 max");
        assert_eq!(mx["credits"], "x0.5 credits");
        // issue #156：DeepSeek-Flash 纳入免费活动集合——两区目录均被覆盖改写 0，
        // 取号豁免联动为真；关闭时保持官方倍率（CN 0.1 / Global 0.2）
        for region in [QoderRegion::Cn, QoderRegion::Global] {
            let mut df = resolve("DeepSeek-Flash", region).expect("DeepSeek-Flash 应在兜底表");
            assert!(
                !entry_is_free(&df),
                "覆盖前目录价非 0，不应判免费"
            );
            apply_flash_free_override(&mut df, true);
            assert_eq!(
                df["credits"], "x0.0 credits",
                "DeepSeek-Flash 开启时应改写为 0"
            );
            assert!(entry_is_free(&df), "DeepSeek-Flash 改写后应判免费");
        }
        assert!(
            resolve_overridden("DeepSeek-Flash", QoderRegion::Cn, false)
                .map(|e| !entry_is_free(&e))
                .unwrap_or(false),
            "DeepSeek-Flash 关闭时 resolve_overridden 保持非免费"
        );
        // is_flash_free_model 归一比较：命名漂移命中、非集合模型不命中
        assert!(is_flash_free_model("deepseek-flash"));
        assert!(is_flash_free_model("DeepSeek  FLASH"));
        assert!(!is_flash_free_model("DeepSeek-V4-Pro"));
    }

    #[test]
    fn factor_formatting() {
        // 0.04 是合法关键值（两位小数）；整数不带小数点；尾零裁掉
        assert_eq!(format_factor(0.04), "0.04");
        assert_eq!(format_factor(0.5), "0.5");
        assert_eq!(format_factor(1.0), "1");
        assert_eq!(format_factor(3.2), "3.2");
        assert_eq!(format_factor(2.0), "2");
    }

    #[test]
    fn adopt_remote_parses_and_resolves() {
        // 采纳会写进程级目录：持测试锁独占，丢弃时自动复位回落兜底
        let _catalog = catalog_test_guard();
        let payload = json!({
            "statusCodeValue": 200,
            "chat": [
                {
                    "key": "utest_key_a",
                    "display_name": "UTest Model A",
                    "enable": true,
                    "is_vl": true,
                    "is_reasoning": true,
                    "price_factor": 0.5,
                    "max_input_tokens": 128000,
                    "source": "remote",
                },
            ],
        });
        let n = adopt_remote(QoderRegion::Cn, &payload).expect("合法信封必须被采纳");
        assert_eq!(n, 1);
        let hit = resolve("UTestModelA", QoderRegion::Cn).expect("去空白 id 应可命中");
        assert_eq!(hit["upstreamKey"], "utest_key_a");
        assert_eq!(hit["supportsImages"], true);
        assert_eq!(hit["enabled"], true);
        assert_eq!(hit["credits"], "x0.5 credits");
        assert_eq!(hit["maxInputTokens"], 128000);
        assert_eq!(hit["config"]["max_input_tokens"], 128000);
        // 业务错误信封 → Err
        assert!(adopt_remote(
            QoderRegion::Cn,
            &json!({"statusCodeValue": 401, "message": "bad"})
        )
        .is_err());
        // 空目录 → Err
        assert!(adopt_remote(QoderRegion::Cn, &json!({"chat": []})).is_err());
    }

    /// 产品决策下线模型回归（REMOVED_MODEL_IDS）：兜底表已删 + 远程带回过滤，
    /// 目录 / 解析全链路不可再出现 Auto/Sonus/Cantus/Ultimate/Performance/Efficient
    #[test]
    fn removed_models_filtered_from_catalog_and_resolve() {
        // 持测试锁：resolve/list 会读进程级目录（含并行 adopt_remote 注入态）
        let _catalog = catalog_test_guard();
        // 远程清单带回下线模型（含带空白/大小写漂移形态）→ 全部丢弃
        let payload = json!({
            "statusCodeValue": 200,
            "chat": [
                {"key": "auto", "display_name": "Auto", "enable": true, "price_factor": 0.5},
                {"key": "smodel", "display_name": " Sonus ", "enable": true, "price_factor": 3.2},
                {"key": "cmodel", "display_name": "CANTUS", "enable": true, "price_factor": 3.2},
                {"key": "ultimate", "display_name": "Ultimate", "enable": true, "price_factor": 1.6},
                {"key": "performance", "display_name": "Performance", "enable": true, "price_factor": 1.1},
                {"key": "efficient", "display_name": "Efficient", "enable": true, "price_factor": 0.3},
                {"key": "keep_key", "display_name": "Keep Model", "enable": true, "price_factor": 0.1},
            ],
        });
        let models = parse_catalog(&payload);
        assert_eq!(models.len(), 1, "6 个下线模型必须全部过滤");
        assert_eq!(text_of(&models[0], "id"), "KeepModel");
        // 兜底表已删：resolve 双区都解析不到
        for removed in ["Auto", "auto", "Sonus", "Cantus", "Ultimate", "Performance", "Efficient"] {
            assert!(
                resolve(removed, QoderRegion::Cn).is_none(),
                "{removed} 不应再可解析"
            );
            assert!(
                resolve(removed, QoderRegion::Global).is_none(),
                "{removed} 不应再可解析"
            );
        }
        // 并集目录也不含
        let items = list();
        assert!(
            items
                .iter()
                .all(|m| !REMOVED_MODEL_IDS.contains(&text_of(m, "id").to_lowercase().as_str())),
            "list() 不得包含下线模型"
        );
    }

    /// 厂商标注回归：Step 5 Preview（阶跃星辰，产品确认）注入 vendor 键；
    /// 未命中映射的模型不给 vendor 键（前端未命中显示 —）；
    /// 连字符风格 display_name（Step-5-Preview）经 norm_model_id 折叠后仍命中
    #[test]
    fn vendor_annotation_for_step5_preview() {
        let payload = json!({
            "statusCodeValue": 200,
            "chat": [
                {"key": "step5model", "display_name": "Step 5 Preview", "enable": true, "price_factor": 1.0},
                {"key": "qfmodel", "display_name": "Qwen3.8-Flash", "enable": true, "price_factor": 0.1},
            ],
        });
        let models = parse_catalog(&payload);
        let step = models
            .iter()
            .find(|m| text_of(m, "id") == "Step5Preview")
            .expect("缺 Step 5 Preview");
        assert_eq!(text_of(step, "vendor"), "阶跃星辰", "Step 5 Preview 为阶跃星辰模型");
        let qwen = models
            .iter()
            .find(|m| text_of(m, "id") == "Qwen3.8-Flash")
            .expect("缺 Qwen3.8-Flash");
        assert!(qwen.get("vendor").is_none(), "未命中映射不给 vendor 键");

        // 连字符风格漂移：对外 id 保留连字符（toModelId 只去空白），匹配层折叠命中
        let hyphen = json!({
            "statusCodeValue": 200,
            "chat": [
                {"key": "step5model", "display_name": "Step-5-Preview", "enable": true, "price_factor": 1.0},
            ],
        });
        let models = parse_catalog(&hyphen);
        let step = models
            .iter()
            .find(|m| text_of(m, "id") == "Step-5-Preview")
            .expect("缺 Step-5-Preview（对外 id 保留连字符）");
        assert_eq!(text_of(step, "vendor"), "阶跃星辰", "连字符风格 id 仍命中厂商映射");
    }

    /// P2 前缀误匹配回归：数字形态码必须做后随字符边界校验——
    /// `"code":1053` 不得命中 105，`"code":10605` 不得命中 1060/106；
    /// 字符串形态与转义形态行为不变
    #[test]
    fn raw_has_code_rejects_prefix_of_longer_code() {
        // 更长码的前缀 → 不命中
        assert!(!raw_has_code(r#"{"code":1053,"message":"x"}"#, "105"));
        assert!(!raw_has_code(r#"{"code": 10530}"#, "105"));
        assert!(!raw_has_code(r#"{\"code\":10605}"#, "1060"));
        // 精确码（数字/字符串/带空格/转义）→ 命中
        assert!(raw_has_code(r#"{"code":105,"message":"expired"}"#, "105"));
        assert!(raw_has_code(r#"{"code": 105}"#, "105"));
        assert!(raw_has_code(r#"{"code":"105"}"#, "105"));
        assert!(raw_has_code(r#"{\"code\":\"10605\"}"#, "10605"));
        // 同文本中前缀命中失败但真码在后面 → 仍命中
        assert!(raw_has_code(r#"{"code":1053,"then":{"code":105}}"#, "105"));
    }

    // ── 思考工具 ──

    #[test]
    fn thinking_parser_finish_is_idempotent() {
        let mut p = ThinkingParser::new();
        p.push("plain");
        p.finish();
        p.finish(); // 幂等：重复收尾无额外产出
        let pieces = p.take();
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].text, "plain");
        assert!(!pieces[0].is_thinking);
    }

    #[test]
    fn strip_helpers() {
        assert_eq!(strip_thinking_tags("<think>a</think>b"), "ab");
        assert_eq!(strip_thinking_tags("<reasoning>x</reasoning>y"), "xy");
        assert_eq!(strip_leading_newline("\n\nx"), "x");
        assert_eq!(strip_leading_newline("\nx"), "x");
        assert_eq!(strip_leading_newline("\r\nx"), "x");
        assert_eq!(strip_leading_newline("x"), "x");
    }

    // ── 请求体构造 ──

    fn catalog_entry() -> Value {
        json!({
            "id": "Qwen3.8Flash", "name": "Qwen3.8Flash", "upstreamKey": "qfmodel",
            "reasoning": true, "supportsReasoning": true,
            "efforts": ["low", "medium", "xhigh"],
            "config": {"key": "qfmodel", "is_reasoning": true, "is_vl": true, "source": "system"},
        })
    }

    #[test]
    fn upstream_body_envelope_shape() {
        let peek = json!({
            "model": "Qwen3.8Flash",
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "你好世界"},
            ],
            "max_tokens": 999999,
        });
        let bytes = prepare_qoder_body(&peek, &catalog_entry(), "u-1").expect("合法 body 必须可构造");
        let v: Value = serde_json::from_slice(&bytes).expect("产出必须是合法 JSON");
        assert_eq!(v["session_type"], "qodercli");
        assert_eq!(v["agent_id"], "agent_common");
        assert_eq!(v["chat_task"], "FREE_INPUT");
        assert_eq!(v["stream"], true);
        assert_eq!(v["model_config"]["key"], "qfmodel", "model_config 顶层 key");
        assert_eq!(v["chat_context"]["extra"]["modelConfig"]["key"], "qfmodel");
        // system 保留在 messages 里（上游不看顶层 system 字段）
        assert_eq!(v["messages"][0]["role"], "system");
        assert_eq!(v["messages"][1]["content"], "你好世界");
        // max_tokens 超上限时截到 MAX_OUTPUT_TOKENS（实测 >32K 上游退化）
        assert_eq!(v["parameters"]["max_tokens"], MAX_OUTPUT_TOKENS);
        assert_eq!(v["chat_context"]["text"], "你好世界");
        assert_eq!(v["business"]["name"], "你好世界");
    }

    /// 上下文窗口覆盖：设置 → parameters.context_length；未设置 → 请求体除随机
    /// id/时间戳外与改造前一致；覆盖键不泄漏进上游 model_config
    #[test]
    fn context_override_injects_parameters_only_when_set() {
        let peek = json!({
            "model": "Qwen3.8Flash",
            "session_id": "s-1",
            "messages": [{"role": "user", "content": "hi"}],
            "reasoning_effort": "high",
        });
        // 去掉每次构造必变的字段（uuid / 时间戳），其余逐字段比较
        let stable = |bytes: Vec<u8>| {
            let mut v: Value = serde_json::from_slice(&bytes).unwrap();
            let obj = v.as_object_mut().unwrap();
            obj.remove("request_id");
            obj.remove("business");
            v
        };
        let base = stable(prepare_qoder_body(&peek, &catalog_entry(), "u-1").unwrap());
        assert!(base["parameters"].get("context_length").is_none(), "未设置不下发");

        let mut unset = catalog_entry();
        apply_context_override(&mut unset, None);
        assert_eq!(unset, catalog_entry(), "None 不改条目");
        assert_eq!(stable(prepare_qoder_body(&peek, &unset, "u-1").unwrap()), base);

        let mut set = catalog_entry();
        apply_context_override(&mut set, Some(1_000_000));
        let got = stable(prepare_qoder_body(&peek, &set, "u-1").unwrap());
        assert_eq!(got["parameters"]["context_length"], 1_000_000);
        assert_eq!(got["parameters"]["enable_thinking"], true, "既有参数保留");
        assert!(got["model_config"].get(CONTEXT_TARGET_KEY).is_none());
        // 仅 parameters 多一个字段，其余不变
        assert!(context_override_hint(&set, 400).contains("context_length=1000000"));
        assert_eq!(context_override_hint(&set, 404), "", "非 400 不附指引");
        assert_eq!(context_override_hint(&unset, 400), "", "未设置不附指引");
        let mut got_rest = got.clone();
        got_rest["parameters"].as_object_mut().unwrap().remove("context_length");
        assert_eq!(got_rest, base);
    }

    #[test]
    fn thinking_resolution_degrades() {
        let entry = catalog_entry();
        // 显式关闭 → 不指定（上游无法真正关闭，发 false 会断连）
        let off = resolve_thinking(&json!({"reasoning_effort": "off"}), &entry);
        assert!(off.enable.is_none() && off.effort.is_none());
        // minimal → low；high → xhigh（别名归一）
        let min = resolve_thinking(&json!({"reasoning_effort": "minimal"}), &entry);
        assert_eq!(min.effort.as_deref(), Some("low"));
        let high = resolve_thinking(&json!({"reasoning_effort": "high"}), &entry);
        assert_eq!(high.effort.as_deref(), Some("xhigh"));
        // 不支持的档位 → 退回声明的 medium
        let weird = resolve_thinking(&json!({"reasoning_effort": "ultra"}), &entry);
        assert_eq!(weird.effort.as_deref(), Some("medium"));
        // 白名单含 max 的模型（GLM/Kimi 系）：max 原值直用不被别名改写（审查修复）
        let glm_entry = json!({
            "id": "GLM-5.3", "upstreamKey": "gmodel", "reasoning": true,
            "efforts": ["high", "low", "max"],
        });
        let max = resolve_thinking(&json!({"reasoning_effort": "max"}), &glm_entry);
        assert_eq!(max.effort.as_deref(), Some("max"));
        let high = resolve_thinking(&json!({"reasoning_effort": "high"}), &glm_entry);
        assert_eq!(high.effort.as_deref(), Some("high"), "白名单原值命中不改写");
        // 未指定 → 开思考但用上游默认档
        let none = resolve_thinking(&json!({}), &entry);
        assert_eq!(none.enable, Some(true));
        assert!(none.effort.is_none());
        // 非思考模型：即便请求了档位也不指定
        let non_reasoning = json!({"reasoning": false, "efforts": []});
        let nr = resolve_thinking(&json!({"reasoning_effort": "high"}), &non_reasoning);
        assert!(nr.enable.is_none());
    }

    #[test]
    fn normalize_messages_drops_failed_pair() {
        let messages = vec![
            json!({"role": "user", "content": "q1"}),
            json!({"role": "assistant", "content": "", "__failed": true,
                   "tool_calls": [{"id": "c1", "function": {"name": "f", "arguments": "{}"}}]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "orphan result"}),
            json!({"role": "user", "content": "q2"}),
        ];
        let out = normalize_messages(&messages);
        // 失败 assistant 连同孤儿 tool 结果一起丢弃
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["content"], "q1");
        assert_eq!(out[1]["content"], "q2");
    }

    /// issue #154：images_of 变体识别——Anthropic image / Responses input_image
    /// 直入 chat completions 时归一为 image_url 形态；坏块丢弃；OpenAI 主形态
    /// 原样保留
    #[test]
    fn normalize_messages_image_variants() {
        let messages = vec![
            json!({"role": "user", "content": [
                {"type": "text", "text": "看图"},
                // Anthropic base64 形态
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aGk="}},
                // Responses input_image 形态
                {"type": "input_image", "image_url": "https://example.com/x.png"},
                // 坏块：空 data 应丢弃
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": ""}},
                // OpenAI 主形态
                {"type": "image_url", "image_url": {"url": "https://example.com/y.png"}},
            ]}),
        ];
        let out = normalize_messages(&messages);
        let content = out[0]["content"].as_array().unwrap();
        // 文本 + 3 张有效图（坏块被滤掉）
        assert_eq!(content.len(), 4);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(
            content[1],
            json!({"type": "image_url", "image_url": {"url": "data:image/png;base64,aGk="}})
        );
        assert_eq!(
            content[2],
            json!({"type": "image_url", "image_url": {"url": "https://example.com/x.png"}})
        );
        assert_eq!(
            content[3],
            json!({"type": "image_url", "image_url": {"url": "https://example.com/y.png"}})
        );
    }

    #[test]
    fn session_seed_stable_across_calls() {
        // 同种子派生同一 session_id（同段对话复用）；空种子每次新建
        let a = session_id_for("u", "qfmodel", Some("seed-1"));
        let b = session_id_for("u", "qfmodel", Some("seed-1"));
        let c = session_id_for("u", "qfmodel", Some("seed-2"));
        assert_eq!(a, b);
        assert_ne!(a, c);
        let x = session_id_for("u", "qfmodel", None);
        let y = session_id_for("u", "qfmodel", None);
        assert_ne!(x, y, "空种子 = 一次性会话");
    }
}
