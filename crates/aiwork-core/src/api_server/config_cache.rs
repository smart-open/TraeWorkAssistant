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
//!   （配置文档均为 KB 级小文档，反序列化开销远低于一次 SQLite 读）。

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

fn registry() -> &'static Mutex<HashMap<(PathBuf, &'static str), Entry>> {
    static REG: OnceLock<Mutex<HashMap<(PathBuf, &'static str), Entry>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 取缓存或加载（miss/过期时调用 `load` 并回填；load 在注册表锁外执行，不放大临界区）
pub fn get_or_load<T>(data_dir: &Path, key: &'static str, load: impl FnOnce() -> T) -> T
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    let cache_key = (data_dir.to_path_buf(), key);
    {
        let reg = registry().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = reg.get(&cache_key) {
            if e.at.elapsed() < TTL {
                if let Ok(v) = serde_json::from_value::<T>((*e.val).clone()) {
                    return v;
                }
                // 类型不匹配（理论不可达）：视作脏条目，走下方重载
            }
        }
    }
    let v = load();
    if let Ok(json) = serde_json::to_value(&v) {
        let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
        reg.insert(cache_key, Entry { val: std::sync::Arc::new(json), at: Instant::now() });
    }
    v
}

/// 写路径显式失效（保存函数写库后调用，配置改动即时生效）
pub fn invalidate(data_dir: &Path, key: &'static str) {
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    reg.remove(&(data_dir.to_path_buf(), key));
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
}
