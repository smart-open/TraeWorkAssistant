//! 模型列表配置化与官网同步
//!
//! - 模型下拉列表持久化在 `api_models.json`，不硬编码在前端
//! - 「同步官网模型」重放 Trae 客户端的 `batch_get_detail_param` 配置接口获取权威列表
//! - 过滤逻辑与客户端模型选择器对齐（2026-09-19 客户端三视图抓包逆向实证）：
//!   内部配置 / 自定义回显 / invis / 当代代际（context max）四层判定，详见 parse_official

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::fs_utils;
use crate::models::{AccountsFile, DeviceMap};

/// 单个模型选项：id = 上游 config_name（原样透传），label = 官方展示名。
/// 扩展字段（统一网关 §3.4，serde default 兼容旧文件——存量 api_models.json
/// 仅 {id,label} 原样读取，不重写不丢失 §9.6）：
/// - `rate`：官网同步解析的积分倍率（L2，§3.2；字段名以实际响应为准，
///   宽容解析失败置 None → 聚合层自动落 L3/L4，不阻塞）
/// - `context_length / efforts / supports_image`：同上 L2 语义
/// - `function`：同步时记录的来源视图（上游 llm_utils_chat 接受的 function 值）。
///   官方新上架模型免改代码：路由按表内视图请求，空 = 旧数据未同步 → 回退硬编码
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelOption {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub rate: Option<f64>,
    #[serde(default)]
    pub context_length: Option<u64>,
    #[serde(default)]
    pub efforts: Vec<String>,
    #[serde(default)]
    pub supports_image: Option<bool>,
    #[serde(default)]
    pub function: String,
}

/// 保位兜底：客户端内置模型，个别账号的配置接口响应可能缺失时补齐
/// （qwen3.8-flash / Doubao-Seed-Code 已随新请求形态返回，此处仅兜底；
///   glm-5.3-flash 的内置位依赖客户端配置回显，缺失时由此补齐）
const BUILTIN_EXTRA: [&str; 3] = ["Doubao-Seed-Code", "glm-5.3-flash", "qwen3.8-flash"];

/// 内置模型 → 官方展示名
const BUILTIN_EXTRA_LABELS: [(&str, &str); 3] = [
    ("Doubao-Seed-Code", "Seed-Code"),
    ("glm-5.3-flash", "GLM-5.3-Flash"),
    ("qwen3.8-flash", "Qwen3.8-Flash"),
];

/// 默认列表（2026-09-19 客户端模型选择器截图逐一比对：Agent 聊天 12 个 + Solo 4 个，
/// 与「同步官网模型」的过滤结果一致；不含客户端已隐藏的上一代模型
/// glm-5.1/kimi-k2.6/kimi-k2.7-code 与 plain DeepSeek-V4-Flash/V4-Pro）
pub fn default_models() -> Vec<ModelOption> {
    const ITEMS: [(&str, &str); 16] = [
        ("Doubao-Seed-Evolving", "Seed-Evolving"),
        ("Doubao-Seed-2.1-Pro", "Seed-2.1-Pro-0915"),
        ("Doubao-Seed-2.1-Turbo", "Seed-2.1-Turbo"),
        ("Doubao-Seed-Code", "Seed-Code"),
        ("glm-5.3-flash", "GLM-5.3-Flash"),
        ("glm-5.3", "GLM-5.3"),
        ("glm-5.2", "GLM-5.2"),
        ("deepseek-v4.1-flash", "DeepSeek-V4.1-Flash"),
        ("DeepSeek-V4-Flash-Official", "DeepSeek-V4-Flash 正式版"),
        ("DeepSeek-V4-Pro-Official", "DeepSeek-V4-Pro 正式版"),
        ("kimi-k3", "Kimi-K3"),
        ("kimi-k2.8-preview", "Kimi-K2.8-Preview"),
        ("minimax-m3", "MiniMax-M3"),
        ("qwen3.8-flash", "Qwen3.8-Flash"),
        ("qwen3.8-max", "Qwen3.8-Max"),
        ("qwen-3.7-plus", "Qwen3.7-Plus"),
    ];
    ITEMS
        .into_iter()
        .map(|(id, label)| ModelOption {
            id: id.to_string(),
            label: label.to_string(),
            // L2 运营元数据不预置：交由统一目录聚合层的 L3/L4 兜底（§3.2）；
            // function 预填硬编码实测映射：新装/历史数据免同步即路由正确
            rate: None,
            context_length: None,
            efforts: Vec::new(),
            supports_image: None,
            function: function_for_model(&id.to_lowercase()).to_string(),
        })
        .collect()
}

/// 硬编码回退：模型 → 上游 function 覆盖。仅在表内 `function` 为空
/// （存量数据未重新同步 / 默认列表兜底项）时使用；已知模型实证：
/// 部分模型仅在 solo_agent 下可用，其余走 Trae Work 模式的默认 function
pub fn function_for_model(model_lower: &str) -> &'static str {
    match model_lower {
        "doubao-seed-code" | "glm-5.3-flash" | "qwen3.8-flash" | "deepseek-v4.1-flash"
        | "kimi-k2.8-preview" => "solo_agent",
        _ => super::FUNCTION,
    }
}

/// 查表优先的 function 解析（官方新上架模型免改代码的关键路径）：
/// 同步落库的来源视图非空则直接采用；表内未收录或视图为空 → 硬编码回退。
/// `model_lower` 须为小写化后的请求模型名（调用方 to_lowercase）；
/// 表内 id 为官方 config_name 原样，比较时统一小写，大小写不敏感
pub fn function_for_model_in(list: &[ModelOption], model_lower: &str) -> String {
    for m in list {
        if m.id.to_lowercase() == model_lower && !m.function.is_empty() {
            return m.function.clone();
        }
    }
    function_for_model(model_lower).to_string()
}

/// 历史数据兼容回填：存量条目（旧版同步/默认列表写入）无 function 字段，
/// 按硬编码实测映射补齐——回填值与旧版路由行为完全一致（零行为变化），
/// 仅完成数据格式升级；后续「同步官网模型」由真实来源视图接管。
/// 返回 (升级后列表, 是否发生回填)；已有值不覆盖，幂等
fn backfill_functions(mut list: Vec<ModelOption>) -> (Vec<ModelOption>, bool) {
    let mut changed = false;
    for m in &mut list {
        if m.function.is_empty() {
            m.function = function_for_model(&m.id.to_lowercase()).to_string();
            changed = true;
        }
    }
    (list, changed)
}

/// 读取模型列表；kv 缺失或为空时写入默认列表。
/// SQLite 化（P2）：data/api_models.json → kv `api_models`（热路径单行 SELECT+解析，
/// 替代原 mtime 解析缓存；旧根路径兼容迁移由启动迁移器完成）。
pub fn load_models(data_dir: &Path) -> Vec<ModelOption> {
    // 热路径缓存（批次 A）：resolve_target / 目录聚合每请求读取
    super::config_cache::get_or_load(data_dir, "api_models", || {
        load_models_uncached(data_dir)
    })
}

fn load_models_uncached(data_dir: &Path) -> Vec<ModelOption> {
    let list: Vec<ModelOption> = crate::store::db(data_dir).kv_get("api_models");
    if !list.is_empty() {
        // 历史数据兼容：存量条目 function 为空 → 一次性回填写回（见 backfill_functions）；
        // 写库失败不阻塞读取（下次启动重试）。无需 invalidate：loader 返回值即本轮缓存内容
        let (upgraded, changed) = backfill_functions(list);
        if changed {
            if let Err(e) = crate::store::db(data_dir).kv_set("api_models", &upgraded) {
                fs_utils::app_log(data_dir, &format!("模型目录 function 回填写入失败: {e}"));
            } else {
                fs_utils::app_log(data_dir, "模型目录历史数据兼容：已回填 function 视图字段");
            }
        }
        return upgraded;
    }
    let defaults = default_models();
    if let Err(e) = crate::store::db(data_dir).kv_set("api_models", &defaults) {
        fs_utils::app_log(data_dir, &format!("模型列表默认配置写入失败: {e}"));
    }
    defaults
}

/// 官方模型内部黑名单（非用户可见的 agent/内部配置）
fn is_internal(name: &str) -> bool {
    const EXACT: [&str; 4] = ["Doubao_1_6", "doubao_1_6", "aquila", "sagitta"];
    const PREFIXES: [&str; 10] = [
        "custom_model",
        "search_agent",
        "fast_apply",
        "input_optimization",
        "explore",
        "file_search",
        "browser_use",
        "agnes",
        "summary",
        "commit",
    ];
    EXACT.contains(&name) || PREFIXES.iter().any(|p| name.starts_with(p))
}

/// 规范排序：按默认列表顺序，官网新增模型追加尾部；内置 3 项保位插入
fn normalize_order(mut fetched: Vec<ModelOption>) -> Vec<ModelOption> {
    let defaults = default_models();
    // 保位插入内置项（若官网列表缺失）
    for extra in BUILTIN_EXTRA {
        if fetched.iter().any(|m| m.id == extra) {
            continue;
        }
        let label = BUILTIN_EXTRA_LABELS
            .iter()
            .find(|(id, _)| *id == extra)
            .map(|(_, l)| l.to_string())
            .unwrap_or_else(|| extra.to_string());
        let pos = defaults
            .iter()
            .position(|d| d.id == extra)
            .unwrap_or(fetched.len());
        let insert_at = fetched
            .iter()
            .position(|m| {
                defaults
                    .iter()
                    .position(|d| d.id == m.id)
                    .map_or(false, |i| i > pos)
            })
            .unwrap_or(fetched.len());
        fetched.insert(
            insert_at,
            ModelOption {
                id: extra.to_string(),
                label,
                rate: None,
                context_length: None,
                efforts: Vec::new(),
                supports_image: None,
                // 与默认列表同源：预填硬编码实测映射，保位项路由行为不变
                function: function_for_model(&extra.to_lowercase()).to_string(),
            },
        );
    }
    let rank = |id: &str| {
        defaults
            .iter()
            .position(|d| d.id == id)
            .unwrap_or(usize::MAX)
    };
    fetched.sort_by(|a, b| rank(&a.id).cmp(&rank(&b.id)));
    fetched
}

/// 重放 batch_get_detail_param 拉取官网最新模型列表并落盘
/// `accounts` 由调用方预先经 vault 解密（含明文 jwt）
pub fn fetch_official(data_dir: &Path, accounts: AccountsFile) -> Result<Vec<ModelOption>, String> {
    // 取第一个可用账号（最多尝试 3 个）
    // SQLite 化（P4）：device_map 表
    let device_map: DeviceMap = crate::store::docs::device_map_load(&crate::store::db(data_dir));
    let candidates: Vec<(&crate::models::RawAccount, String, String)> = accounts
        .accounts
        .iter()
        .filter(|a| !a.jwt.trim().is_empty())
        .take(3)
        .filter_map(|a| {
            let uid = a.user_id.as_deref()?.to_string();
            let device_id = device_map
                .get(&uid)
                .map(|d| d.device_id.clone())
                .unwrap_or_default();
            let machine_id = super::pool::seeded_hex(64, &uid, "mach");
            Some((a, device_id, machine_id))
        })
        .collect();
    if candidates.is_empty() {
        return Err("没有可用账号（缺少 JWT），请先在账号管理中添加账号".into());
    }

    // 项目未启用 ureq 的 proxy-from-env feature：Agent 默认直连，
    // 不读环境变量/系统代理，不会被本地 MITM 代理拦截形成循环
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(30))
        .build();
    // 请求形态 = 客户端 Agent 聊天选择器的真实请求（2026-09-19 抓包固化）：
    // 7 函数 + access_type=0。经验证 config_name="" 与客户端原样（带当前模型名）
    // 返回完全相同的模型集合；旧 9 函数/access_type=1 形态返回的是另一视图
    // （缺 kimi-k2.8-preview/qwen3.8-flash，且 invis 标志按错误语境计算）
    let body = json!({
        "functions": [
            "builder", "builder_v3", "chat_v3", "code_reviewer",
            "code_review_summary", "refactor", "solo_agent"
        ],
        "agent_type": "solo_agent",
        "current_config_info": { "config_name": "", "is_custom_model": false },
        "mode_type": 0,
        "access_type": 0,
        "ab_force_vids": "",
        "ab_autotest_advanced_mode": 0,
        "show_custom_model": true,
    });

    let mut last_err = String::new();
    let mut parsed: Option<Vec<ModelOption>> = None;
    for (account, device_id, machine_id) in &candidates {
        // 存储惯例：jwt 可能带 `Cloud-IDE-JWT ` 前缀（自动捕获/本地抓取/刷新链路均
        // 规范化为带前缀写入）。mchost.guru 网关的 x-ide-token 只接受裸 JWT，
        // 带前缀会被网关在 token 校验前统一拒绝（401 code 1001，与垃圾 token 同型
        // 报错，2026-09-19 消融探针实证：剥前缀后同请求 200）。对齐 pool.rs 入池
        // 时的 strip_prefix 行为。
        let jwt_raw = account.jwt.trim();
        let jwt_clean = jwt_raw.strip_prefix("Cloud-IDE-JWT ").unwrap_or(jwt_raw).trim();
        let resp = agent
            .post("https://api5-normal.mchost.guru/api/ide/v1/batch_get_detail_param")
            .set("Content-Type", "application/json")
            .set("Request-Traffic-Type", "prod")
            .set("User-Agent", "TraeClient/TTNet")
            .set("x-app-id", super::APP_ID)
            .set("x-app-version", "default")
            .set("x-app-version-code", &super::IDE_VERSION_CODE.to_string())
            .set("x-bridge-transport", "aha")
            .set("x-device-brand", "CREFG-XX")
            .set("x-device-cpu", "Intel")
            .set("x-device-id", device_id)
            .set("x-device-type", "windows")
            .set("x-ide-token", jwt_clean)
            .set("x-ide-version", &super::IDE_VERSION)
            .set("x-ide-version-code", &super::IDE_VERSION_CODE.to_string())
            .set("x-ide-version-type", "stable")
            .set("x-lgw-req-sdk-type", "3")
            .set("x-machine-id", machine_id)
            .set("x-os-version", "Windows 11 Home China")
            .set("package-type", "stable_cn")
            .set("x-lscbd-aid", "787976")
            .set("x-lscbd-platform", "windows")
            .set("app-version", &super::IDE_VERSION)
            .set("x-ss-dp", "787976")
            .send_json(body.clone());

        match resp {
            Ok(r) => match into_string(r) {
                Ok(text) => match parse_official(&text) {
                    Ok(list) if !list.is_empty() => {
                        parsed = Some(list);
                        break;
                    }
                    Ok(_) => last_err = "官网返回模型列表为空".into(),
                    Err(e) => last_err = e,
                },
                Err(e) => last_err = e,
            },
            Err(ureq::Error::Status(code, r)) => {
                // 401 等：换下一个账号重试
                let detail = into_string(r).unwrap_or_default();
                last_err = format!("HTTP {code}: {}", detail.chars().take(160).collect::<String>());
            }
            Err(e) => last_err = format!("请求失败: {e}"),
        }
    }

    let fetched = parsed.ok_or_else(|| {
        let mut msg = format!("同步失败（已尝试 {} 个账号）: {last_err}", candidates.len());
        // 401 / code 1001 = JWT 无效（过期或在别处重新登录被服务端吊销）：
        // 补充可行动指引，避免用户误判为同步功能故障
        if last_err.contains("401") || last_err.contains("1001") {
            msg.push_str(
                "——HTTP 401/code 1001 表示账号 JWT 已失效：请在「账号管理」对该账号执行「续期 JWT」，\
                 或重新 OAuth 登录 / 在客户端登录后「保存当前登录态」，再重试同步",
            );
        }
        msg
    })?;

    let list = normalize_order(fetched);
    crate::store::db(data_dir).kv_set("api_models", &list)?;
    // 写路径显式失效（批次 A）：官网同步结果即时生效
    super::config_cache::invalidate(data_dir, "api_models");
    fs_utils::app_log(
        data_dir,
        &format!("官网模型列表同步成功: {} 个模型", list.len()),
    );
    Ok(list)
}

fn into_string(r: ureq::Response) -> Result<String, String> {
    r.into_string()
        .map_err(|e| format!("读取响应失败: {e}"))
}

/// 宽容取值：候选键逐个探测（数值），任一命中即返回（L2 字段名待抓包确认 §3.2）
fn dig_f64(v: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|k| {
        v.get(*k)
            .and_then(|x| x.as_f64())
            .or_else(|| v.get(*k).and_then(|x| x.as_str()).and_then(|s| s.parse().ok()))
    })
}

fn dig_u64(v: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|k| {
        v.get(*k)
            .and_then(|x| x.as_u64())
            .or_else(|| v.get(*k).and_then(|x| x.as_str()).and_then(|s| s.parse().ok()))
    })
}

fn dig_bool(v: &Value, keys: &[&str]) -> Option<bool> {
    keys.iter().find_map(|k| v.get(*k).and_then(|x| x.as_bool()))
}

fn dig_strs(v: &Value, keys: &[&str]) -> Vec<String> {
    keys.iter()
        .find_map(|k| {
            v.get(*k)
                .and_then(|x| x.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|s| s.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .filter(|a| !a.is_empty())
        })
        .unwrap_or_default()
}

/// 档位语义序（与 wb_catalog::resolve_effort 的 rank 一致）；未知档位排最后。
/// 字母序会把 {low,medium,high} 排成 high/low/medium，破坏展示与降级语义
fn effort_rank(e: &str) -> usize {
    ["minimal", "low", "medium", "high", "xhigh", "max"]
        .iter()
        .position(|o| *o == e)
        .unwrap_or(usize::MAX)
}

/// L2 运营字段提取（batch_get_detail_param 条目）。
/// 字段位置以 2026-09-19 客户端抓包固化为准：
/// - 倍率：`display_contact_config`（JSON 字符串）→ consumption_rate.data.rate
///   （基准倍率，会员/闲时折扣在 discount 块，客户端选择器亦展示基准值）
/// - 上下文：`context_window_tokens.max`（缺省回落 dev）
/// - 图片：`display_config.multimodal`
/// - efforts：响应未提供 → 空，交由聚合层 L3/L4 兜底
fn extract_meta(ci: &Value) -> (Option<f64>, Option<u64>, Vec<String>, Option<bool>) {
    let contact: Value = ci
        .get("display_contact_config")
        .and_then(|v| v.as_str())
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(Value::Null);
    let rate = contact
        .get("consumption_rate")
        .and_then(|c| c.get("data"))
        .and_then(|d| dig_f64(d, &["rate"]));
    let ctx = ci.get("context_window_tokens");
    let context_length = ctx
        .and_then(|c| dig_u64(c, &["max"]))
        .or_else(|| ctx.and_then(|c| dig_u64(c, &["dev"])));
    let mut efforts = dig_strs(
        ci,
        &["efforts", "supported_efforts", "supportedEfforts", "thinking_modes"],
    )
    .into_iter()
    .map(|e| e.to_lowercase())
    .collect::<Vec<_>>();
    efforts.sort_by_key(|e| effort_rank(e));
    efforts.dedup();
    let supports_image = ci
        .get("display_config")
        .and_then(|d| dig_bool(d, &["multimodal"]));
    (rate, context_length, efforts, supports_image)
}

/// 主对话语境优先：同模型跨 function 重复出现时，solo_agent/chat_v3 携带权威的
/// 展示名/倍率/上下文，builder* 为降级副本、code_reviewer/refactor 为评审语境替身
/// （2026-09-19 C2 抓包实证：deepseek-v4.1-flash 在 chat_v3 位展示名是
/// 'DeepSeek-V4-Flash 正式版'，solo_agent 位才是 'DeepSeek-V4.1-Flash'）
const PRIMARY_FUNCTIONS: [&str; 4] = ["solo_agent", "chat_v3", "builder", "builder_v3"];

/// 当代代际阈值（客户端可见性判别实证，2026-09-19 三视图逐字段比对）：
/// 客户端仅展示 context_window_tokens.max ≥ 256k 级的模型——当代主力 1M、
/// Seed 系列 256k；上一代模型（glm-5.1/kimi-k2.6/minimax-m2.7 等 max=200k）、
/// 内部/辅助配置（max 缺失）一律不展示。该规则同时天然剔除降级副本条目
/// （builder* 语境的 max=None 副本）与用户自定义回显（无 max）。
/// 实测覆盖 98 模型 × 4 视图与客户端两张选择器截图零误差。
const CONTEXT_MAX_MIN: u64 = 250_000;

/// 解析 batch_get_detail_param 响应：跨 function 合并、按客户端可见性过滤、去重。
/// 合并时按 PRIMARY_FUNCTIONS 优先级记录来源视图到 `function` 字段（该 function 名
/// 即上游 llm_utils_chat 接受的请求值）——官方新上架模型路由免改代码的数据来源；
/// 带 __dev 内部名的条目视为「可调用实证」，替换分支以提供最终胜出条目的视图为准
fn parse_official(text: &str) -> Result<Vec<ModelOption>, String> {
    let root: serde_json::Value = serde_json::from_str(text).map_err(|e| format!("解析失败: {e}"))?;
    let fcs = root
        .get("function_configs")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "响应缺少 function_configs".to_string())?;

    // 主语境优先稳定排序（同优先级保持服务端相对顺序）
    let mut ordered_fns: Vec<&serde_json::Value> = fcs.iter().collect();
    ordered_fns.sort_by_key(|fc| {
        let fname = fc.get("function").and_then(|f| f.as_str()).unwrap_or("");
        PRIMARY_FUNCTIONS
            .iter()
            .position(|p| *p == fname)
            .unwrap_or(PRIMARY_FUNCTIONS.len())
    });

    let mut result: Vec<ModelOption> = Vec::new();
    let mut seen: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    for fc in ordered_fns {
        let Some(items) = fc.get("config_info_list").and_then(|v| v.as_array()) else {
            continue;
        };
        let fname = fc.get("function").and_then(|f| f.as_str()).unwrap_or("");
        for ci in items {
            let id = ci.get("config_name").and_then(|v| v.as_str()).unwrap_or("").trim();
            if id.is_empty() || is_internal(id) {
                continue;
            }
            // 自定义模型回显剔除：用户自建（custom_* 前缀）与服务端自定义预设
            // （usage=custom_model / custom_models 字段，如 custom_claude-3-7-sonnet）
            // 随 show_custom_model=true 回显，但客户端主选择器不将其列入标准列表；
            // glm-5.3-flash 内置位不带这些特征，不受影响
            if id.starts_with("custom_")
                || ci.get("usage").and_then(|v| v.as_str()) == Some("custom_model")
                || ci
                    .get("custom_models")
                    .and_then(|v| v.as_array())
                    .map_or(false, |a| !a.is_empty())
            {
                continue;
            }
            if ci.get("is_invisible_to_user").and_then(|v| v.as_bool()).unwrap_or(false) {
                continue;
            }
            if ci.get("config_switch").and_then(|v| v.as_bool()) == Some(false) {
                continue;
            }
            let label = ci
                .get("display_config")
                .and_then(|d| d.get("display_name"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if label.is_empty() {
                continue;
            }
            // 当代代际判定（客户端可见性核心规则，见 CONTEXT_MAX_MIN 注释）
            let ctx_max = ci
                .get("context_window_tokens")
                .and_then(|c| c.get("max"))
                .and_then(|v| v.as_u64());
            if ctx_max.map_or(true, |m| m < CONTEXT_MAX_MIN) {
                continue;
            }
            let has_dev = ci
                .get("model_detail_list")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter().any(|m| {
                        m.get("model_name")
                            .and_then(|v| v.as_str())
                            .map_or(false, |s| s.ends_with("__dev"))
                    })
                })
                .unwrap_or(false);
            match seen.get(id) {
                Some(true) => {} // 已收录且带 __dev，跳过
                Some(false) => {
                    if has_dev {
                        // 用带 __dev 的条目整体替换先前收录的同名条目（展示名+运营字段+
                        // 来源视图——带 __dev 的视图是可调用实证）
                        if let Some(pos) = result.iter().position(|m| m.id == id) {
                            let (rate, context_length, efforts, supports_image) = extract_meta(ci);
                            result[pos] = ModelOption {
                                id: id.to_string(),
                                label: label.to_string(),
                                rate,
                                context_length,
                                efforts,
                                supports_image,
                                function: fname.to_string(),
                            };
                        }
                        seen.insert(id.to_string(), true);
                    }
                }
                None => {
                    seen.insert(id.to_string(), has_dev);
                    let (rate, context_length, efforts, supports_image) = extract_meta(ci);
                    result.push(ModelOption {
                        id: id.to_string(),
                        label: label.to_string(),
                        rate,
                        context_length,
                        efforts,
                        supports_image,
                        function: fname.to_string(),
                    });
                }
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn function_override_solo_agent_models() {
        assert_eq!(function_for_model("glm-5.3-flash"), "solo_agent");
        assert_eq!(function_for_model("qwen3.8-flash"), "solo_agent");
        assert_eq!(function_for_model("doubao-seed-code"), "solo_agent");
        assert_eq!(function_for_model("deepseek-v4.1-flash"), "solo_agent");
        assert_eq!(function_for_model("kimi-k2.8-preview"), "solo_agent");
        assert_eq!(function_for_model("glm-5.2"), super::super::FUNCTION);
        assert_eq!(function_for_model("deepseek-v4-flash"), super::super::FUNCTION);
    }

    /// 查表优先：表内 function 非空则直接采用（官方新上架免改代码），
    /// 未收录 / 视图为空（旧数据）→ 硬编码回退
    #[test]
    fn function_for_model_in_prefers_table_then_falls_back() {
        let list = vec![
            ModelOption {
                id: "brand-new-model".into(),
                label: "Brand New".into(),
                rate: None,
                context_length: None,
                efforts: Vec::new(),
                supports_image: None,
                function: "solo_agent".into(),
            },
            // 已知模型但视图为空（存量数据未重新同步）→ 回退硬编码
            ModelOption {
                id: "deepseek-v4.1-flash".into(),
                label: "DeepSeek-V4.1-Flash".into(),
                rate: None,
                context_length: None,
                efforts: Vec::new(),
                supports_image: None,
                function: String::new(),
            },
        ];
        assert_eq!(function_for_model_in(&list, "brand-new-model"), "solo_agent");
        // id 大小写不敏感：表内 id 官方原样（如 Doubao-Seed-Code）也按小写命中
        assert_eq!(
            function_for_model_in(&list, &"BRAND-NEW-MODEL".to_lowercase()),
            "solo_agent"
        );
        // 视图为空 → 硬编码
        assert_eq!(function_for_model_in(&list, "deepseek-v4.1-flash"), "solo_agent");
        // 表外已知模型 → 硬编码；表外未知模型 → 默认 function
        assert_eq!(function_for_model_in(&list, "glm-5.3-flash"), "solo_agent");
        assert_eq!(function_for_model_in(&list, "glm-5.2"), super::super::FUNCTION);
        assert_eq!(function_for_model_in(&list, "totally-unknown"), super::super::FUNCTION);
        // 空表 → 全部硬编码回退
        assert_eq!(function_for_model_in(&[], "brand-new-model"), super::super::FUNCTION);
    }

    /// 历史数据兼容：无 function 的存量条目按硬编码实测映射回填（与旧路由
    /// 行为零差异），官方原样大小写 id 命中，已有值不覆盖
    #[test]
    fn backfill_functions_upgrades_legacy_entries() {
        let mo = |id: &str, function: &str| ModelOption {
            id: id.into(),
            label: id.into(),
            rate: None,
            context_length: None,
            efforts: Vec::new(),
            supports_image: None,
            function: function.into(),
        };
        let legacy = vec![
            mo("glm-5.2", ""),
            mo("Doubao-Seed-Code", ""),
            mo("brand-new-x", "solo_agent"),
        ];
        let (upgraded, changed) = backfill_functions(legacy);
        assert!(changed);
        assert_eq!(upgraded[0].function, super::super::FUNCTION);
        // 官方原样大小写 id（Doubao-Seed-Code）按小写命中硬编码映射
        assert_eq!(upgraded[1].function, "solo_agent");
        assert_eq!(upgraded[2].function, "solo_agent");
    }

    /// 回填幂等：升级后列表再次执行无变更（不会反复写库）
    #[test]
    fn backfill_functions_idempotent_after_upgrade() {
        let (once, _) = backfill_functions(default_models());
        let (_, changed) = backfill_functions(once);
        assert!(!changed);
    }

    /// 默认列表自带 function：新装环境（首次写入）无需同步即路由正确
    #[test]
    fn default_models_carry_functions() {
        for m in default_models() {
            assert!(!m.function.is_empty(), "{} 缺 function", m.id);
        }
        let f = |id: &str| {
            default_models()
                .iter()
                .find(|m| m.id == id)
                .unwrap()
                .function
                .clone()
        };
        assert_eq!(f("deepseek-v4.1-flash"), "solo_agent");
        assert_eq!(f("kimi-k2.8-preview"), "solo_agent");
        assert_eq!(f("glm-5.2"), super::super::FUNCTION);
    }

    #[test]
    fn parse_official_dedupes_and_filters() {
        let text = serde_json::json!({
            "function_configs": [
                { "function": "code_reviewer", "config_info_list": [
                    { "config_name": "summary", "is_invisible_to_user": false,
                      "display_config": { "display_name": "Summary" } },
                    { "config_name": "glm-5.3", "is_invisible_to_user": false,
                      "display_config": { "display_name": "glm-5.3" },
                      "context_window_tokens": { "dev": 200000, "max": 1000000 },
                      "model_detail_list": [{ "model_name": "glm-5.3" }] }
                ]},
                { "function": "solo_agent", "config_info_list": [
                    { "config_name": "glm-5.3", "is_invisible_to_user": false,
                      "display_config": { "display_name": "GLM-5.3" },
                      "context_window_tokens": { "dev": 200000, "max": 1000000 },
                      "model_detail_list": [{ "model_name": "glm-5.3__dev" }] },
                    { "config_name": "glm-5.2", "is_invisible_to_user": true,
                      "display_config": { "display_name": "GLM-5.2" } },
                    { "config_name": "qwen-3.7-plus", "is_invisible_to_user": false,
                      "display_config": { "display_name": "Qwen3.7-Plus" },
                      "context_window_tokens": { "dev": 200000, "max": 1000000 },
                      "model_detail_list": [{ "model_name": "qwen-3.7-plus__dev" }] }
                ]}
            ]
        })
        .to_string();
        let list = parse_official(&text).unwrap();
        // summary 被过滤；glm-5.3 去重取 solo_agent 主语境 __dev 版本展示名；
        // 隐藏的 glm-5.2 不出现
        assert_eq!(
            list.iter().map(|m| (m.id.as_str(), m.label.as_str())).collect::<Vec<_>>(),
            vec![("glm-5.3", "GLM-5.3"), ("qwen-3.7-plus", "Qwen3.7-Plus")]
        );
        // 来源视图落库：solo_agent 优先级最高（code_reviewer 视图同模型被跳过）
        assert_eq!(list[0].function, "solo_agent");
        assert_eq!(list[1].function, "solo_agent");
    }

    /// 客户端可见性四层过滤（2026-09-19 截图逆向实证）：
    /// 上一代 200k / 无 max 内部配置 / 自定义回显 / invis 全部剔除
    #[test]
    fn parse_official_filters_legacy_gen_and_custom_echo() {
        let text = serde_json::json!({
            "function_configs": [
                { "function": "solo_agent", "config_info_list": [
                    { "config_name": "glm-5.2", "is_invisible_to_user": false,
                      "display_config": { "display_name": "GLM-5.2" },
                      "context_window_tokens": { "dev": 200000, "max": 1000000 },
                      "model_detail_list": [{ "model_name": "glm-5.2__dev" }] },
                    { "config_name": "glm-5.1", "is_invisible_to_user": false,
                      "display_config": { "display_name": "GLM-5.1" },
                      "context_window_tokens": { "dev": 200000, "max": 200000 },
                      "model_detail_list": [{ "model_name": "glm-5.1__dev" }] },
                    { "config_name": "code-review-judge", "is_invisible_to_user": false,
                      "display_config": { "display_name": "DeepSeek-V3.1-Terminus" },
                      "context_window_tokens": { "dev": 112000 },
                      "model_detail_list": [{ "model_name": "x__dev" }] },
                    { "config_name": "custom_claude-3-7-sonnet", "is_invisible_to_user": false,
                      "custom_models": ["anthropic//claude-3-7-sonnet-20250219"],
                      "usage": "custom_model",
                      "display_config": { "display_name": "Claude-3.7-sonnet" },
                      "context_window_tokens": { "dev": 38192 } },
                    { "config_name": "DeepSeek-V4-Flash", "is_invisible_to_user": false,
                      "display_config": { "display_name": "DeepSeek-V4-Flash" },
                      "context_window_tokens": { "dev": 200000, "max": 200000 },
                      "model_detail_list": [{ "model_name": "deepseek_v4_flash__dev" }] },
                    { "config_name": "DeepSeek-V4-Flash-Official", "is_invisible_to_user": false,
                      "display_config": { "display_name": "DeepSeek-V4-Flash 正式版" },
                      "context_window_tokens": { "dev": 200000, "max": 1000000 },
                      "model_detail_list": [{ "model_name": "DeepSeek-V4-Flash-Official__dev" }] },
                    { "config_name": "Doubao-Seed-2.1-Turbo", "is_invisible_to_user": false,
                      "display_config": { "display_name": "Seed-2.1-Turbo" },
                      "context_window_tokens": { "dev": 256000, "max": 256000 },
                      "model_detail_list": [{ "model_name": "Doubao-Seed-2.1-Turbo__dev" }] },
                    { "config_name": "Doubao-Seed-2.0-Code", "is_invisible_to_user": false,
                      "display_config": { "display_name": "Doubao-Seed-2.0-Code" },
                      "context_window_tokens": { "dev": 184000, "max": 184000 },
                      "model_detail_list": [{ "model_name": "Doubao-Seed-2.0-Code__dev" }] }
                ]}
            ]
        })
        .to_string();
        let list = parse_official(&text).unwrap();
        // 仅当代代际（1M/256k）通过：glm-5.1(200k)/judge(无max)/custom 回显/
        // plain V4-Flash(200k)/Seed-2.0-Code(184k) 全部剔除
        assert_eq!(
            list.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["glm-5.2", "DeepSeek-V4-Flash-Official", "Doubao-Seed-2.1-Turbo"]
        );
    }

    /// 主语境优先：同模型跨 function 展示名不同时，取 solo_agent 位权威名
    #[test]
    fn parse_official_prefers_primary_function_occurrence() {
        let text = serde_json::json!({
            "function_configs": [
                { "function": "chat_v3", "config_info_list": [
                    { "config_name": "deepseek-v4.1-flash", "is_invisible_to_user": false,
                      "display_config": { "display_name": "DeepSeek-V4-Flash 正式版" },
                      "context_window_tokens": { "dev": 116000, "max": 1000000 },
                      "model_detail_list": [{ "model_name": "deepseek-v4.1-flash__dev" }] }
                ]},
                { "function": "solo_agent", "config_info_list": [
                    { "config_name": "deepseek-v4.1-flash", "is_invisible_to_user": false,
                      "display_config": { "display_name": "DeepSeek-V4.1-Flash" },
                      "context_window_tokens": { "dev": 116000, "max": 1000000 },
                      "model_detail_list": [{ "model_name": "deepseek-v4.1-flash__dev" }] }
                ]}
            ]
        })
        .to_string();
        let list = parse_official(&text).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].label, "DeepSeek-V4.1-Flash");
        // 主语境优先落库：solo_agent 位权威名 + 来源视图
        assert_eq!(list[0].function, "solo_agent");
    }

    /// 替换分支落库：先收录视图缺 __dev（无可调用实证），带 __dev 的视图
    /// 整体替换时来源视图一并更新（function 名即上游接受的请求值）
    #[test]
    fn parse_official_replace_branch_updates_function() {
        let text = serde_json::json!({
            "function_configs": [
                { "function": "chat_v3", "config_info_list": [
                    { "config_name": "edge-model", "is_invisible_to_user": false,
                      "display_config": { "display_name": "Edge V1" },
                      "context_window_tokens": { "dev": 200000, "max": 1000000 } }
                ]},
                { "function": "builder", "config_info_list": [
                    { "config_name": "edge-model", "is_invisible_to_user": false,
                      "display_config": { "display_name": "Edge V2" },
                      "context_window_tokens": { "dev": 200000, "max": 1000000 },
                      "model_detail_list": [{ "model_name": "edge-model__dev" }] }
                ]}
            ]
        })
        .to_string();
        let list = parse_official(&text).unwrap();
        assert_eq!(list.len(), 1);
        // 元数据与来源视图均取自带 __dev 的胜出条目
        assert_eq!(list[0].label, "Edge V2");
        assert_eq!(list[0].function, "builder");
    }

    #[test]
    fn normalize_order_inserts_builtins_and_sorts() {
        let fetched = vec![
            ModelOption { id: "brand-new-model".into(), label: "Brand New".into(), rate: None, context_length: None, efforts: Vec::new(), supports_image: None, function: String::new() },
            ModelOption { id: "glm-5.3".into(), label: "GLM-5.3".into(), rate: None, context_length: None, efforts: Vec::new(), supports_image: None, function: String::new() },
            ModelOption { id: "qwen3.8-max".into(), label: "Qwen3.8-Max".into(), rate: None, context_length: None, efforts: Vec::new(), supports_image: None, function: String::new() },
        ];
        let list = normalize_order(fetched);
        let ids: Vec<&str> = list.iter().map(|m| m.id.as_str()).collect();
        // 内置 3 项按默认列表位置插入，未知模型排在最后
        let idx = |s: &str| ids.iter().position(|&x| x == s).unwrap();
        assert!(idx("Doubao-Seed-Code") < idx("glm-5.3-flash"));
        assert!(idx("glm-5.3-flash") < idx("glm-5.3"));
        assert!(idx("qwen3.8-flash") < idx("qwen3.8-max"));
        assert_eq!(ids.last(), Some(&"brand-new-model"));
        assert_eq!(ids.len(), 6);
    }

    // ==================== 统一网关 §3.4 ModelOption 扩展 ====================

    /// 存量旧格式（仅 id/label）serde default 兼容读取，不丢不重写（§9.6）
    #[test]
    fn old_file_shape_reads_with_defaults() {
        let text = r#"[{"id":"glm-5.3","label":"GLM-5.3"}]"#;
        let list: Vec<ModelOption> = serde_json::from_str(text).unwrap();
        assert_eq!(list.len(), 1);
        let m = &list[0];
        assert!(m.rate.is_none());
        assert!(m.context_length.is_none());
        assert!(m.efforts.is_empty());
        assert!(m.supports_image.is_none());
        // function 同样 serde default：空 = 回退硬编码 function_for_model
        assert!(m.function.is_empty());
    }

    /// parse_official 提取 L2 运营字段（真实字段位置：display_contact_config 倍率 /
    /// context_window_tokens 上下文 / display_config.multimodal 图片）
    #[test]
    fn parse_official_extracts_meta_fields() {
        let text = serde_json::json!({
            "function_configs": [
                { "function": "solo_agent", "config_info_list": [
                    { "config_name": "glm-5.3", "is_invisible_to_user": false,
                      "display_config": { "display_name": "GLM-5.3", "multimodal": false },
                      "context_window_tokens": { "dev": 116000, "max": 1000000 },
                      "display_contact_config": "{\"access\":{\"data\":{\"identity_list\":[0,5,1,2,3,100]}},\"consumption_rate\":{\"enable\":true,\"data\":{\"rate\":0.78}},\"reasoning\":{\"enable\":true}}",
                      "model_detail_list": [{ "model_name": "glm-5.3__dev" }] },
                    { "config_name": "kimi-k3", "is_invisible_to_user": false,
                      "display_config": { "display_name": "Kimi-K3", "multimodal": true },
                      "context_window_tokens": { "dev": 200000, "max": 1000000 },
                      "display_contact_config": "{\"consumption_rate\":{\"enable\":true,\"data\":{\"rate\":1.83}},\"multimodal\":{\"enable\":true}}",
                      "model_detail_list": [{ "model_name": "kimi-k3__dev" }] }
                ]}
            ]
        })
        .to_string();
        let list = parse_official(&text).unwrap();
        let g = list.iter().find(|m| m.id == "glm-5.3").unwrap();
        assert_eq!(g.rate, Some(0.78), "display_contact_config 内基准倍率");
        assert_eq!(g.context_length, Some(1_000_000), "取 max 上下文");
        assert_eq!(g.supports_image, Some(false), "multimodal=false 显式为否");
        let k = list.iter().find(|m| m.id == "kimi-k3").unwrap();
        assert_eq!(k.rate, Some(1.83));
        assert_eq!(k.supports_image, Some(true), "multimodal=true 支持图片");
        // efforts 响应未提供 → 空（交由 L3/L4 兜底）
        assert!(k.efforts.is_empty());
    }

    /// 字符串数值宽容解析（上游偶发字符串形态）
    #[test]
    fn extract_meta_tolerates_string_numbers() {
        let ci = serde_json::json!({
            "context_window_tokens": { "dev": "131072" },
            "display_contact_config": "{\"consumption_rate\":{\"enable\":true,\"data\":{\"rate\":\"0.16\"}}}"
        });
        let (rate, ctx, _, _) = extract_meta(&ci);
        assert_eq!(rate, Some(0.16));
        assert_eq!(ctx, Some(131_072), "max 缺失回落 dev");
    }
}
