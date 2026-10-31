//! 按模型长上下文设置（管理面单模型设置，**按池隔离存储**，各池页面各管各的）。
//!
//! 两池机制不同、语义不可互译，因此各存一张表、互不读取：
//! - **Trae**：kv `trae_long_context` = `[canonical_id]`（已开启长上下文的模型集合）。
//!   开启 → 该模型请求注入 `is_max_mode:true`（与 `-max` 入口为「或」关系，同受
//!   `efforts::TRAE_MAX_MODE_REF` 支持表门控；表外模型拒绝开启）；关闭 = 跟随上游
//!   （不注入，亦不关闭请求显式带的 `-max`）
//! - **Qoder**：kv `qoder_context_length` = `{canonical_id: tokens}`。设置 → 请求体
//!   `parameters.context_length = tokens`（官方客户端同名数值字段；上游档位白名单未知，
//!   原样下发，上游 400 时清除设置即可恢复）；未设置 = 跟随上游（不下发字段）。
//!   写入受 Qoder 目录门控（与注入生效面对齐：注入点 resolve 不命中的模型 ID
//!   落库也永不生效，拼错/下架即拒）；清除不受门控（残留条目可清理，长度检查
//!   同理仅拦写入——超长存量条目仍可清理）
//!
//! Buddy / BuddyAI 请求体无上下文字段（ACP 会话级配置），不提供设置。
//! 两表均经 config_cache 缓存，写路径锁内直读库读-改-写并显式失效（保存即对下一请求生效）。
//! **未设置 = 零行为差异**：各池注入点在 lookup 为空时不触碰请求体。
//! 只影响请求侧，不改 /v1/models 目录声明与计费。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde::Serialize;

use super::unified_catalog::canonical_id;
use crate::tasks::qoder_upstream::QoderRegion;

/// Trae 长上下文开关表 kv 键（集合，稳定序）
pub const TRAE_KV_KEY: &str = "trae_long_context";

/// Qoder 上下文长度表 kv 键（`{canonical_id: tokens}`，稳定序）
pub const QODER_KV_KEY: &str = "qoder_context_length";

/// Qoder 合法区间（token）：下限挡住误填「200」这类 K 单位遗漏，上限挡住多打一个 0
pub const QODER_MIN_TOKENS: u64 = 1_024;
pub const QODER_MAX_TOKENS: u64 = 10_000_000;

/// 单表条目上限（防管理面误操作把整表撑大；热路径整表反序列化）
pub const MAX_ENTRIES: usize = 1_000;

/// 模型 ID 入参长度上限（字节；审查 Low：无上限的超长串会原样进 kv 表与列表页）。
/// 仅门控写入路径（trae_set 开启 / qoder_set 设置），清除放行——超长存量条目
/// 仍必须可清理（与目录门控「清除不受门控」同理）
pub const MAX_MODEL_ID_LEN: usize = 128;

/// 整表读-改-写互斥：kv 为单键整表覆盖写，管理面并发保存时两边读到同一份旧表
/// 各自写回会丢更新。对齐 custom_models UPSERT_LOCK / runtime.rs POOL_CFG_LOCK 范式：
/// load → 改 → kv_set → invalidate 整段持锁；纯读（lookup）无需持锁。
/// 两表共用一把叶子锁（写频率极低）——锁内不取其他池锁/配置锁
static SET_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn cfg_write_lock() -> MutexGuard<'static, ()> {
    SET_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// 读取整表（缺失/损坏回退空表），经 config_cache
fn load_table<T>(data_dir: &Path, key: &'static str) -> T
where
    T: Serialize + DeserializeOwned + Default,
{
    super::config_cache::get_or_load(data_dir, key, || load_uncached(data_dir, key))
}

/// 直读库（不经缓存）：缓存 miss 的加载器 / 写路径锁内读-改-写共用
fn load_uncached<T: DeserializeOwned + Default>(data_dir: &Path, key: &'static str) -> T {
    crate::store::db(data_dir).kv_get(key)
}

/// 锁内读-改-写：`edit` 返回 Ok(true) 表示表有变化才落库。锁内直读库、不走
/// config_cache——缓存可能是 TTL 内的进程外旧值，基于缓存改写会覆盖进程外写入
fn update_table<T>(
    data_dir: &Path,
    key: &'static str,
    edit: impl FnOnce(&mut T) -> Result<bool, String>,
) -> Result<(), String>
where
    T: Serialize + DeserializeOwned + Default,
{
    let _g = cfg_write_lock();
    let mut table: T = load_uncached(data_dir, key);
    if edit(&mut table)? {
        let res = crate::store::db(data_dir).kv_set(key, &table);
        super::config_cache::invalidate(data_dir, key);
        res?;
    }
    Ok(())
}

fn non_empty_id(model: &str) -> Result<String, String> {
    let id = canonical_id(model);
    if id.is_empty() {
        Err("模型 ID 不能为空".into())
    } else {
        Ok(id)
    }
}

/// 写入路径长度门控（审查 Low 修复）：仅拦写入；清除放行——超长存量条目
/// 仍必须可清理（与目录门控「清除不受门控」同理，否则成永久死数据）
fn check_id_len(id: &str) -> Result<(), String> {
    if id.len() > MAX_MODEL_ID_LEN {
        Err(format!("模型 ID 过长（超过 {MAX_MODEL_ID_LEN} 字节）"))
    } else {
        Ok(())
    }
}

// ==================== Trae：长上下文开关 ====================

/// 已开启长上下文的 Trae 模型（canonical_id）
pub fn trae_list(data_dir: &Path) -> BTreeSet<String> {
    load_table(data_dir, TRAE_KV_KEY)
}

/// 开启/关闭单模型长上下文；返回生效后的状态。
/// 开启要求模型在 Max Mode 支持表内（表外开启无法执行，整体拒绝不落库）；
/// 关闭任何模型都允许（含已下架/表外/超长 ID 的残留条目）
pub fn trae_set(data_dir: &Path, model: &str, enabled: bool) -> Result<bool, String> {
    let id = non_empty_id(model)?;
    if enabled {
        check_id_len(&id)?;
        if !super::efforts::trae_max_mode_supported(&id) {
            return Err(format!("模型 {id} 不支持长上下文"));
        }
    }
    update_table(data_dir, TRAE_KV_KEY, |set: &mut BTreeSet<String>| {
        if !enabled {
            return Ok(set.remove(&id));
        }
        if !set.contains(&id) && set.len() >= MAX_ENTRIES {
            return Err(format!("长上下文设置已达上限 {MAX_ENTRIES} 条"));
        }
        Ok(set.insert(id))
    })?;
    Ok(enabled)
}

/// Trae 出站入口（routes 层调用）：该模型是否开启了长上下文（且仍在支持表内）。
/// 未开启 → false 且无任何副作用
pub fn trae_wants_max(data_dir: &Path, model: &str) -> bool {
    let id = canonical_id(model);
    !id.is_empty()
        && super::efforts::trae_max_mode_supported(&id)
        && trae_list(data_dir).contains(&id)
}

// ==================== Qoder：parameters.context_length ====================

/// 已设置的 Qoder 上下文长度表
pub fn qoder_list(data_dir: &Path) -> BTreeMap<String, u64> {
    load_table(data_dir, QODER_KV_KEY)
}

/// 写入/清除单模型上下文长度：`tokens` 为 None 或 0 = 清除（恢复跟随上游）。
/// 返回生效后的值（清除 → None）。校验失败整体拒绝、不落库。
/// 写入受目录门控（审查 Medium）：拼错/下架的模型 ID 注入点 resolve 不命中即
/// 永不生效，静默落库只会误导用户——与 Trae 侧支持表门控对齐；清除不受门控
/// （残留条目必须可清理，下架模型的旧设置才能被移除；长度检查同理放行）
pub fn qoder_set(data_dir: &Path, model: &str, tokens: Option<u64>) -> Result<Option<u64>, String> {
    let id = non_empty_id(model)?;
    let tokens = tokens.filter(|n| *n > 0);
    if let Some(n) = tokens {
        check_id_len(&id)?;
        if !(QODER_MIN_TOKENS..=QODER_MAX_TOKENS).contains(&n) {
            return Err(format!(
                "上下文长度需在 {QODER_MIN_TOKENS}~{QODER_MAX_TOKENS} token 之间（收到 {n}）"
            ));
        }
        if crate::tasks::qoder_upstream::resolve(&id, QoderRegion::Cn).is_none() {
            return Err(format!("模型 {id} 不在 Qoder 目录中，设置不会生效"));
        }
    }
    update_table(data_dir, QODER_KV_KEY, |map: &mut BTreeMap<String, u64>| match tokens {
        Some(n) => {
            if !map.contains_key(&id) && map.len() >= MAX_ENTRIES {
                return Err(format!("上下文长度设置已达上限 {MAX_ENTRIES} 条"));
            }
            Ok(map.insert(id, n) != Some(n))
        }
        None => Ok(map.remove(&id).is_some()),
    })?;
    Ok(tokens)
}

/// 注入日志节流表（审查 Low）：功能开启后每请求命中注入点都会落一行 app.log，
/// 高频流量下会把日志轮转挤到 .old、冲掉其他诊断行。按模型 60s 节流，窗口内
/// 只落首条。表不做主动回收（条目数 = 实际产生注入的模型数，量级极小），
/// 重启即清零。进程级全局表：多 data_dir 场景（测试）按模型共享节流窗口，可接受
static INJECT_LOG_THROTTLE: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();

const INJECT_LOG_THROTTLE_SECS: u64 = 60;

/// 返回 true = 本条允许落盘（并占用该模型的节流窗口）
fn inject_log_allowed(model_id: &str) -> bool {
    let mut map = INJECT_LOG_THROTTLE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    match map.get(model_id) {
        Some(t) if t.elapsed() < Duration::from_secs(INJECT_LOG_THROTTLE_SECS) => false,
        _ => {
            map.insert(model_id.to_string(), Instant::now());
            true
        }
    }
}

/// Qoder 出站入口（qoder_route 调用）：目标值（None = 未设置，不下发字段）。
/// 已设置时落一行 app.log（按模型 60s 节流）
pub fn qoder_target(data_dir: &Path, model: &str) -> Option<u64> {
    let id = canonical_id(model);
    if id.is_empty() {
        return None;
    }
    let target = qoder_list(data_dir).get(&id).copied()?;
    if inject_log_allowed(&id) {
        crate::fs_utils::app_log(
            data_dir,
            &format!("context_length qoder: model={model} target={target} parameters.context_length injected"),
        );
    }
    Some(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ctxwin_test_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn qoder_set_lookup_clear_roundtrip() {
        let dir = tmp_dir("qoder_roundtrip");
        assert_eq!(qoder_target(&dir, "glm-5.3"), None, "未设置 = 跟随上游");
        assert_eq!(qoder_set(&dir, " GLM-5.3 ", Some(1_000_000)).unwrap(), Some(1_000_000));
        // canonical 归一：大小写/空白变体命中同一条
        assert_eq!(qoder_target(&dir, "glm-5.3"), Some(1_000_000));
        assert_eq!(qoder_target(&dir, "Glm-5.3"), Some(1_000_000));
        assert_eq!(qoder_target(&dir, "glm-5.2"), None);
        // 0 / None 均为清除
        assert_eq!(qoder_set(&dir, "glm-5.3", Some(0)).unwrap(), None);
        assert_eq!(qoder_target(&dir, "glm-5.3"), None);
        qoder_set(&dir, "glm-5.3", Some(200_000)).unwrap();
        assert_eq!(qoder_set(&dir, "glm-5.3", None).unwrap(), None);
        assert!(qoder_list(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn qoder_rejects_invalid_without_writing() {
        let dir = tmp_dir("qoder_invalid");
        assert!(qoder_set(&dir, "  ", Some(200_000)).is_err());
        assert!(qoder_set(&dir, "m", Some(200)).is_err(), "K 单位遗漏");
        assert!(qoder_set(&dir, "m", Some(QODER_MAX_TOKENS + 1)).is_err());
        // 目录门控（审查 Medium）：目录外模型写入整体拒绝、不落库；清除不受门控
        assert!(qoder_set(&dir, "no-such-model", Some(128_000)).is_err(), "目录外写入被拒");
        assert!(crate::store::db(&dir).kv_get_raw(QODER_KV_KEY).is_none(), "门控拒绝后不落库");
        assert_eq!(qoder_set(&dir, "no-such-model", None).unwrap(), None, "清除不受门控");
        // 模型 ID 长度上限（审查 Low）：trae/qoder 双入口写入同拒
        let long_id = "a".repeat(MAX_MODEL_ID_LEN + 1);
        assert!(qoder_set(&dir, &long_id, Some(128_000)).is_err());
        assert!(trae_set(&dir, &long_id, true).is_err());
        // 清除路径放行长度检查（审查 Low 修复）：超长存量条目仍可清理
        assert_eq!(qoder_set(&dir, &long_id, None).unwrap(), None);
        assert!(!trae_set(&dir, &long_id, false).unwrap());
        assert!(crate::store::db(&dir).kv_get_raw(TRAE_KV_KEY).is_none());
        assert!(crate::store::db(&dir).kv_get_raw(QODER_KV_KEY).is_none(), "拒绝后不落库");
        // 清除不存在的条目：不报错、不落库
        assert_eq!(qoder_set(&dir, "m", None).unwrap(), None);
        assert!(crate::store::db(&dir).kv_get_raw(QODER_KV_KEY).is_none());
        // 库里残留目录外条目（手工改库 / 模型下架）→ 清除不受门控，仍可清理
        crate::store::db(&dir)
            .kv_set(QODER_KV_KEY, &BTreeMap::from([("legacy-model".to_string(), 128_000u64)]))
            .unwrap();
        crate::api_server::config_cache::invalidate(&dir, QODER_KV_KEY);
        assert_eq!(qoder_set(&dir, "legacy-model", None).unwrap(), None);
        assert!(qoder_list(&dir).is_empty());
        // 端点合法（须用目录内模型 id）
        assert!(qoder_set(&dir, "glm-5.3", Some(QODER_MIN_TOKENS)).is_ok());
        assert!(qoder_set(&dir, "glm-5.3", Some(QODER_MAX_TOKENS)).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn trae_toggle_roundtrip() {
        let dir = tmp_dir("trae_toggle");
        assert!(!trae_wants_max(&dir, "glm-5.3"), "未开启 = 跟随上游");
        assert!(trae_set(&dir, " GLM-5.3 ", true).unwrap());
        assert!(trae_wants_max(&dir, "glm-5.3"));
        assert!(trae_wants_max(&dir, "Glm-5.3"));
        assert!(!trae_wants_max(&dir, "glm-5.2"));
        // 重复开启幂等
        assert!(trae_set(&dir, "glm-5.3", true).unwrap());
        assert_eq!(trae_list(&dir).len(), 1);
        assert!(!trae_set(&dir, "glm-5.3", false).unwrap());
        assert!(!trae_wants_max(&dir, "glm-5.3"));
        assert!(trae_list(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn trae_rejects_unsupported_model() {
        let dir = tmp_dir("trae_unsupported");
        assert!(trae_set(&dir, "doubao-seed-code", true).is_err(), "支持表外不可开启");
        assert!(trae_set(&dir, "  ", true).is_err());
        assert!(crate::store::db(&dir).kv_get_raw(TRAE_KV_KEY).is_none(), "拒绝后不落库");
        // 关闭表外/不存在条目：不报错、不落库
        assert!(!trae_set(&dir, "doubao-seed-code", false).unwrap());
        assert!(crate::store::db(&dir).kv_get_raw(TRAE_KV_KEY).is_none());
        // 库里残留表外条目（手工改库 / 支持表收缩）→ 出站不注入
        crate::store::db(&dir)
            .kv_set(TRAE_KV_KEY, &BTreeSet::from(["doubao-seed-code".to_string()]))
            .unwrap();
        crate::api_server::config_cache::invalidate(&dir, TRAE_KV_KEY);
        assert!(!trae_wants_max(&dir, "doubao-seed-code"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pools_are_isolated() {
        // 同一模型：Trae 开长上下文不影响 Qoder，Qoder 设数值不影响 Trae
        let dir = tmp_dir("isolated");
        trae_set(&dir, "glm-5.3", true).unwrap();
        assert_eq!(qoder_target(&dir, "glm-5.3"), None, "Trae 开关不外溢到 Qoder");
        assert!(qoder_list(&dir).is_empty());

        qoder_set(&dir, "glm-5.2", Some(1_000_000)).unwrap();
        assert!(!trae_wants_max(&dir, "glm-5.2"), "Qoder 数值不外溢到 Trae");
        assert_eq!(trae_list(&dir), BTreeSet::from(["glm-5.3".to_string()]));

        // 清除一侧不动另一侧
        qoder_set(&dir, "glm-5.3", Some(256_000)).unwrap();
        trae_set(&dir, "glm-5.3", false).unwrap();
        assert_eq!(qoder_target(&dir, "glm-5.3"), Some(256_000));
        qoder_set(&dir, "glm-5.3", None).unwrap();
        trae_set(&dir, "glm-5.3", true).unwrap();
        qoder_set(&dir, "glm-5.3", Some(128_000)).unwrap();
        qoder_set(&dir, "glm-5.3", None).unwrap();
        assert!(trae_wants_max(&dir, "glm-5.3"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pool_entries_noop_when_unset() {
        let dir = tmp_dir("noop");
        assert!(!trae_wants_max(&dir, "glm-5.3"));
        assert_eq!(qoder_target(&dir, "qwen3.8-max"), None);
        assert!(!dir.join("logs").join("app.log").exists(), "未设置时不落任何日志");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_set_keeps_every_entry() {
        // 整表读-改-写无互斥时，20 线程并发保存全部返回成功但最终只剩少数条目
        // （后写者抹掉先写者）。同时混入锁外热路径读与另一张表的写；断言前不手动
        // 失效——迟到回填的旧快照须已被 config_cache 代际校验丢弃。
        // qoder_set 受目录门控（审查 Medium）：改用目录内模型 id（canonical 归一
        // 后去重），20 线程按 i % ids.len() 轮询分配，tokens 绑定 id 序号——并发
        // 交错下最终值确定，断言「每条都在且值正确」仍能检测丢更新
        let dir = tmp_dir("concurrent");
        // 持目录测试锁：list() 读的是进程级全局目录，与 qoder_route 的 adopt_remote
        // 类测试并发时可能读到测试注入的临时目录——复位后 resolve 不命中会 unwrap
        // panic（flaky）。持锁令本测试与目录切换测试互斥，list/resolve 全程读同一
        // 份静态兜底目录（main 同源代码未加固，移植时补上）
        let (_catalog_lock, _catalog_reset) = crate::tasks::qoder_upstream::catalog_test_guard();
        let trae_ids: Vec<&str> = efforts_ref();
        let qoder_ids: Vec<String> = crate::tasks::qoder_upstream::list()
            .iter()
            .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();
        assert!(qoder_ids.len() >= 2, "兜底目录为空，门控测试失去意义");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(60));
        let handles: Vec<_> = (0..20u64)
            .flat_map(|i| {
                let (d1, b1) = (dir.clone(), barrier.clone());
                let (d2, b2) = (dir.clone(), barrier.clone());
                let (d3, b3) = (dir.clone(), barrier.clone());
                let trae_id = trae_ids[i as usize % trae_ids.len()].to_string();
                let qoder_idx = i as usize % qoder_ids.len();
                let qoder_id = qoder_ids[qoder_idx].clone();
                let reader_id = qoder_ids[0].clone();
                [
                    std::thread::spawn(move || {
                        b1.wait();
                        qoder_set(&d1, &qoder_id, Some(128_000 + qoder_idx as u64)).unwrap();
                    }),
                    std::thread::spawn(move || {
                        b2.wait();
                        trae_set(&d2, &trae_id, true).unwrap();
                    }),
                    std::thread::spawn(move || {
                        b3.wait();
                        for _ in 0..5 {
                            let _ = qoder_target(&d3, &reader_id);
                            let _ = trae_wants_max(&d3, "glm-5.3");
                        }
                    }),
                ]
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let map = qoder_list(&dir);
        let expected: Vec<String> = qoder_ids.iter().map(|id| canonical_id(id)).collect();
        assert_eq!(map.len(), expected.len(), "并发保存后每条设置都在：{map:?}");
        for (idx, key) in expected.iter().enumerate() {
            assert_eq!(map.get(key), Some(&(128_000 + idx as u64)), "条目 {key} 值不符");
        }
        assert_eq!(trae_list(&dir).len(), trae_ids.len().min(20));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn efforts_ref() -> Vec<&'static str> {
        super::super::efforts::TRAE_MAX_MODE_REF.to_vec()
    }

    #[test]
    fn set_visible_despite_late_refill() {
        // 热路径 lookup 在 load 里读到旧表后被挂起，写者此时完成 set 并 invalidate，
        // 读者随后回填旧快照 → 之后 TTL 内 lookup 仍读到旧值。set 返回后任何
        // lookup 必须是新值（无手动失效）
        use std::sync::mpsc;
        let dir = tmp_dir("late_refill");
        qoder_set(&dir, "glm-5.3", Some(128_000)).unwrap();
        let (loaded_tx, loaded_rx) = mpsc::channel::<()>();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let d = dir.clone();
        let reader = std::thread::spawn(move || {
            // 与 qoder_list() 同一缓存键 + 同一加载器，仅在读库后插入挂起点
            super::super::config_cache::get_or_load(&d, QODER_KV_KEY, || {
                let old: BTreeMap<String, u64> = load_uncached(&d, QODER_KV_KEY);
                loaded_tx.send(()).unwrap();
                go_rx.recv().unwrap();
                old
            })
            .get("glm-5.3")
            .copied()
        });
        loaded_rx.recv().unwrap();
        assert_eq!(qoder_set(&dir, "glm-5.3", Some(1_000_000)).unwrap(), Some(1_000_000));
        go_tx.send(()).unwrap();
        assert_eq!(reader.join().unwrap(), Some(128_000), "挂起的读返回其读到的旧表");
        assert_eq!(qoder_target(&dir, "glm-5.3"), Some(1_000_000), "set 返回后不得再读到旧值");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
