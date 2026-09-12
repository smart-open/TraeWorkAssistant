//! 文件读写工具：原子替换 + 容错加载 + 时间辅助。
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// 读取 JSON，文件不存在或解析失败返回默认值。
pub fn read_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    match fs::read_to_string(path) {
        Ok(s) if !s.trim().is_empty() => serde_json::from_str(&s).unwrap_or_default(),
        _ => T::default(),
    }
}

// ── mtime 校验的 JSON 解析缓存（调度热路径磁盘读优化）────────────────────────
// resolve_target 每请求读约 4 份小 JSON（dispatch_policy / wb_model_route /
// wb_model_catalog / api_models），重复读盘 + 解析开销大。
// 策略：mtime + size 均未变化 → 直接克隆缓存中的已解析值（免 IO 免解析）；
// write_json 写成功后逐出对应条目（应用内写入立即生效），外部编辑靠 mtime/size 变化兜底。

type JsonCacheVal = Box<dyn std::any::Any + Send + Sync>;
type JsonCache = std::collections::HashMap<PathBuf, (std::time::SystemTime, u64, JsonCacheVal)>;

fn json_cache() -> &'static std::sync::Mutex<JsonCache> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<JsonCache>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// 带解析缓存的 JSON 读取：文件缺失 / 为空 / 解析失败返回 None，兜底语义由调用方决定。
/// 供只读热路径使用（调度分流、目录聚合等）；命中时克隆已解析值——
/// 小结构克隆成本远低于磁盘 IO + 解析。同一路径请保持请求类型一致（缓存按类型下溯）。
pub fn read_json_cached<T>(path: &Path) -> Option<T>
where
    T: serde::de::DeserializeOwned + Clone + Send + Sync + 'static,
{
    let meta = fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    let len = meta.len();
    {
        let map = json_cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some((m, l, val)) = map.get(path) {
            if *m == mtime && *l == len {
                if let Some(v) = val.downcast_ref::<T>() {
                    return Some(v.clone());
                }
            }
        }
    }
    let text = fs::read_to_string(path).ok()?;
    if text.trim().is_empty() {
        return None;
    }
    let value: T = serde_json::from_str(&text).ok()?;
    json_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(path.to_path_buf(), (mtime, len, Box::new(value.clone())));
    Some(value)
}

/// 逐出路径对应的缓存条目（write_json 成功后调用）
fn evict_json_cache(path: &Path) {
    json_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(path);
}

/// 原子写：先写临时文件再 rename，避免断电损坏。
/// 临时文件名带 pid+纳秒后缀：并发写者（如模型列表的读取自愈与官网同步）互不踩踏。
pub fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp = path.with_extension(format!("tmp.{}.{}", std::process::id(), nanos));
    {
        let mut f = fs::File::create(&tmp).map_err(|e| format!("创建临时文件失败: {e}"))?;
        let buf = serde_json::to_vec_pretty(value).map_err(|e| format!("序列化失败: {e}"))?;
        f.write_all(&buf).map_err(|e| format!("写入失败: {e}"))?;
        f.flush().map_err(|e| format!("刷新失败: {e}"))?;
    }
    fs::rename(&tmp, path).map_err(|e| format!("替换文件失败: {e}"))?;
    evict_json_cache(path);
    Ok(())
}

/// 掩码：保留前4后4，中间用 … 代替。
pub fn mask(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= 8 {
        return s.to_string();
    }
    let head: String = chars.iter().take(4).collect();
    let tail: String = chars.iter().skip(chars.len() - 4).collect();
    format!("{}…{}", head, tail)
}

pub fn now_iso() -> String {
    chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string()
}

pub fn now_ts() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

pub fn today_prefix() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// 按保留天数清理日志文件（proxy / checkin / switcher / app）。
/// 仅丢弃带 `[YYYY-MM-DD` 前缀且日期早于 cutoff 的行；无日期前缀的行（如部分外部脚本输出）一律保留。
/// 任何错误静默忽略——日志清理失败不应影响主流程。
pub fn trim_logs(data_dir: &Path, retention_days: u64) {
    if retention_days == 0 {
        return;
    }
    let cutoff = chrono::Local::now().date_naive() - chrono::Duration::days(retention_days as i64);
    let logs_dir = data_dir.join("logs");
    for name in ["proxy.log", "checkin.log", "switcher.log", "app.log"] {
        let p = logs_dir.join(name);
        let content = match fs::read(&p) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
            Err(_) => continue,
        };
        let mut kept: Vec<&str> = Vec::new();
        for line in content.lines() {
            let date_part = line.strip_prefix('[').and_then(|s| s.get(..10));
            let drop = if let Some(d) = date_part {
                chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d")
                    .map(|parsed| parsed < cutoff)
                    .unwrap_or(false)
            } else {
                false
            };
            if !drop {
                kept.push(line);
            }
        }
        let new_content = kept.join("\n");
        if new_content != content {
            let _ = fs::write(&p, new_content);
        }
    }
}

/// 追加一行到 data_dir/logs/app.log，用于托盘/通知等关键路径排查。
pub fn app_log(data_dir: &Path, msg: &str) {
    let log_path = data_dir.join("logs").join("app.log");
    if let Some(parent) = log_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut f) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        let _ = writeln!(f, "[{}] {}", now_ts(), msg);
    }
}

// ── F-49：响应宽容解析（信封解包 + 递归键查找）────────────────────────────
// 官方接口字段可能被 data/result/resp/response 等包裹键任意一层包裹
// （参考 oss-ecosystem-research.md §5.5 dig() 规范），解析层统一采用以抗字段变动。
// 语义：对 keys 逐个尝试；每个键先在当前层查找，未命中则沿包裹键逐层下钻
//（数组元素同层展开），限深 8 层防止病态响应拖垮解析。

const ENVELOPE_KEYS: [&str; 5] = ["data", "result", "resp", "response", "info"];
const DIG_MAX_DEPTH: usize = 8;

/// 在 `v` 中按顺序查找 keys 中的任一键，返回第一个命中值。
pub fn dig<'a>(v: &'a serde_json::Value, keys: &[&str]) -> Option<&'a serde_json::Value> {
    keys.iter().find_map(|k| dig_key(v, k, 0))
}

fn dig_key<'a>(v: &'a serde_json::Value, key: &str, depth: usize) -> Option<&'a serde_json::Value> {
    if depth > DIG_MAX_DEPTH {
        return None;
    }
    match v {
        serde_json::Value::Object(map) => {
            if let Some(hit) = map.get(key) {
                return Some(hit);
            }
            // 沿信封包裹键下钻
            ENVELOPE_KEYS
                .iter()
                .find_map(|wk| map.get(*wk).and_then(|child| dig_key(child, key, depth + 1)))
        }
        // 列表包裹：同层展开各元素查找
        serde_json::Value::Array(arr) => arr
            .iter()
            .find_map(|item| dig_key(item, key, depth + 1)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 解析缓存契约：读取缓存生效；write_json 逐出后立即可见新值；损坏/缺失返回 None
    #[test]
    fn cached_read_reflects_writes_and_missing_files() {
        let dir = std::env::temp_dir().join(format!("twa_fscache_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("cache_probe.json");
        #[derive(serde::Deserialize, Clone, PartialEq, Debug)]
        struct V {
            v: i32,
        }
        std::fs::write(&p, r#"{"v":1}"#).unwrap();
        assert_eq!(read_json_cached::<V>(&p), Some(V { v: 1 }));
        // 应用内写入（write_json）逐出缓存 → 立即读到新值
        write_json(&p, &serde_json::json!({"v": 2})).unwrap();
        assert_eq!(read_json_cached::<V>(&p), Some(V { v: 2 }));
        // 文件损坏 → None（不缓存毒值，调用方走自愈/兜底）
        std::fs::write(&p, "not-json").unwrap();
        assert_eq!(read_json_cached::<V>(&p), None);
        // 文件缺失 → None
        std::fs::remove_file(&p).unwrap();
        assert_eq!(read_json_cached::<V>(&p), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
