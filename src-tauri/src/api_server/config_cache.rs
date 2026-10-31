//! 调度配置热路径内存缓存（网关性能批次 A）。
//!
//! 背景：`dispatch::resolve_target` 每请求串行读 5 类配置
//! （custom_models / wb_model_route / wb_catalog / api_models / dispatch_policy），
//! 原实现全部同步读 SQLite 且共用 store 层单 `Mutex<Connection>`——
//! 与用量落盘、鉴权记账写事务争锁时直接阻塞 axum 的 async worker 线程。
//!
//! 策略：
//! - **写路径显式失效**：本进程内的保存函数（save_list / save_policy / fetch_and_replace 等）
//!   写库后调用 `invalidate`，配置改动即时生效；
//! - **TTL 兜底**：5s 短 TTL 覆盖「本进程外」的写入（CLI `--task-run` 子进程、手工改库），
//!   最迟 5s 可见；
//! - 缓存值以 `Arc<serde_json::Value>` 快照存放，命中时 `from_value` 反序列化
//!   （配置文档均为 KB 级小文档，反序列化开销远低于一次 SQLite 读）；
//! - **代际校验回填**：load 在注册表锁外执行，miss 时先记下该键代际，回填前校验
//!   代际未变。`invalidate` 递增代际——load 期间若有写路径完成「写库 + 失效」，
//!   这次（可能读到旧表的）迟到回填被丢弃，不会在失效之后把旧快照重新挂回缓存
//!   并续上一整个 TTL。被丢弃的那次调用本身仍返回其读到的值（与写并发，
//!   线性化在写之前），失效之后发起的读一律重新读库。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 配置缓存 TTL：本进程内写路径已显式失效（即时生效），TTL 仅兜底进程外写入
pub const TTL: Duration = Duration::from_secs(5);

struct Entry {
    val: std::sync::Arc<serde_json::Value>,
    at: Instant,
}

/// 单键槽位：失效只清 `entry`、不删槽位——代际须跨失效保留，迟到回填才校验得出
/// （键为静态名 × data_dir，槽位数有界）
#[derive(Default)]
struct Slot {
    gen: u64,
    entry: Option<Entry>,
}

type Registry = HashMap<(PathBuf, &'static str), Slot>;

fn registry() -> &'static Mutex<Registry> {
    static REG: OnceLock<Mutex<Registry>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 未过期命中 → `Ok(v)`；否则 `Err((代际快照, 过期旧快照))`，供锁外 load 后校验回填
fn probe<T: serde::de::DeserializeOwned>(
    cache_key: &(PathBuf, &'static str),
) -> Result<T, (u64, Option<std::sync::Arc<serde_json::Value>>)> {
    let reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    let Some(slot) = reg.get(cache_key) else {
        return Err((0, None));
    };
    let mut stale = None;
    if let Some(e) = &slot.entry {
        if e.at.elapsed() < TTL {
            if let Ok(v) = serde_json::from_value::<T>((*e.val).clone()) {
                return Ok(v);
            }
            // 类型不匹配（理论不可达）：视作脏条目，走重载
        } else {
            stale = Some(e.val.clone());
        }
    }
    Err((slot.gen, stale))
}

/// 回填：仅当代际仍等于 miss 时的快照才写入（期间发生过失效 → 丢弃迟到回填）
fn refill<T: serde::Serialize>(cache_key: (PathBuf, &'static str), gen: u64, v: &T) {
    let Ok(json) = serde_json::to_value(v) else {
        return;
    };
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    let slot = reg.entry(cache_key).or_default();
    if slot.gen == gen {
        slot.entry = Some(Entry { val: std::sync::Arc::new(json), at: Instant::now() });
    }
}

/// 取缓存或加载（miss/过期时调用 `load` 并回填；load 在注册表锁外执行，不放大临界区；
/// 回填经代际校验，见模块文档）
pub fn get_or_load<T>(data_dir: &Path, key: &'static str, load: impl FnOnce() -> T) -> T
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    let cache_key = (data_dir.to_path_buf(), key);
    let gen = match probe::<T>(&cache_key) {
        Ok(v) => return v,
        Err((gen, _)) => gen,
    };
    let v = load();
    refill(cache_key, gen, &v);
    v
}

/// 取缓存或加载（可失败版，org_index 等整库读场景专用）：
/// - 未过期缓存命中 → 直接返回；
/// - miss/过期 → 调用 `load`；成功则回填缓存并返回；
/// - `load` 失败（如瞬时读库失败）→ **不缓存失败结果**，若注册表中存在旧快照
///   （即使已过期）则沿用旧快照兜底并记日志——避免一次瞬时读库失败把空索引
///   回填缓存、导致全库账号在 TTL 窗口内不可取号；
/// - 首启无任何快照才调用 `fallback`（调用方通常回退空索引）。
pub fn get_or_load_fallible<T, E>(
    data_dir: &Path,
    key: &'static str,
    load: impl FnOnce() -> Result<T, E>,
    fallback: impl FnOnce() -> T,
) -> T
where
    T: serde::Serialize + serde::de::DeserializeOwned,
    E: std::fmt::Display,
{
    let cache_key = (data_dir.to_path_buf(), key);
    // 未过期命中直接返回；过期条目暂存（快照引用），load 失败时兜底用
    let (gen, stale) = match probe::<T>(&cache_key) {
        Ok(v) => return v,
        Err(miss) => miss,
    };
    match load() {
        Ok(v) => {
            refill(cache_key, gen, &v);
            v
        }
        Err(err) => {
            eprintln!("[config_cache] {key} 重载失败（沿用旧快照兜底）: {err}");
            if let Some(json) = stale {
                if let Ok(v) = serde_json::from_value::<T>((*json).clone()) {
                    return v;
                }
            }
            fallback()
        }
    }
}

/// 写路径显式失效（保存函数写库后调用，配置改动即时生效）：清快照并递增代际，
/// 失效前已开始、尚未回填的 load 结果随之作废
pub fn invalidate(data_dir: &Path, key: &'static str) {
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    let slot = reg.entry((data_dir.to_path_buf(), key)).or_default();
    slot.gen = slot.gen.wrapping_add(1);
    slot.entry = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 命中缓存_失效后重载() {
        let dir = std::env::temp_dir().join(format!("cfgcache_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("data")).unwrap();
        let mut n = 0;
        let v1 = get_or_load(&dir, "t1", || { n += 1; n });
        let v2 = get_or_load(&dir, "t1", || { n += 1; n });
        assert_eq!((v1, v2), (1, 1), "TTL 内命中缓存不重载");
        invalidate(&dir, "t1");
        let v3 = get_or_load(&dir, "t1", || { n += 1; n });
        assert_eq!(v3, 2, "失效后重载");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 读线程卡在 load 里（已读到旧值），写者此刻完成「写库 + invalidate」，
    /// 再放读线程回填：迟到的旧快照不得挂回缓存
    fn late_refill_after_invalidate(fallible: bool) {
        use std::sync::mpsc;
        let dir = std::env::temp_dir().join(format!("cfgcache_late_{fallible}_{}", std::process::id()));
        let key: &'static str = if fallible { "t_late_fallible" } else { "t_late" };
        let (loaded_tx, loaded_rx) = mpsc::channel::<()>();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let d = dir.clone();
        let reader = std::thread::spawn(move || {
            let slow_load = || {
                let old = 1u32; // load 已读到旧表
                loaded_tx.send(()).unwrap();
                go_rx.recv().unwrap();
                old
            };
            if fallible {
                get_or_load_fallible(&d, key, || Ok::<_, String>(slow_load()), || 0)
            } else {
                get_or_load(&d, key, slow_load)
            }
        });
        loaded_rx.recv().unwrap();
        invalidate(&dir, key); // 写者：kv_set(新值) 后失效
        go_tx.send(()).unwrap();
        assert_eq!(reader.join().unwrap(), 1, "并发中的读返回其读到的值");
        let v = get_or_load(&dir, key, || 2u32);
        assert_eq!(v, 2, "失效后不得再命中迟到回填的旧快照");
        // 正常回填仍生效（不退化为每次读库）
        assert_eq!(get_or_load(&dir, key, || 3u32), 2);
    }

    #[test]
    fn 失效后迟到回填被丢弃() {
        late_refill_after_invalidate(false);
    }

    #[test]
    fn 失效后迟到回填被丢弃_fallible() {
        late_refill_after_invalidate(true);
    }
}
