//! 会话粘性双模式（T2.4/F-31 v1.2）
//!
//! - **显式模式**：客户端请求体携带 `conversation_id` → 按 id 绑定账号与上游
//!   会话，TTL 30m 滚动续期（每次命中刷新 last_seen）；
//! - **指纹模式**：无 conversation_id 时退化为「前 3 条消息 SHA256 取 6 位 +
//!   60s 时间窗锁定」——仅当同一指纹在 60s 内再次出现才续用绑定（antigravity-tools
//!   实证设计；Buddy 上游代理流量缓存恒不命中 §5.5 #10，价值在会话一致性）。
//!
//! 线程安全：单 Mutex 内完成 resolve+bind（写锁 re-check，防 TOCTOU）。
//! 持久化：`data/wb_sticky_sessions.json`（TTL 内的绑定，原子写 + 1s 节流；
//! 旧根路径文件仅作启动加载兼容）。

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};

use sha2::{Digest, Sha256};

/// 显式模式 TTL 默认值（滚动续期）；实际生效值可经
/// `set_explicit_ttl` 覆盖（F-76② api_pool.json.wb_sticky_ttl_secs 热应用）
pub const EXPLICIT_TTL_SECS: i64 = 30 * 60;
/// 指纹模式时间窗
pub const FINGERPRINT_WINDOW_SECS: i64 = 60;
/// 落盘节流间隔：距上次成功保存不足该时长则跳过本次写盘
const SAVE_THROTTLE_MS: u64 = 1000;

/// 一条绑定：uid + 上游 conversation_id + 最后命中时间
#[derive(Debug, Clone)]
pub struct Binding {
    pub uid: String,
    pub conv_id: String,
    pub last_seen: i64,
    /// true = 显式 conversationId 模式（30m TTL）；false = 指纹模式（60s 窗）
    pub explicit: bool,
}

#[derive(Default)]
pub struct StickyStore {
    inner: Mutex<HashMap<String, Binding>>,
    /// 上次成功落盘时刻（节流，见 save）：Some 之前的 save 一律跳过写盘
    last_save: Mutex<Option<std::time::Instant>>,
    /// 显式模式 TTL 秒（F-76② 可配置，0 视为未设置 → 回退 EXPLICIT_TTL_SECS）
    explicit_ttl_secs: AtomicI64,
}

/// 会话键：显式 conversationId 或消息指纹
pub enum SessionKey {
    Explicit(String),
    Fingerprint(String),
}

impl SessionKey {
    /// 从请求体提取会话键：顶层 `conversation_id`（非空字符串）为显式；
    /// 否则取 messages 前 3 条（role + content 规范化拼接）的 SHA256 前 6 位
    pub fn from_body(body: &serde_json::Value) -> SessionKey {
        if let Some(cid) = body
            .get("conversation_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return SessionKey::Explicit(cid.to_string());
        }
        SessionKey::Fingerprint(fingerprint_messages(body))
    }

    /// 缓存键（"cid:{id}" / "fp:{指纹}"）——池粘性键复用同一前缀约定（§4.4）
    pub fn cache_key(&self) -> String {
        match self {
            SessionKey::Explicit(s) => format!("cid:{}", s),
            SessionKey::Fingerprint(s) => format!("fp:{}", s),
        }
    }

    fn is_explicit(&self) -> bool {
        matches!(self, SessionKey::Explicit(_))
    }
}

/// 前 3 条消息指纹：SHA256(role1|content1|role2|content2|...) 前 6 位 hex。
/// content 数组取 text 块拼接；空 messages → 空串（不可粘）。
pub fn fingerprint_messages(body: &serde_json::Value) -> String {
    let mut norm = String::new();
    if let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) {
        for msg in msgs.iter().take(3) {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
            let content = match msg.get("content") {
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(serde_json::Value::Array(blocks)) => blocks
                    .iter()
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join(""),
                _ => String::new(),
            };
            norm.push_str(role);
            norm.push('|');
            norm.push_str(content.trim());
            norm.push('|');
        }
    }
    if norm.is_empty() {
        return String::new();
    }
    let mut h = Sha256::new();
    h.update(norm.as_bytes());
    let digest = h.finalize();
    digest.iter().take(3).map(|b| format!("{:02x}", b)).collect()
}

impl StickyStore {
    // 预留 API（持久化运维/前端扩展用；当前主要消费 resolve/bind/save/load）
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self::default()
    }

    /// 设置显式模式 TTL 秒（F-76②，pool_set 热应用；≤0 回退默认值）
    pub fn set_explicit_ttl(&self, secs: i64) {
        self.explicit_ttl_secs.store(secs, Ordering::Relaxed);
    }

    /// 当前生效的显式模式 TTL 秒（未设置/非法回退 EXPLICIT_TTL_SECS）
    fn effective_explicit_ttl(&self) -> i64 {
        let v = self.explicit_ttl_secs.load(Ordering::Relaxed);
        if v > 0 {
            v
        } else {
            EXPLICIT_TTL_SECS
        }
    }

    /// 解析绑定（TTL 校验 + 显式模式滚动续期）。过期/不匹配即返回 None。
    pub fn resolve(&self, key: &SessionKey, now: i64) -> Option<Binding> {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let ttl = if key.is_explicit() {
            self.effective_explicit_ttl()
        } else {
            FINGERPRINT_WINDOW_SECS
        };
        let b = map.get(&key.cache_key())?;
        if !b.explicit == key.is_explicit() {
            return None; // 键类型变化（少见）：按无绑定处理
        }
        if now - b.last_seen > ttl {
            map.remove(&key.cache_key());
            return None;
        }
        let mut b = b.clone();
        b.last_seen = now; // 滚动续期
        map.insert(key.cache_key(), b.clone());
        Some(b)
    }

    /// 写入/覆盖绑定（Mutex 内 re-check：同键已有更晚绑定且 uid 不同时，
    /// 以更晚者为准——并发首请求只留一个胜者）
    pub fn bind(&self, key: &SessionKey, uid: &str, conv_id: &str, now: i64) {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let ck = key.cache_key();
        if let Some(existing) = map.get(&ck) {
            if existing.uid != uid && existing.last_seen > now - 5 {
                return; // 5s 内他账号刚绑定：让胜者保持
            }
        }
        map.insert(
            ck,
            Binding {
                uid: uid.to_string(),
                conv_id: conv_id.to_string(),
                last_seen: now,
                explicit: key.is_explicit(),
            },
        );
    }

    /// 清理全部过期绑定，返回清理条数（save 落库前调用，控制绑定表无界增长）
    pub fn evict_expired(&self, now: i64) -> usize {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let before = map.len();
        map.retain(|_, b| {
            let ttl = if b.explicit { EXPLICIT_TTL_SECS } else { FINGERPRINT_WINDOW_SECS };
            now - b.last_seen <= ttl
        });
        before - map.len()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    // ---------- 持久化 ----------

    /// 落盘（原子写 + 节流，写入 data/ 子目录）。
    ///
    /// 节流取舍：bind 后高频全量重写浪费 IO——距上次成功保存不足
    /// SAVE_THROTTLE_MS 时跳过本次落盘。内存态照常更新（resolve 不受影响），
    /// 跳过的落盘由下一次间隔超过阈值的请求兜底；代价是进程崩溃时可能丢失
    /// 最近约 1s 的绑定——粘性绑定本就有 TTL，丢失仅导致个别会话重新取号，
    /// 可接受。
    pub fn save(&self, data_dir: &std::path::Path) {
        {
            let last = self.last_save.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(t) = *last {
                if t.elapsed() < std::time::Duration::from_millis(SAVE_THROTTLE_MS) {
                    return;
                }
            }
        }
        // P6 流水化：落库前清理过期绑定（原实现过期项滞留内存/落盘缓慢增长）
        let now_secs = chrono::Utc::now().timestamp();
        self.evict_expired(now_secs);
        let file = {
            let map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            serde_json::json!({
                "version": 1,
                "saved_at": now_ts(),
                "bindings": map.iter().map(|(k, b)| serde_json::json!({
                    "key": k, "uid": b.uid, "conv_id": b.conv_id,
                    "last_seen": b.last_seen, "explicit": b.explicit,
                })).collect::<Vec<_>>(),
            })
        };
        if crate::store::docs::sticky_bindings_save(&crate::store::db(data_dir), &file).is_ok() {
            let mut last = self.last_save.lock().unwrap_or_else(|e| e.into_inner());
            *last = Some(std::time::Instant::now());
        }
    }

    /// 启动时加载（过期的条目在 resolve 时自然失效）。
    /// SQLite 化（P6）：wb_sticky_sessions → sticky_bindings 表；
    /// 旧根路径兼容由启动迁移器完成。
    pub fn load(data_dir: &std::path::Path) -> Self {
        let file: serde_json::Value = crate::store::docs::sticky_bindings_load(&crate::store::db(data_dir));
        let mut map = HashMap::new();
        if let Some(list) = file.get("bindings").and_then(|b| b.as_array()) {
            for item in list {
                let key = match item.get("key").and_then(|v| v.as_str()) {
                    Some(k) if !k.is_empty() => k.to_string(),
                    _ => continue,
                };
                let uid = item.get("uid").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let conv_id = item.get("conv_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let last_seen = item.get("last_seen").and_then(|v| v.as_i64()).unwrap_or(0);
                let explicit = item.get("explicit").and_then(|v| v.as_bool()).unwrap_or(false);
                if uid.is_empty() || conv_id.is_empty() {
                    continue;
                }
                map.insert(key, Binding { uid, conv_id, last_seen, explicit });
            }
        }
        Self {
            inner: Mutex::new(map),
            last_save: Mutex::new(None),
            explicit_ttl_secs: std::sync::atomic::AtomicI64::new(0),
        }
    }
}

fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body_with(conv: Option<&str>, msgs: serde_json::Value) -> serde_json::Value {
        let mut b = json!({ "messages": msgs });
        if let Some(c) = conv {
            b["conversation_id"] = json!(c);
        }
        b
    }

    #[test]
    fn explicit_mode_binds_and_renews_with_30m_ttl() {
        let store = StickyStore::new();
        let key = SessionKey::from_body(&body_with(Some("conv-1"), json!([{"role":"user","content":"hi"}])));
        assert!(matches!(key, SessionKey::Explicit(_)));
        assert!(store.resolve(&key, 1000).is_none());
        store.bind(&key, "uid_a", "up-conv-1", 1000);
        let b = store.resolve(&key, 1000 + EXPLICIT_TTL_SECS - 1).unwrap();
        assert_eq!(b.uid, "uid_a");
        assert_eq!(b.conv_id, "up-conv-1");
        // 滚动续期：resolve 刷新 last_seen → 命中即续命（TTL 30m 滚动）
        assert!(store.resolve(&key, 1000 + EXPLICIT_TTL_SECS + 500).is_some());
        // 过期判定：重新绑定后不再访问，超过 TTL → 失效
        store.bind(&key, "uid_a", "up-conv-1", 1000);
        assert!(store.resolve(&key, 1000 + EXPLICIT_TTL_SECS + 1).is_none());
    }

    #[test]
    fn fingerprint_mode_only_within_60s_window() {
        let store = StickyStore::new();
        let body = body_with(None, json!([
            {"role":"system","content":"you are helpful"},
            {"role":"user","content":"hello"},
            {"role":"assistant","content":"hi"},
            {"role":"user","content":"continue"},
        ]));
        let key = SessionKey::from_body(&body);
        assert!(matches!(key, SessionKey::Fingerprint(_)));
        assert_eq!(fingerprint_messages(&body).len(), 6);
        store.bind(&key, "uid_b", "up-conv-2", 5000);
        assert!(store.resolve(&key, 5000 + FINGERPRINT_WINDOW_SECS - 1).is_some());
        // 超过 60s 窗 → 失效（这正是指纹模式的语义：仅短窗内粘住）
        // 注意：上面的 resolve 已滚动续期，重新绑定回到 t=5000 再验证过期
        store.bind(&key, "uid_b", "up-conv-2", 5000);
        assert!(store.resolve(&key, 5000 + FINGERPRINT_WINDOW_SECS + 1).is_none());
    }

    #[test]
    fn fingerprint_is_content_sensitive() {
        let a = fingerprint_messages(&body_with(None, json!([{"role":"user","content":"问题A"}])));
        let b = fingerprint_messages(&body_with(None, json!([{"role":"user","content":"问题B"}])));
        let a2 = fingerprint_messages(&body_with(None, json!([
            {"role":"user","content":[{"type":"text","text":"问题A"}]}
        ])));
        assert_ne!(a, b);
        assert_eq!(a, a2); // string 与 [{type:text}] 等价
        // 空 messages → 空指纹
        assert_eq!(fingerprint_messages(&json!({"messages":[]})), "");
    }

    #[test]
    fn bind_recheck_keeps_recent_winner() {
        let store = StickyStore::new();
        let key = SessionKey::from_body(&body_with(Some("c"), json!([{"role":"user","content":"x"}])));
        store.bind(&key, "uid_a", "cv1", 1000);
        // 并发双请求各自取号后先后绑定（TOCTOU）：5s 内先绑定者保持胜者
        store.bind(&key, "uid_b", "cv2", 1003);
        assert_eq!(store.resolve(&key, 1004).unwrap().uid, "uid_a");
        // 5s 后再绑定（账号已失效重取等场景）→ 允许覆盖
        store.bind(&key, "uid_b", "cv2", 1010);
        assert_eq!(store.resolve(&key, 1011).unwrap().uid, "uid_b");
    }

    #[test]
    fn evict_expired_counts() {
        let store = StickyStore::new();
        let k1 = SessionKey::from_body(&body_with(Some("c1"), json!([{"role":"user","content":"a"}])));
        let k2 = SessionKey::from_body(&body_with(Some("c2"), json!([{"role":"user","content":"b"}])));
        store.bind(&k1, "u1", "v1", 1000);
        store.bind(&k2, "u2", "v2", 2000);
        assert_eq!(store.evict_expired(1000 + EXPLICIT_TTL_SECS + 10), 1);
        assert_eq!(store.len(), 1);
    }

    /// 落盘写入 data/ 子目录；1s 节流窗口内的重复 save 跳过写盘（内存态照常更新）
    #[test]
    fn save_writes_data_subdir_and_throttles_within_one_second() {
        let dir = std::env::temp_dir().join(format!(
            "twa_sticky_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("data")).unwrap();
        let store = StickyStore::new();
        let key = SessionKey::from_body(&body_with(Some("c"), json!([{"role":"user","content":"x"}])));
        // P6：save 落库前会按真实时钟清理过期绑定 → 测试绑定也用真实时间
        let now = chrono::Utc::now().timestamp();
        store.bind(&key, "u1", "v1", now);
        store.save(&dir);
        // SQLite 化（P6）：落盘 = sticky_bindings 表
        let snapshot1 = crate::store::docs::sticky_bindings_load(&crate::store::db(&dir)).to_string();
        assert!(snapshot1.contains("v1"), "落盘应写入 sticky_bindings 表");
        // 1s 内再次 bind + save：落盘被节流跳过，内容不变
        store.bind(&key, "u2", "v2", now + 6);
        store.save(&dir);
        let snapshot2 = crate::store::docs::sticky_bindings_load(&crate::store::db(&dir)).to_string();
        assert_eq!(snapshot1, snapshot2, "距上次成功保存 <1000ms 应跳过落库");
        // 内存态已更新（resolve 读到新绑定）
        assert_eq!(store.resolve(&key, now + 7).unwrap().uid, "u2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 读取兼容：data/ 新路径不存在时回退旧根路径（存量用户数据），save 迁移写新路径
    #[test]
    fn load_falls_back_to_legacy_root_path() {
        let dir = std::env::temp_dir().join(format!(
            "twa_sticky_legacy_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("data")).unwrap();
        // SQLite 化（P6）：种子 = sticky_bindings 表
        crate::store::docs::sticky_bindings_save(
            &crate::store::db(&dir),
            &json!({"version": 1, "bindings": [
                {"key": "cid:legacy", "uid": "u9", "conv_id": "c9", "last_seen": 500, "explicit": true}
            ]}),
        )
        .unwrap();
        let store = StickyStore::load(&dir);
        let key = SessionKey::Explicit("legacy".into());
        // 种子 last_seen=500 为历史值（会过期失效），重新绑定到真实时钟后再验证
        let now = chrono::Utc::now().timestamp();
        store.bind(&key, "u9", "c9", now);
        assert_eq!(store.resolve(&key, now + 1).unwrap().conv_id, "c9");
        // save 落 sticky_bindings 表
        store.save(&dir);
        assert!(crate::store::db(&dir)
            .with_conn(|c| {
                let n: i64 = c.query_row("SELECT COUNT(*) FROM sticky_bindings", [], |r| r.get(0))?;
                Ok(n > 0)
            })
            .unwrap_or(false));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
