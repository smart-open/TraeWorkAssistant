//! 多管理员附加令牌（Phase 3 T12b）：主 token（env `AIWORK_ADMIN_TOKEN` /
//! conf/admin_token）之外的可吊销附加令牌，登录与鉴权时并集校验。
//!
//! - 存 kv `admin_tokens`：`{ tokens: [{ id, token, label, created_at }] }`；
//! - 主 token 永不进入 kv（不可被吊销），防锁死；
//! - 进程内 LazyLock 缓存（HashSet），create/revoke 后热更新，
//!   鉴权路径只读快照，不触库；
//! - 附加令牌上限 20 个，防无限膨胀。

use std::collections::HashSet;
use std::sync::{Arc, LazyLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use aiwork_core::state::AppState;
use serde::{Deserialize, Serialize};

/// kv 键
const KV_KEY: &str = "admin_tokens";
/// 附加令牌数量上限
const MAX_TOKENS: usize = 20;

/// 附加令牌条目（kv 持久化；token 明文仅落库与创建响应，列表接口一律掩码）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminTokenEntry {
    /// 条目 id（吊销定位用）
    pub id: String,
    /// 令牌明文（64 hex，与主 token 同源随机实现）
    pub token: String,
    /// 备注名称
    pub label: String,
    /// 创建时间（epoch 毫秒，前端格式化）
    pub created_at: i64,
}

/// kv 文档结构
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AdminTokensFile {
    tokens: Vec<AdminTokenEntry>,
}

/// 列表视图（token 掩码）
#[derive(Debug, Clone, Serialize)]
pub struct AdminTokenView {
    pub id: String,
    pub token_masked: String,
    pub label: String,
    pub created_at: i64,
}

/// 进程内附加令牌缓存（热生效）：鉴权路径只读 Arc 快照
static CACHE: LazyLock<RwLock<Arc<HashSet<String>>>> =
    LazyLock::new(|| RwLock::new(Arc::new(HashSet::new())));

/// 从 kv 重载缓存（AdminState::new 装配时与 create/revoke 后各调用一次）
pub fn reload(state: &AppState) {
    let file: AdminTokensFile = aiwork_core::store::db(&state.data_dir).kv_get(KV_KEY);
    let set: HashSet<String> = file.tokens.iter().map(|t| t.token.clone()).collect();
    *CACHE.write().expect("admin_tokens 缓存锁中毒") = Arc::new(set);
}

/// 附加令牌命中校验（主 token 由调用方先行比对，此处仅查附加集）
pub fn contains(candidate: &str) -> bool {
    CACHE
        .read()
        .expect("admin_tokens 缓存锁中毒")
        .contains(candidate)
}

/// 令牌掩码：前 4 + … + 后 4；过短串整体打码
fn mask(token: &str) -> String {
    let cs: Vec<char> = token.chars().collect();
    if cs.len() <= 8 {
        "****".to_string()
    } else {
        format!("{}…{}", cs[..4].iter().collect::<String>(), cs[cs.len() - 4..].iter().collect::<String>())
    }
}

/// 列出附加令牌（token 掩码；不含主 token）
pub fn list(state: &AppState) -> Vec<AdminTokenView> {
    let file: AdminTokensFile = aiwork_core::store::db(&state.data_dir).kv_get(KV_KEY);
    file.tokens
        .iter()
        .map(|t| AdminTokenView {
            id: t.id.clone(),
            token_masked: mask(&t.token),
            label: t.label.clone(),
            created_at: t.created_at,
        })
        .collect()
}

/// 创建附加令牌：生成 64 hex 明文并落库（创建响应中明文仅返回一次）
pub fn create(state: &AppState, label: String) -> Result<AdminTokenEntry, String> {
    let label = label.trim().to_string();
    if label.is_empty() {
        return Err("参数错误: 备注名称不能为空".to_string());
    }
    let mut file: AdminTokensFile = aiwork_core::store::db(&state.data_dir).kv_get(KV_KEY);
    if file.tokens.len() >= MAX_TOKENS {
        return Err(format!("附加管理员令牌已达上限（{MAX_TOKENS} 个）"));
    }
    let entry = AdminTokenEntry {
        id: aiwork_core::commands::oauth::random_hex(8),
        token: aiwork_core::commands::oauth::random_hex(64),
        label,
        created_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0),
    };
    file.tokens.push(entry.clone());
    aiwork_core::store::db(&state.data_dir)
        .kv_set(KV_KEY, &file)
        .map_err(|e| format!("保存失败: {e}"))?;
    reload(state);
    let msg = format!("附加管理员令牌已创建: {}（{}）", entry.label, entry.id);
    println!("{msg}");
    aiwork_core::fs_utils::app_log(&state.data_dir, &msg);
    Ok(entry)
}

/// 吊销附加令牌（按 id）；主 token 不在本集合，天然不可吊销
pub fn revoke(state: &AppState, id: String) -> Result<(), String> {
    let mut file: AdminTokensFile = aiwork_core::store::db(&state.data_dir).kv_get(KV_KEY);
    let before = file.tokens.len();
    file.tokens.retain(|t| t.id != id);
    if file.tokens.len() == before {
        return Err(format!("令牌不存在: {id}"));
    }
    aiwork_core::store::db(&state.data_dir)
        .kv_set(KV_KEY, &file)
        .map_err(|e| format!("保存失败: {e}"))?;
    reload(state);
    let msg = format!("附加管理员令牌已吊销: {id}");
    println!("{msg}");
    aiwork_core::fs_utils::app_log(&state.data_dir, &msg);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// 串行化涉及全局 CACHE 的测试，避免并行互扰
    static CACHE_LOCK: StdMutex<()> = StdMutex::new(());

    fn test_state(tag: &str) -> AppState {
        let dir = std::env::temp_dir().join(format!("aiwork_admin_tokens_{tag}_{}", std::process::id()));
        AppState {
            data_dir: dir,
            jwt_refresh_lock: Arc::new(std::sync::Mutex::new(())),
        }
    }

    /// 掩码规则：常规 64 hex 前 4 后 4；短串整体打码
    #[test]
    fn mask_rules() {
        let long = "a".repeat(64);
        assert_eq!(mask(&long), "aaaa…aaaa");
        assert_eq!(mask("short"), "****");
    }

    /// 创建 → 缓存命中（可登录）→ 吊销 → 缓存失效；主 token 不受影响
    #[test]
    fn create_verify_revoke_roundtrip() {
        let _g = CACHE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let state = test_state("rt");
        reload(&state);
        // 空标签拒绝
        assert!(create(&state, "  ".to_string()).is_err());

        let entry = create(&state, "测试用令牌".to_string()).unwrap();
        assert_eq!(entry.token.len(), 64);
        reload(&state);
        assert!(contains(&entry.token), "创建后应可命中缓存");

        // 列表返回掩码，不泄露明文
        let views = list(&state);
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].token_masked, format!("{}…{}", &entry.token[..4], &entry.token[60..]));
        assert!(!views[0].token_masked.contains(&entry.token));

        // 吊销后立即失效
        revoke(&state, entry.id.clone()).unwrap();
        assert!(!contains(&entry.token), "吊销后应失效");
        // 不存在的 id 报错
        assert!(revoke(&state, "no-such-id".to_string()).is_err());
    }
}
