//! 自定义模型资源池（OpenAI 兼容上游直通）
//!
//! `data/custom_models.json`：用户以列表方式维护的自定义模型——
//! 名称（请求模型名，路由键）、OpenAI 兼容 API 地址、API Key 及模型其他字段
//! （上下文长度 / 最大输出 / 图片支持 / 倍率 / 备注 / 启用开关）。
//!
//! 调度语义（dispatch.rs ⓪ 段）：请求模型名 canonical 命中 enabled 条目 →
//! 直达自定义上游（用户显式配置优先于内置目录），单源无跨池回退；
//! 未命中或 enabled=false → 回落 Trae/Buddy 统一调度管线。
//!
//! 协议约定：上游为 OpenAI 兼容 chat completions（`{base}/v1/chat/completions`，
//! base 以 `/v1` 结尾时直接拼 `/chat/completions`），Bearer 鉴权。
//! 请求体强制 `stream:true`（与 WB 上游同策略：统一 SSE 处理，非流式本地聚合）。

use std::path::{Path, PathBuf};

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
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub context_length: u64,
    #[serde(default)]
    pub max_tokens: u64,
    #[serde(default)]
    pub supports_image: bool,
    /// 展示倍率（0 = 未声明）
    #[serde(default)]
    pub rate: f64,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub updated_at: i64,
}

impl Default for CustomModel {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            // 新建条目默认启用（serde 缺字段与 Default::default() 口径一致）
            enabled: true,
            context_length: 0,
            max_tokens: 0,
            supports_image: false,
            rate: 0.0,
            note: String::new(),
            updated_at: 0,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CustomModelsFile {
    #[serde(default)]
    pub models: Vec<CustomModel>,
    #[serde(default)]
    pub updated_at: i64,
}

fn file_path(data_dir: &Path) -> PathBuf {
    data_dir.join("data").join("custom_models.json")
}

/// 读取列表（热路径缓存：调度每请求命中判定走 read_json_cached）
pub fn load(data_dir: &Path) -> Vec<CustomModel> {
    crate::fs_utils::read_json_cached::<CustomModelsFile>(&file_path(data_dir))
        .map(|f| f.models)
        .unwrap_or_default()
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
    crate::fs_utils::write_json(
        &file_path(data_dir),
        &CustomModelsFile { models: list, updated_at: now },
    )
}

/// upsert 单条：id 为空或不存在则新增（生成 cm-<12hex> id），存在则整条覆盖。
/// 返回保存后的完整列表。校验：name / base_url 必填；name canonical 不得与其他条目重复。
pub fn upsert(data_dir: &Path, mut m: CustomModel) -> Result<Vec<CustomModel>, String> {
    m.name = m.name.trim().to_string();
    m.base_url = m.base_url.trim().trim_end_matches('/').to_string();
    if m.name.is_empty() {
        return Err("模型名称不能为空".into());
    }
    if m.base_url.is_empty()
        || (!m.base_url.starts_with("http://") && !m.base_url.starts_with("https://"))
    {
        return Err("API 地址必须以 http:// 或 https:// 开头".into());
    }
    let mut list = load(data_dir);
    let canonical = canonical_id(&m.name);
    // 名称唯一性：canonical 冲突即拒绝（同一路由键两个上游无确定性语义）
    if let Some(dup) = list
        .iter()
        .find(|e| canonical_id(&e.name) == canonical && e.id != m.id)
    {
        return Err(format!("模型名称与既有条目重复：{}", dup.name));
    }
    match list.iter_mut().find(|e| e.id == m.id && !m.id.is_empty()) {
        Some(slot) => {
            m.updated_at = now_ts();
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

/// 请求模型名命中 enabled 自定义模型（canonical 匹配；调度热路径入口）。
/// 同 canonical 多条（异常态）取首条。
pub fn find_enabled(data_dir: &Path, model: &str) -> Option<CustomModel> {
    let canonical = canonical_id(model);
    if canonical.is_empty() {
        return None;
    }
    load(data_dir)
        .into_iter()
        .find(|m| m.enabled && canonical_id(&m.name) == canonical)
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
            let dir = std::env::temp_dir().join(format!(
                "twa_custom_{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ));
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
        // 名称 canonical 冲突拒绝（trim+lowercase 同键）
        assert!(upsert(&f.0, CustomModel { name: "MY-Model".into(), base_url: "https://x.com".into(), ..Default::default() })
            .is_err());
        // 编辑（同 id 覆盖）
        let mut edited = list[0].clone();
        edited.enabled = false;
        let list = upsert(&f.0, edited).unwrap();
        assert!(!list[0].enabled);
        assert!(find_enabled(&f.0, "my-model").is_none(), "disabled 不命中");
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

    /// 校验：空名称 / 非法 base_url 拒绝
    #[test]
    fn t04_validation() {
        let f = Fixture::new();
        assert!(upsert(&f.0, CustomModel { name: "".to_string(), base_url: "https://a.com".into(), ..Default::default() }).is_err());
        assert!(upsert(&f.0, CustomModel { name: "x".into(), base_url: "ftp://a.com".into(), ..Default::default() }).is_err());
    }
}
