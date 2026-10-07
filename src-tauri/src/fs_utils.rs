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

// （SQLite 化 P3：read_json_cached 解析缓存随 6 个热路径文件迁入 store 后删除；
//  write_json 的 evict 调用一并移除）

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

/// 文本原子写（同 write_json 的 temp+rename 模型；供 machineid / storage.json 等
/// 非 JSON 序列化路径复用，防断电产生半截指纹文件）。
pub fn write_text_atomic(path: &Path, content: &str) -> Result<(), String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp = path.with_extension(format!("tmp.{}.{}", std::process::id(), nanos));
    {
        let mut f = fs::File::create(&tmp).map_err(|e| format!("创建临时文件失败: {e}"))?;
        f.write_all(content.as_bytes())
            .map_err(|e| format!("写入失败: {e}"))?;
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

/// 凭证级掩码（审查 P1）：无论长度一律不返回原文——短 token 也全掩码。
/// 用于 JWT / sessionid / sid_guard / ttwid 等等同密码的字段的列表展示。
pub fn mask_secret(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= 8 {
        return "****".to_string();
    }
    let head: String = chars.iter().take(4).collect();
    let tail: String = chars.iter().skip(chars.len() - 4).collect();
    format!("{}****{}", head, tail)
}

/// user_id / 账号 id / 快照槽位作文件系统路径段时的安全校验（审查 P0-1 全仓统一入口）。
/// 只做字符集白名单（字母数字 - _ .），杜绝 `..`、绝对路径、分隔符注入导致的目录逃逸；
/// 池内存在性校验由各调用方按各自账号池补充（wb_chat_uid_guard 模式）。
/// 注：`.` 用于快照槽位轮转后缀（<slot>.bak / <slot>.bak2，switcher/copy.rs），
/// `..` 与裸 `.` 仍被显式拒绝——后者 join 后退化为目录本身，而 profile_delete 对
/// 校验后的槽位整目录删除（remove_dir_all），放行裸 `.` 等于放行整目录清除。
pub fn ensure_uid_safe(uid: &str) -> Result<(), String> {
    if uid.is_empty()
        || uid.len() > 64
        || !uid
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        || uid.contains("..")
        || uid == "."
    {
        return Err(format!("非法账号标识: {}", &uid.chars().take(24).collect::<String>()));
    }
    Ok(())
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

/// app.log 大小轮转阈值（10MB）：超过即重命名为 app.log.old（覆盖旧档）。
/// trim_logs 仅启动期按保留天数清理，长驻进程需靠此防止热路径日志无限增长
const APP_LOG_ROTATE_BYTES: u64 = 10 * 1024 * 1024;

/// 追加一行到 data_dir/logs/app.log，用于托盘/通知等关键路径排查。
/// 超过 APP_LOG_ROTATE_BYTES 时先轮转为 app.log.old 再追加。
pub fn app_log(data_dir: &Path, msg: &str) {
    let log_path = data_dir.join("logs").join("app.log");
    if let Some(parent) = log_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(meta) = fs::metadata(&log_path) {
        if meta.len() > APP_LOG_ROTATE_BYTES {
            let old = data_dir.join("logs").join("app.log.old");
            let _ = fs::rename(&log_path, &old);
        }
    }
    if let Ok(mut f) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        // 先整行格式化再一次 write_all：writeln! 对多片段格式串会拆成多次
        // write 系统调用，多线程/多进程并发追加时行中交错（2026-10-04 实测
        // 出现两行前缀互相嵌入的串行错乱行）；单次 write_all 同行不拆分
        let line = format!("[{}] {}\n", now_ts(), msg);
        let _ = f.write_all(line.as_bytes());
    }
}

// ── F-49：响应宽容解析（信封解包 + 递归键查找）────────────────────────────
// 官方接口字段可能被 data/result/resp/response 等包裹键任意一层包裹
// （参考 oss-ecosystem-research.md §5.5 dig() 规范），解析层统一采用以抗字段变动。
// 语义：对 keys 逐个尝试；每个键先在当前层查找，未命中则沿包裹键逐层下钻
//（数组元素同层展开），限深 8 层防止病态响应拖垮解析。

// "auth"/"account"：CodeBuddy 桌面端 auth 文件为嵌套结构（token/到期在 .auth.*，
// 账号信息在 .account.*，实测 2026-09 结构 {account, accounts, allAccounts, auth}），
// 追加这两个包裹键后 dig() 为原 workbuddy 本地实现的超集，语义安全。
const ENVELOPE_KEYS: [&str; 7] = ["data", "result", "resp", "response", "info", "auth", "account"];
const DIG_MAX_DEPTH: usize = 8;

/// 在 `v` 中按顺序查找 keys 中的任一键，返回第一个命中值。
/// **语义红线**：keys 是「同义键名候选」（任一命中即可），不是路径。
/// 需要固定嵌套层级取值时用 [`path`]，否则只会返回第一个命中的**容器对象**
/// （如 `dig(&body, &["ResponseMetadata", "Error", "Code"])` 命中的是
/// `ResponseMetadata` 对象本身，「信封错误码」判定会恒不成立）。
pub fn dig<'a>(v: &'a serde_json::Value, keys: &[&str]) -> Option<&'a serde_json::Value> {
    keys.iter().find_map(|k| dig_key(v, k, 0))
}

/// 严格路径取值：按 `keys` 逐段下钻（不做全树搜索、不沿包裹键跳层），
/// 任一段缺失即返回 None。用于「确有固定嵌套层级」的字段
/// （如火山信封 `ResponseMetadata.Error.Code`）。
pub fn path<'a>(v: &'a serde_json::Value, keys: &[&str]) -> Option<&'a serde_json::Value> {
    let mut cur = v;
    for k in keys {
        cur = cur.get(*k)?;
    }
    Some(cur)
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

    /// 账号 id / 快照槽位白名单（`-` `_` `.`）：
    /// `.bak`/`.bak2` 为快照轮转槽位合法形态（此前误拒导致快照无法删除）
    #[test]
    fn uid_safe_allows_snapshot_bak_suffix() {
        assert!(ensure_uid_safe("qd-443681d83879").is_ok());
        assert!(ensure_uid_safe("qd-443681d83879.bak").is_ok());
        assert!(ensure_uid_safe("qd-443681d83879.bak2").is_ok());
    }

    /// 路径穿越/分隔符/空串/裸点仍一律拒绝（`..` 防目录逃逸；裸 `.` join 后退化
    /// 为目录本身，防 profile_delete 类整目录删除误清快照根）
    #[test]
    fn uid_safe_rejects_traversal_and_separators() {
        assert!(ensure_uid_safe("").is_err());
        assert!(ensure_uid_safe("..").is_err());
        assert!(ensure_uid_safe(".").is_err());
        assert!(ensure_uid_safe("a..b").is_err());
        assert!(ensure_uid_safe("a/b").is_err());
        assert!(ensure_uid_safe(r"a\b").is_err());
        assert!(ensure_uid_safe("qd-443681d83879.bak/../../etc").is_err());
    }

    /// 定级回归（2026-10-06）：`path` 逐段严格下钻，`dig` 是「候选键名」全树查找。
    /// 路径式误用 dig 只会命中第一个键所在的容器对象（字符串化得空串）——
    /// 火山信封判定曾因此整段失效，此处以单测锁定两者语义差异。
    #[test]
    fn path_walks_literally_while_dig_searches_candidate_keys() {
        let v = serde_json::json!({
            "ResponseMetadata": {"Error": {"Code": "20101", "Message": "refresh token is invalid"}}
        });
        assert_eq!(
            path(&v, &["ResponseMetadata", "Error", "Code"]).and_then(|x| x.as_str()),
            Some("20101")
        );
        // dig 命中 ResponseMetadata 对象本身 → as_str() 为 None（非字符串）
        assert!(dig(&v, &["ResponseMetadata", "Error", "Code"]).and_then(|x| x.as_str()).is_none());
        assert!(dig(&v, &["ResponseMetadata"]).map(|x| x.is_object()).unwrap_or(false));
        // 路径首段不存在时不沿包裹键跳层（严格语义）
        assert!(path(&v, &["Error", "Code"]).is_none());
        // 数字 code 经 path 取值同样可用
        let n = serde_json::json!({"ResponseMetadata": {"Error": {"Code": 20403}}});
        assert_eq!(
            path(&n, &["ResponseMetadata", "Error", "Code"]).and_then(|x| x.as_i64()),
            Some(20403)
        );
    }
}

// （read_json_cached 随 SQLite 化 P3 迁入 store 后删除，缓存契约测试一并移除）
