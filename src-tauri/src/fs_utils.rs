//! 文件读写工具：原子替换 + 容错加载 + 时间辅助。
use std::fs;
use std::io::Write;
use std::path::Path;

/// 读取 JSON，文件不存在或解析失败返回默认值。
pub fn read_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    match fs::read_to_string(path) {
        Ok(s) if !s.trim().is_empty() => serde_json::from_str(&s).unwrap_or_default(),
        _ => T::default(),
    }
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

/// 按保留天数清理日志文件（proxy / checkin / switcher）。
/// 仅丢弃带 `[YYYY-MM-DD` 前缀且日期早于 cutoff 的行；无日期前缀的行（如部分外部脚本输出）一律保留。
/// 任何错误静默忽略——日志清理失败不应影响主流程。
pub fn trim_logs(data_dir: &Path, retention_days: u64) {
    if retention_days == 0 {
        return;
    }
    let cutoff = chrono::Local::now().date_naive() - chrono::Duration::days(retention_days as i64);
    let logs_dir = data_dir.join("logs");
    for name in ["proxy.log", "checkin.log", "switcher.log"] {
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
