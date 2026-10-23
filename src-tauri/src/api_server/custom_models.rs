//! 自定义模型资源池（OpenAI 兼容上游组）
//!
//! `data/custom_models.json`：用户以列表方式维护的自定义模型——
//! 名称（请求模型名，路由键）、OpenAI 兼容 API 地址、API Key 及模型其他字段
//! （上下文长度 / 最大输出 / 图片支持 / 倍率 / 负载权重 / 备胎 / 备注 / 启用开关）。
//!
//! 调度语义（v3，dispatch.rs）：同名多条 = 上游组（custom_lb 组内加权轮询 +
//! 主备 failover + 熔断切换）；作为第五资源池参与统一调度（默认候选序尾部兜底），
//! `custom/模型名` 前缀锁池直达、Key 绑定 "custom" 与 per_model 写 "custom" 生效；
//! 纯 custom 模型（无同名内置源）为唯一源直达。
//!
//! 协议约定：上游为 OpenAI 兼容 chat completions（`{base}/v1/chat/completions`，
//! base 以 `/v1` 结尾时直接拼 `/chat/completions`），Bearer 鉴权。
//! 请求体强制 `stream:true`（与 WB 上游同策略：统一 SSE 处理，非流式本地聚合）。

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::unified_catalog::canonical_id;

/// 单条自定义模型
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomModel {
    /// 稳定 id（cm-<12hex>，upsert 时空值自动生成）
    #[serde(default)]
    pub id: String,
    /// 请求模型名（路由键；canonical 匹配 trim+lowercase）
    #[serde(default)]
    pub name: String,
    /// OpenAI 兼容 API 地址（如 https://api.openai.com 或含 /v1 前缀）
    #[serde(default)]
    pub base_url: String,
    /// API Key（Bearer）
    #[serde(default)]
    pub api_key: String,
    /// 供应商（展示用，如 OpenAI / DeepSeek / 智谱；用户可自由输入）
    #[serde(default)]
    pub vendor: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub context_length: u64,
    #[serde(default)]
    pub max_tokens: u64,
    #[serde(default)]
    pub supports_image: bool,
    /// 展示倍率（0 = 免费；支持两位小数，如 0.01）
    #[serde(default)]
    pub rate: f64,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub updated_at: i64,
    /// 负载权重（默认 1；0 视为 1）——上游组内加权轮询用（P2 生效）
    #[serde(default = "default_weight")]
    pub weight: u32,
    /// 备胎标记（默认 false）：false = 活跃条目（正常参与加权轮询）；
    /// true = 备胎（活跃条目全部失败/熔断时才启用）——failover 主备语义
    #[serde(default)]
    pub standby: bool,
}

impl Default for CustomModel {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            vendor: String::new(),
            // 新建条目默认启用（serde 缺字段与 Default::default() 口径一致）
            enabled: true,
            context_length: 0,
            max_tokens: 0,
            supports_image: false,
            rate: 0.0,
            note: String::new(),
            updated_at: 0,
            weight: 1,
            standby: false,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_weight() -> u32 {
    1
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CustomModelsFile {
    #[serde(default)]
    pub models: Vec<CustomModel>,
    #[serde(default)]
    pub updated_at: i64,
}

/// 读取列表（SQLite 化 P3：custom_models 表；热路径单行组读取替代解析缓存）
pub fn load(data_dir: &Path) -> Vec<CustomModel> {
    // 热路径缓存（批次 A）：dispatch / 端点每请求读取，SQLite 读改为内存缓存命中
    super::config_cache::get_or_load(data_dir, "custom_models", || {
        crate::store::docs::custom_models_load(&crate::store::db(data_dir)).models
    })
}

/// 整表保存（调用方负责校验后的最终形态落盘）
pub fn save_list(data_dir: &Path, models: Vec<CustomModel>) -> Result<(), String> {
    let now = now_ts();
    let mut list = models;
    for m in &mut list {
        if m.updated_at == 0 {
            m.updated_at = now;
        }
    }
    let r = crate::store::docs::custom_models_save(
        &crate::store::db(data_dir),
        &CustomModelsFile { models: list, updated_at: now },
    );
    if r.is_ok() {
        // 写路径显式失效（批次 A）：配置改动即时生效
        super::config_cache::invalidate(data_dir, "custom_models");
    }
    r
}

/// base_url 规范化（A5/A6）：scheme 统一小写化 + 剥离默认端口（https:443 /
/// http:80）+ 去尾部斜杠——`HTTPS://x:443/` 与 `https://x` 归一为同键，杜绝
/// 大小写与默认端口变体绕过查重与匹配。host 大小写保持原文（审查范围仅 scheme）；
/// IPv6 裸地址无端口形态含 ']'，rsplit_once(':') 取末段不会误剥离，安全。
pub fn norm_base(base_url: &str) -> String {
    let s = base_url.trim();
    // scheme 通用小写化（A5）：任意大小写前缀归一；无 "://" 原样去尾斜杠返回
    let (scheme, rest) = match s.split_once("://") {
        Some((scheme, rest)) => (scheme.to_ascii_lowercase(), rest),
        None => return s.trim_end_matches('/').to_string(),
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    // 剥离默认端口（A6）：rsplit_once 取最后一段冒号比对纯数字端口
    let authority = match authority.rsplit_once(':') {
        Some((host, "443")) if scheme == "https" => host,
        Some((host, "80")) if scheme == "http" => host,
        _ => authority,
    };
    let mut out = format!("{scheme}://{authority}{path}");
    while out.ends_with('/') {
        out.pop();
    }
    out
}

/// upsert 单条：id 为空或不存在则新增（生成 cm-<12hex> id），存在则整条覆盖。
/// 返回保存后的完整列表。校验：name / base_url 必填；仅拒绝「同名 + 同 base_url +
/// 同 api_key」完全重复——同名不同上游 = 合法上游组成员（P1：同名多条 = 上游组）。
pub fn upsert(data_dir: &Path, mut m: CustomModel) -> Result<Vec<CustomModel>, String> {
    m.name = m.name.trim().to_string();
    // A5/A6：保存即归一（scheme 小写 + 剥默认端口 + 去尾斜杠），后续校验/查重/
    // 请求拼 URL 全部基于归一形态
    m.base_url = norm_base(&m.base_url);
    m.vendor = m.vendor.trim().to_string();
    if m.weight == 0 {
        m.weight = 1; // 0 视为 1，防除零与无效权重
    }
    if m.name.is_empty() {
        return Err("模型名称不能为空".into());
    }
    // 审查修复（A5）：scheme 不区分大小写（RFC 3986）——`HTTPS://a.com` 合法，
    // 原实现对大写 scheme 误拒；校验用小写副本，存储保留用户书写原样
    let lower_url = m.base_url.to_ascii_lowercase();
    if m.base_url.is_empty()
        || (!lower_url.starts_with("http://") && !lower_url.starts_with("https://"))
    {
        return Err("API 地址必须以 http:// 或 https:// 开头".into());
    }
    let mut list = load(data_dir);
    let canonical = canonical_id(&m.name);
    // 完全重复校验（P1 放宽）：同名（canonical 相同）+ 同 base_url + 同 api_key
    // = 无意义冗余，拒绝；同名不同上游（base_url 或 api_key 不同）= 上游组成员，放行。
    // 查重两侧过 norm_base（A5/A6）：存量数据未归一，`HTTPS://x:443` 变体不得绕过
    if let Some(dup) = list.iter().find(|e| {
        e.id != m.id
            && canonical_id(&e.name) == canonical
            && norm_base(&e.base_url) == norm_base(&m.base_url)
            && e.api_key == m.api_key
    }) {
        return Err(format!("相同模型与上游地址/Key 的条目已存在：{}", dup.name));
    }
    match list.iter_mut().find(|e| e.id == m.id && !m.id.is_empty()) {
        Some(slot) => {
            m.updated_at = now_ts();
            // B2：编辑保存即清熔断——旧条目的熔断状态不得残留影响新配置
            // （base_url/api_key 改动后原冷却窗与新上游无关）
            super::custom_lb::clear_circuit(&m.id);
            *slot = m;
        }
        None => {
            if m.id.is_empty() {
                m.id = format!("cm-{:012x}", std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or_else(|_| rand_u64()));
            }
            m.updated_at = now_ts();
            list.push(m);
        }
    }
    save_list(data_dir, list.clone())?;
    Ok(list)
}

/// 按 id 删除；返回是否确有删除
pub fn remove(data_dir: &Path, id: &str) -> Result<bool, String> {
    let mut list = load(data_dir);
    let before = list.len();
    list.retain(|m| m.id != id);
    let removed = list.len() != before;
    if removed {
        save_list(data_dir, list)?;
    }
    Ok(removed)
}

/// 请求模型名命中全部 enabled 自定义模型（canonical 匹配；上游组语义入口）。
/// 同 canonical 多条 = 上游组（P1），组内负载均衡轮询（P2）。
pub fn find_enabled_all(data_dir: &Path, model: &str) -> Vec<CustomModel> {
    let canonical = canonical_id(model);
    if canonical.is_empty() {
        return Vec::new();
    }
    load(data_dir)
        .into_iter()
        .filter(|m| m.enabled && canonical_id(&m.name) == canonical)
        .collect()
}

/// 单条语义兼容包装（白名单预检 / catalog 等调用方零改动）：返回首条 enabled 条目。
pub fn find_enabled(data_dir: &Path, model: &str) -> Option<CustomModel> {
    find_enabled_all(data_dir, model).into_iter().next()
}

/// OpenAI 兼容 chat completions URL：
/// base 以 /v1 结尾 → `{base}/chat/completions`；否则 → `{base}/v1/chat/completions`
pub fn chat_url(base_url: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/chat/completions")
    } else {
        format!("{base}/v1/chat/completions")
    }
}

fn rand_u64() -> u64 {
    // 兜底熵：地址熵 + 栈指针（id 唯一性由时间纳秒主路径保证，此为极端回退）
    let p = &canonical_id as *const _ as u64;
    p ^ 0x9e37_79b9_7f4a_7c15
}

fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ==================== 测试 ====================

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Fixture {
            // 目录名唯一性交给 fs_utils::test_temp_dir（进程内计数器），防 CI 偶发撞名
            let dir = crate::fs_utils::test_temp_dir("twa_custom");
            std::fs::create_dir_all(dir.join("data")).unwrap();
            Fixture(dir)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// upsert / canonical 命中 / 名称唯一性
    #[test]
    fn t01_upsert_find_and_dedup() {
        let f = Fixture::new();
        let list = upsert(&f.0, CustomModel {
            name: " My-Model ".into(),
            base_url: "https://api.example.com/".into(),
            api_key: "sk-test".into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(list.len(), 1);
        assert!(list[0].id.starts_with("cm-"));
        assert_eq!(list[0].base_url, "https://api.example.com");
        // canonical 命中（trim+lowercase）
        assert!(find_enabled(&f.0, "my-model").is_some());
        assert!(find_enabled(&f.0, "MY-MODEL").is_some());
        assert!(find_enabled(&f.0, "other").is_none());
        // 名称 canonical 冲突拒绝（trim+lowercase 同键）——同名同上游完全重复
        assert!(upsert(&f.0, CustomModel { name: "MY-Model".into(), base_url: "https://api.example.com".into(), api_key: "sk-test".into(), ..Default::default() })
            .is_err());
        // 同名不同上游（不同 base_url）= 上游组成员，放行（P1）
        let list2 = upsert(&f.0, CustomModel { name: "MY-Model".into(), base_url: "https://x.com".into(), ..Default::default() })
            .unwrap();
        assert_eq!(list2.len(), 2, "同名不同 base_url 构成上游组");
        // 编辑（同 id 覆盖）
        let mut edited = list[0].clone();
        edited.enabled = false;
        let list = upsert(&f.0, edited).unwrap();
        assert!(!list[0].enabled);
        // 组内首条禁用后，find_enabled 回退第二条；两条全禁用才不命中（P1 组语义）
        let second = list
            .iter()
            .find(|e| e.base_url == "https://x.com")
            .unwrap()
            .clone();
        assert_eq!(
            find_enabled(&f.0, "my-model").unwrap().id,
            second.id,
            "组内禁用一条回退另一条"
        );
        let mut edited2 = second.clone();
        edited2.enabled = false;
        let _ = upsert(&f.0, edited2).unwrap();
        assert!(find_enabled(&f.0, "my-model").is_none(), "组内全禁用不命中");
        assert!(find_enabled_all(&f.0, "my-model").is_empty());
    }

    /// remove / save_load 往返
    #[test]
    fn t02_remove_and_roundtrip() {
        let f = Fixture::new();
        let list = upsert(&f.0, CustomModel { name: "a".into(), base_url: "https://a.com".into(), ..Default::default() }).unwrap();
        let _ = upsert(&f.0, CustomModel { name: "b".into(), base_url: "https://b.com".into(), ..Default::default() }).unwrap();
        assert_eq!(list.len(), 1);
        assert!(remove(&f.0, &list[0].id).unwrap());
        assert!(!remove(&f.0, &list[0].id).unwrap(), "重复删除返回 false");
        assert_eq!(load(&f.0).len(), 1);
        assert_eq!(load(&f.0)[0].name, "b");
    }

    /// chat_url 归一：含 /v1 与不含 /v1 两种 base
    #[test]
    fn t03_chat_url_normalization() {
        assert_eq!(chat_url("https://api.openai.com"), "https://api.openai.com/v1/chat/completions");
        assert_eq!(chat_url("https://api.openai.com/"), "https://api.openai.com/v1/chat/completions");
        assert_eq!(chat_url("https://api.openai.com/v1"), "https://api.openai.com/v1/chat/completions");
        assert_eq!(chat_url("https://api.openai.com/v1/"), "https://api.openai.com/v1/chat/completions");
    }

    /// 校验：空名称 / 非法 base_url 拒绝；大写 scheme 合法（A5：scheme 不区分大小写）
    #[test]
    fn t04_validation() {
        let f = Fixture::new();
        assert!(upsert(&f.0, CustomModel { name: "".to_string(), base_url: "https://a.com".into(), ..Default::default() }).is_err());
        assert!(upsert(&f.0, CustomModel { name: "x".into(), base_url: "ftp://a.com".into(), ..Default::default() }).is_err());
        // A5 审查修复：大写 scheme 不再误拒（RFC 3986 scheme 大小写不敏感）
        assert!(upsert(&f.0, CustomModel { name: "up".into(), base_url: "HTTPS://a.com".into(), ..Default::default() }).is_ok());
    }

    /// P1：同名不同 base_url 多条 upsert 成功（上游组）；find_enabled_all 全量返回；
    /// find_enabled 兼容包装仍返回首条
    #[test]
    fn t05_same_name_multi_upstream() {
        let f = Fixture::new();
        let _ = upsert(&f.0, CustomModel { name: "glm-5.3".into(), base_url: "https://a.com".into(), api_key: "k1".into(), ..Default::default() }).unwrap();
        let _ = upsert(&f.0, CustomModel { name: "GLM-5.3".into(), base_url: "https://b.com".into(), api_key: "k2".into(), ..Default::default() }).unwrap();
        let _ = upsert(&f.0, CustomModel { name: "glm-5.3".into(), base_url: "https://a.com".into(), api_key: "k3".into(), ..Default::default() }).unwrap();
        let group = find_enabled_all(&f.0, "glm-5.3");
        assert_eq!(group.len(), 3, "同名不同上游构成 3 条上游组");
        assert_eq!(find_enabled(&f.0, "GLM-5.3").unwrap().base_url, "https://a.com", "兼容包装返回首条");
        // 同上游不同 api_key（同上游多 Key 分摊）也是合法组成员
        assert_eq!(group.iter().filter(|m| m.base_url == "https://a.com").count(), 2);
    }

    /// P1：完全重复（同名 + 同 base_url + 同 api_key）拒绝；同名不同 api_key 放行
    #[test]
    fn t06_exact_duplicate_rejected() {
        let f = Fixture::new();
        let _ = upsert(&f.0, CustomModel { name: "m".into(), base_url: "https://a.com".into(), api_key: "k1".into(), ..Default::default() }).unwrap();
        // 完全重复（新条目 id 为空 != 既有 id，命中查重）
        assert!(upsert(&f.0, CustomModel { name: "M".into(), base_url: "https://a.com".into(), api_key: "k1".into(), ..Default::default() }).is_err());
        // 同 base_url 不同 api_key 放行
        assert!(upsert(&f.0, CustomModel { name: "m".into(), base_url: "https://a.com".into(), api_key: "k2".into(), ..Default::default() }).is_ok());
        // 同 api_key 不同 base_url 放行
        assert!(upsert(&f.0, CustomModel { name: "m".into(), base_url: "https://b.com".into(), api_key: "k1".into(), ..Default::default() }).is_ok());
        // 编辑自身（同 id）不命中查重
        let list = load(&f.0);
        let mut edited = list[0].clone();
        edited.note = "updated".into();
        assert!(upsert(&f.0, edited).is_ok());
    }

    /// P1：旧 JSON（无 weight/standby 字段）反序列化 weight=1、standby=false（零迁移）
    #[test]
    fn t07_weight_default() {
        let legacy = r#"{"id":"cm-aaaabbbbcccc","name":"old","base_url":"https://o.com","api_key":"k","enabled":true}"#;
        let m: CustomModel = serde_json::from_str(legacy).unwrap();
        assert_eq!(m.weight, 1);
        assert!(!m.standby);
        // weight=0 视为 1（upsert 归一）
        let f = Fixture::new();
        let list = upsert(&f.0, CustomModel { name: "w".into(), base_url: "https://w.com".into(), weight: 0, ..Default::default() }).unwrap();
        assert_eq!(list[0].weight, 1);
    }

    /// A5/A6：base_url 规范化——scheme 小写化 + 默认端口剥离 + 尾斜杠去除；
    /// 保存归一 + 查重两侧归一命中（`HTTPS://x:443` 变体不得绕过查重）
    #[test]
    fn t08_norm_base_scheme_and_port() {
        assert_eq!(norm_base("HTTPS://api.example.com/"), "https://api.example.com", "scheme 小写化");
        assert_eq!(norm_base("https://x:443"), "https://x", "https 默认端口剥离");
        assert_eq!(norm_base("http://x:80/v1"), "http://x/v1", "http 默认端口剥离");
        assert_eq!(norm_base("https://x:8443"), "https://x:8443", "非默认端口保留");
        assert_eq!(norm_base("https://x"), "https://x", "无端口形态不变");
        assert_eq!(norm_base("https://[::1]:443/v1"), "https://[::1]/v1", "IPv6 带默认端口剥离");
        assert_eq!(norm_base("  https://x/  "), "https://x", "首尾空白");
        let f = Fixture::new();
        let list = upsert(&f.0, CustomModel { name: "m".into(), base_url: "HTTPS://a.com:443".into(), api_key: "k1".into(), ..Default::default() }).unwrap();
        assert_eq!(list[0].base_url, "https://a.com", "保存即归一");
        // 查重两侧归一：HTTPS:// + :443 变体命中既有 https://a.com → 拒绝
        assert!(
            upsert(&f.0, CustomModel { name: "m".into(), base_url: "HTTPS://a.com:443".into(), api_key: "k1".into(), ..Default::default() }).is_err(),
            "大小写/默认端口变体须命中查重"
        );
        // macos_main 补充：scheme 不匹配的默认端口（http 服务的 :443）是显式
        // 非默认端口，与 https://a.com 不同上游，放行
        assert!(
            upsert(&f.0, CustomModel { name: "m".into(), base_url: "http://a.com:443".into(), api_key: "k1".into(), ..Default::default() }).is_ok(),
            "scheme 不匹配的默认端口不剥"
        );
        // macos_main 补充：:4430 等 443 前缀端口不受 strip_suffix 误剥影响
        assert!(
            upsert(&f.0, CustomModel { name: "m".into(), base_url: "https://a.com:4430".into(), api_key: "k1".into(), ..Default::default() }).is_ok(),
            "443 前缀端口不误剥"
        );
    }
}
