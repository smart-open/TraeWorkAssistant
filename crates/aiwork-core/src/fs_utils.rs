//! 文件读写工具：原子替换 + 容错加载 + 时间辅助。
use std::fs;
use std::io::Write;
use std::path::Path;

/// 读取 JSON，文件不存在或解析失败返回默认值。
/// UTF-8 BOM 容错（审查-兼容）：Windows 记事本等编辑器写入 BOM 会让 serde 解析
/// 失败 → 此前静默回退默认值，后续写回会覆盖用户配置；解析失败时记录文件名。
pub fn read_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    match fs::read_to_string(path) {
        Ok(mut s) if !s.trim().is_empty() => {
            if s.starts_with('\u{feff}') {
                s.remove(0);
            }
            serde_json::from_str(&s).unwrap_or_else(|e| {
                eprintln!(
                    "[fs_utils] 配置解析失败（已回退默认值，注意保存会覆盖原文件）: {} : {e}",
                    path.display()
                );
                T::default()
            })
        }
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

/// user_id / 账号 id 作文件系统路径段时的安全校验（审查 P0-1 全仓统一入口）。
/// 只做字符集白名单（字母数字 - _），杜绝 `..`、绝对路径、分隔符注入导致的目录逃逸；
/// 池内存在性校验由各调用方按各自账号池补充（wb_chat_uid_guard 模式）。
pub fn ensure_uid_safe(uid: &str) -> Result<(), String> {
    if uid.is_empty()
        || uid.len() > 64
        || !uid
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        || uid.contains("..")
    {
        return Err(format!("非法账号标识: {}", &uid.chars().take(24).collect::<String>()));
    }
    Ok(())
}

pub fn now_iso() -> String {
    // B1（审查 2026-10-13 移植）：补时区偏移（%:z）——此前输出的本地裸串无偏移，
    // DateTime::parse_from_rfc3339 解析恒失败，消费端 credits_fetched_at 恒 None，
    // reserve 过滤与 24h 陈旧判定整体失效（fail-open）。带偏移后 rfc3339 直接可解析；
    // 存量裸串由 parse_ts_loose 二级兜底覆盖。
    chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string()
}

pub fn now_ts() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// 宽松时间解析到 Unix 秒（`credits_fetched_at` 等时间戳字段的唯一入口）。
///
/// 背景（issue #91 审查）：本机生产的时间戳串多为 `chrono::Local` 裸串
/// （`now_iso()` → `2026-09-15T10:00:00`、`now_ts()` → `2026-09-15 10:00:00`），
/// **不带时区偏移**，而 `DateTime::parse_from_rfc3339` 强制要求偏移 → 恒失败。
/// 此前 `api_server/runtime.rs` 只走 rfc3339 一臂，导致 reserve 过滤与 24h 陈旧
/// 判定在生产环境结构性失效（单测直接构造 `Some(i64)` 掩盖了该问题）。
///
/// 解析顺序（由严到宽，B1 移植 2026-10-13）：纯数字时间戳（秒/毫秒 ≥1e12）→
/// rfc3339（`Z`/`/` 预处理归一）→ 带小数秒/无小数秒的 naive 串（按本机
/// Local 时区解释，与写入侧 `chrono::Local` 同口径）→ 纯日期（当天 23:59:59）。
/// 无法识别返回 None（调用方按「无时间信息」fail-open 处理）。
pub fn parse_ts_loose(s: &str) -> Option<i64> {
    use chrono::TimeZone;
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // 纯数字：按时间戳秒/毫秒解释（毫秒 ≥1e12）。
    // 下界过滤（第五轮审查修复）：全 0/极短数字串（字段漂移回 "0"）此前解析为
    // 1970 年 → 恒被判陈旧 → 兜底轮反复触发；2001-09 之前视为非法回 None
    if s.chars().all(|c| c.is_ascii_digit()) {
        let n: f64 = s.parse().ok()?;
        return if n >= 1e12 {
            Some((n / 1000.0) as i64)
        } else if n > 1_000_000_000.0 {
            Some(n as i64)
        } else {
            None
        };
    }
    let s = s.replace('Z', "+00:00").replace('/', "-");
    if let Ok(d) = chrono::DateTime::parse_from_rfc3339(&s) {
        return Some(d.timestamp());
    }
    for fmt in [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(d) = chrono::NaiveDateTime::parse_from_str(&s, fmt) {
            return chrono::Local.from_local_datetime(&d).single().map(|t| t.timestamp());
        }
    }
    // 纯日期：按当天 23:59:59 计（避免凌晨 0 点把当日快照误判为「已过期」）
    if let Ok(d) = chrono::NaiveDate::parse_from_str(&s, "%Y-%m-%d") {
        return chrono::Local
            .from_local_datetime(&d.and_hms_opt(23, 59, 59)?)
            .single()
            .map(|t| t.timestamp());
    }
    None
}

/// 业务日期（YYYY-MM-DD）：日期串生产与消费的**唯一**入口。
///
/// 此前 `usage.rs::today_key()`、`today_prefix()`、`qoder_sessions_compact()`、
/// `qoder_sessions_prune()` 四处各自 `chrono::Local::now().date_naive()`，
/// 任一侧漂移都会让「当日/保留窗」边界与已落库行的 date 键错开一天。
///
/// 时区说明：以本机 Local 为准（与已落库的历史 date 键同口径，升级不断裂）。
/// docker 部署若需与桌面（默认 UTC+8）保持同一日界，请在容器设置 `TZ=Asia/Shanghai`。
pub fn today_key() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// 业务日期（chrono 形态，`today_key()` 的同源版本，供需要日期算术的调用方使用）
pub fn business_today() -> chrono::NaiveDate {
    chrono::Local::now().date_naive()
}

pub fn today_prefix() -> String {
    today_key()
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
        let _ = writeln!(f, "[{}] {}", now_ts(), msg);
    }
}

/// 无 data_dir 上下文的全局日志兜底（issue #224 移植：panic hook / 启动早期用）：
/// `AIWORK_DATA_DIR` 优先（与 AppState::new 同源），未设置时回落默认根目录
///（DEFAULT_DATA_ROOT/DATA_DIR_NAME，Windows 按进程当前盘符解析）。目录不可写
/// 静默放弃——panic hook 内绝不能再 panic（写入失败吞错，不产生二次 panic）
pub fn app_log_default(msg: &str) {
    let root = std::env::var("AIWORK_DATA_DIR")
        .ok()
        .map(|v| std::path::PathBuf::from(v.trim()))
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| {
            std::path::PathBuf::from(crate::state::DEFAULT_DATA_ROOT)
                .join(crate::state::DATA_DIR_NAME)
        });
    app_log(&root, msg);
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

// （read_json_cached 随 SQLite 化 P3 迁入 store 后删除，缓存契约测试一并移除）

#[cfg(test)]
mod fs_path_tests {
    use super::*;

    /// 定级回归（2026-10-06 移植）：`path` 逐段严格下钻，`dig` 是「候选键名」全树查找。
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

    /// issue #91 回归：`credits_fetched_at` 链路一度只走严格 rfc3339 解析，而本机
    /// 生产的偏偏是 `now_iso()`/`now_ts()` 的**无偏移本地裸串** → 恒 None →
    /// reserve 保留额过滤与 24h 陈旧判定在生产环境结构性失效（单测直接构造
    /// `Some(i64)` 掩盖）。此处锁定「本机写入 → 能解析回时间戳」这条真实链路。
    #[test]
    fn parse_ts_loose_accepts_locally_produced_naive_strings() {
        // now_iso(): 2026-09-15T10:00:00（无偏移）——rfc3339 解析必失败的那一种
        assert!(chrono::DateTime::parse_from_rfc3339("2026-09-15T10:00:00").is_err());
        assert_eq!(
            parse_ts_loose("2026-09-15T10:00:00"),
            parse_ts_loose("2026-09-15 10:00:00"),
            "两种分隔符应等价"
        );
        // now_ts(): 空格分隔裸串
        assert!(parse_ts_loose("2026-09-15 10:00:00").is_some());
        // 往返：本机 now_iso()/now_ts() 产出必须可解析（正是生产端的形态）
        assert!(parse_ts_loose(&now_iso()).is_some(), "now_iso() 必须可解析");
        assert!(parse_ts_loose(&now_ts()).is_some(), "now_ts() 必须可解析");
        // 带偏移 / Z 结尾仍走严格臂
        let with_z = parse_ts_loose("2026-09-15T10:00:00Z");
        let with_off = parse_ts_loose("2026-09-15T10:00:00+08:00");
        assert!(with_z.is_some() && with_off.is_some());
        assert_eq!(with_z.unwrap() - with_off.unwrap(), 8 * 3600);
        // 小数秒
        assert!(parse_ts_loose("2026-09-15T10:00:00.500").is_some());
        // B1 移植回归：纯数字 Unix 秒/毫秒（2026-09-15T00:00:00Z = 1789440000）
        assert_eq!(parse_ts_loose("1789440000"), Some(1789440000));
        assert_eq!(parse_ts_loose("1789440000000"), Some(1789440000));
        // 纯日期回退当日 23:59:59（不得被判为 Nil）
        assert!(parse_ts_loose("2026-09-15").is_some());
        // 非法输入 fail-open：None（不得 panic）
        for bad in ["", "   ", "not-a-time", "2026/09/15 10:00"] {
            assert!(parse_ts_loose(bad).is_none() || !bad.trim().is_empty());
        }
        assert_eq!(parse_ts_loose(""), None);
        assert_eq!(parse_ts_loose("not-a-time"), None);
        // 前后空白容忍
        assert!(parse_ts_loose("  2026-09-15 10:00:00  ").is_some());
    }
}
