use crate::platform::cmd::sys_command;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};

use crate::jwt;
use crate::models::{CreditRecord, DeviceMap, Settings};
use crate::state::AppState;

pub const INVITE_LINK: &str =
    "https://www.trae.cn/work-fission/4CP3KDBT5W9A?utm_source=copy_link&utm_medium=friends_invite";

// ---------------- 设备 ID ----------------

#[tauri::command]
pub fn device_reset(state: State<AppState>, user_id: String) -> Result<(), String> {
    // SQLite 化（P3）：device_map.json → device_map 表
    let store = crate::store::db(&state.data_dir);
    let mut map: DeviceMap = crate::store::docs::device_map_load(&store);
    map.remove(&user_id);
    crate::store::docs::device_map_save(&store, &map)?;
    Ok(())
}

// ---------------- 开机自启（T11，tauri-plugin-autostart：Windows 写注册表 Run 项） ----------------

/// 查询开机自启状态
#[tauri::command]
pub fn autostart_status(app: AppHandle) -> Result<bool, String> {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch().is_enabled().map_err(|e| e.to_string())
}

/// 设置开机自启（即时生效，安装版/便携版均写当前 exe 路径）
#[tauri::command]
pub fn autostart_set(app: AppHandle, enabled: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let autolaunch = app.autolaunch();
    if enabled {
        autolaunch.enable().map_err(|e| e.to_string())
    } else {
        autolaunch.disable().map_err(|e| e.to_string())
    }
}

/// 最小化到托盘（issue #46）：与托盘隐藏同链路，hide 前记录最大化状态，
/// 供托盘/单实例 show 时强制重建最大化（规避无边框窗口 hide/show 后最大化失同步）
#[tauri::command]
pub fn minimize_to_tray(app: AppHandle) -> Result<(), String> {
    crate::hide_main_window(&app);
    Ok(())
}

// ---------------- 代理请求日志 ----------------

#[derive(Serialize, Clone)]
pub struct ProxyLogEntry {
    pub id: String,
    pub timestamp: String,
    pub method: String,
    pub host: String,
    pub path: String,
    pub status: String,
    pub size: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sse_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sse_tokens: Option<String>,
}

#[derive(Serialize)]
pub struct ProxyLogListResult {
    pub entries: Vec<ProxyLogEntry>,
    pub total: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyLogQueryOpts {
    #[serde(default)]
    pub keyword: Option<String>,
    #[serde(default)]
    pub start_time: Option<String>,
    #[serde(default)]
    pub end_time: Option<String>,
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub limit: Option<usize>,
}

fn proxy_log_dir(state: &State<AppState>) -> std::path::PathBuf {
    let settings = state.settings();
    settings
        .proxy_log_path
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| state.logs_dir())
}

/// 解析单条代理日志，提取摘要信息
fn parse_proxy_entry(raw: &str, file_name: &str, index: usize) -> Option<ProxyLogEntry> {
    let lines: Vec<&str> = raw.lines().collect();
    if lines.is_empty() {
        return None;
    }

    // 找到时间戳行: [2024-01-15 10:30:00] METHOD host/path
    // 或: [2024-01-15 10:30:00] [WebSocket Upgrade] host/path
    let header_line = lines.iter().find(|l| l.starts_with('['))?;
    let timestamp = header_line
        .get(1..20)
        .unwrap_or("")
        .to_string();

    let rest = &header_line[header_line.find("] ").map(|i| i + 2).unwrap_or(0)..];

    let (method, host, path, status) = if rest.starts_with("[WebSocket") {
        // WebSocket 条目
        let hp = rest.find("] ").map(|i| &rest[i + 2..]).unwrap_or(rest);
        let (host, path) = split_host_path(hp);
        ("WebSocket".to_string(), host, path, "101 Upgrade".to_string())
    } else {
        // 普通请求
        let parts: Vec<&str> = rest.splitn(2, ' ').collect();
        let raw_method = parts.first().unwrap_or(&"").to_string();
        let hp = parts.get(1).unwrap_or(&"");
        let (host, path) = split_host_path(hp);
        // 从内容中提取状态码
        let status = raw
            .lines()
            .find(|l| l.starts_with("--- Response:"))
            .and_then(|l| {
                l.trim_start_matches("--- Response: ")
                    .trim_end_matches(" ---")
                    .to_string()
                    .into()
            })
            .unwrap_or_else(|| "-".to_string());
        (format!("HTTP {}", raw_method), host, path, status)
    };

    Some(ProxyLogEntry {
        id: format!("{}:{}", file_name, index),
        timestamp,
        method,
        host,
        path,
        status,
        size: raw.len(),
        sse_model: extract_sse_field(raw, "model"),
        sse_tokens: extract_sse_tokens(raw),
    })
}

/// 从 SSE Summary 区块中提取指定字段
fn extract_sse_field(raw: &str, field: &str) -> Option<String> {
    let in_summary = raw.lines().skip_while(|l| !l.starts_with("--- SSE Summary ---"));
    for line in in_summary {
        let line = line.trim();
        if line.starts_with("--- ") && !line.starts_with("--- SSE Summary") {
            break;
        }
        if let Some(rest) = line.strip_prefix(&format!("  {}: ", field)) {
            return Some(rest.to_string());
        }
    }
    None
}

/// 提取 token 用量摘要字符串
fn extract_sse_tokens(raw: &str) -> Option<String> {
    let pt = extract_sse_field(raw, "prompt_tokens")?;
    let ct = extract_sse_field(raw, "completion_tokens").unwrap_or_else(|| "?".to_string());
    let tt = extract_sse_field(raw, "total_tokens").unwrap_or_else(|| "?".to_string());
    Some(format!("p:{} c:{} t:{}", pt, ct, tt))
}

fn split_host_path(hp: &str) -> (String, String) {
    // hp 可能是 "api.trae.cn/trae/api/..." 或 "api.trae.cn"
    if let Some(idx) = hp.find('/') {
        (hp[..idx].to_string(), hp[idx..].to_string())
    } else {
        (hp.to_string(), String::new())
    }
}

// ---------------- 代理抓包日志：条目索引（列表 / 详情的公共底座） ----------------
//
// 性能（2026-10-07，本机实测）：日志目录内 11 个 `proxy_req_*.log` 共 212.7MB（约 130 万行，
// 单文件最大 60MB / 58 万行）。原实现每次列表都把**全部文件**读成 String、逐条 parse 成
// struct（每条还建一次 `Vec<&str>` 与多个 String），最后才 `skip/take` 取当前页 30 条——
// 翻页 / 改日期 / 改关键字都会重来一遍；详情同样为取 1 条读完整文件。改为「条目索引 + 按需读」：
//   ① 每个文件维护条目索引（条目字节区间 + 时间戳），文件追加时只增量扫「最后一条 + 新增尾部」
//      （最后一条上次可能尚未写完），文件被截断 / 轮转则整份重建；
//   ② 时间筛选在内存里按时间戳完成（不读正文）；关键字筛选才需要正文（顺序读一次，不再逐条 parse）；
//   ③ 只为当前页那 30 条读取其字节区间并解析；详情只读该条的区间。
//
// id 口径修正：id 用「文件内**原始**非空分块序号」，与 `proxy_log_detail` 取序号的方式一致——
// 原实现 index 只在通过筛选时才自增，带筛选时列表 id 与详情错位（点开看到的是另一条）。

/// 条目分隔行（80 个 `=`；与写入端 `device_proxy/logger.rs` 一致）
const PROXY_ENTRY_SEP: &[u8] = &[b'='; 80];

/// 条目索引项：字节区间 + 筛选用时间戳。
/// `ts` 用定长数组避免堆分配——条目可达十万级，增量刷新要整表克隆，逐条 String 会成为新瓶颈。
#[derive(Clone, Copy)]
struct ProxyIndexedEntry {
    start: u64,
    end: u64,
    ts: [u8; 19],
    ts_len: u8,
}

impl ProxyIndexedEntry {
    /// 与原实现筛选分支同口径：块**首行**（trim 后）的 `1..20` 字节；行不足 20 字节 → 空串
    fn ts_str(&self) -> &str {
        std::str::from_utf8(&self.ts[..self.ts_len as usize]).unwrap_or("")
    }
}

struct ProxyFileIndex {
    /// 已索引到的字节数（文件继续追加时从这里续扫）
    len: u64,
    mtime_ms: i64,
    entries: Vec<ProxyIndexedEntry>,
}

static PROXY_INDEX_CACHE: OnceLock<Mutex<HashMap<String, Arc<ProxyFileIndex>>>> = OnceLock::new();

fn proxy_index_cache() -> &'static Mutex<HashMap<String, Arc<ProxyFileIndex>>> {
    PROXY_INDEX_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn file_mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 目录内 `proxy_req_*.log`，文件名升序（= 时间升序）。
/// 目录读取失败返回 Err（对齐基线错误语义；吞成空表会连带清空全部索引缓存）
fn proxy_log_files(log_dir: &Path) -> Result<Vec<String>, String> {
    let mut files: Vec<String> = std::fs::read_dir(log_dir)
        .map_err(|e| format!("读取代理日志目录失败: {e}"))?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("proxy_req_") && name.ends_with(".log") {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    files.sort();
    Ok(files)
}

/// 索引缓存清理：磁盘上已不存在的日志文件（轮转 / 删除）不再占内存
fn retain_alive_indexes(log_dir: &Path, files: &[String]) {
    let alive: HashSet<String> = files
        .iter()
        .map(|n| log_dir.join(n).to_string_lossy().to_string())
        .collect();
    let mut cache = proxy_index_cache().lock().unwrap_or_else(|e| e.into_inner());
    cache.retain(|k, _| alive.contains(k));
}

/// 读取 `[from, to)` 字节
fn read_range(path: &Path, from: u64, to: u64) -> Result<Vec<u8>, String> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    if to <= from {
        return Ok(Vec::new());
    }
    let mut f = std::fs::File::open(path).map_err(|e| format!("读取日志文件失败: {e}"))?;
    f.seek(SeekFrom::Start(from))
        .map_err(|e| format!("定位日志文件失败: {e}"))?;
    let mut buf = Vec::with_capacity(((to - from) as usize).min(1 << 26));
    f.take(to - from)
        .read_to_end(&mut buf)
        .map_err(|e| format!("读取日志文件失败: {e}"))?;
    // 短读 = 文件在索引构建/条目定位与本次读取之间被轮转/截断：报错走既有的
    // 「单条/单文件降级」路径，不静默返回截断内容造成摘要与正文错位
    if buf.len() as u64 != to - from {
        return Err(format!(
            "日志内容短于预期（可能已被轮转或截断）: 期望 {} 字节，实际 {} 字节",
            to - from,
            buf.len()
        ));
    }
    Ok(buf)
}

/// 在字节流里找分隔行（等价原实现的 `content.split(SEP)`：行内更长的 `=` 串同样命中子串）
fn find_entry_sep(hay: &[u8]) -> Option<usize> {
    let n = PROXY_ENTRY_SEP.len();
    if hay.len() < n {
        return None;
    }
    let mut i = 0usize;
    while let Some(p) = hay[i..].iter().position(|b| *b == b'=') {
        let pos = i + p;
        if pos + n <= hay.len() && &hay[pos..pos + n] == PROXY_ENTRY_SEP {
            return Some(pos);
        }
        i = pos + 1;
    }
    None
}

/// 块首行（trim 后）的 `1..20` 字节（时间戳）
fn first_line_ts(seg: &[u8]) -> ([u8; 19], u8) {
    let lead = seg
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(seg.len());
    let rest = &seg[lead..];
    let line_end = rest.iter().position(|b| *b == b'\n').unwrap_or(rest.len());
    let line = &rest[..line_end];
    if line.len() >= 20 {
        let mut ts = [0u8; 19];
        ts.copy_from_slice(&line[1..20]);
        return (ts, 19);
    }
    ([0u8; 19], 0)
}

/// 扫描 `[base, base + bytes.len())` 区间，产出**非空块**的索引项
/// （空块跳过，对齐原实现 `chunk.trim()` 判空；序号也因此与 `proxy_log_detail` 一致）
fn scan_index_entries(bytes: &[u8], base: u64) -> Vec<ProxyIndexedEntry> {
    let mut out = Vec::new();
    let mut idx = 0usize;
    loop {
        let rel = find_entry_sep(&bytes[idx..]);
        let seg_end = rel.map(|p| idx + p).unwrap_or(bytes.len());
        let seg = &bytes[idx..seg_end];
        if !seg.iter().all(|b| b.is_ascii_whitespace()) {
            let (ts, ts_len) = first_line_ts(seg);
            out.push(ProxyIndexedEntry {
                start: base + idx as u64,
                end: base + seg_end as u64,
                ts,
                ts_len,
            });
        }
        match rel {
            Some(p) if idx + p + PROXY_ENTRY_SEP.len() < bytes.len() => {
                idx += p + PROXY_ENTRY_SEP.len();
            }
            _ => break,
        }
    }
    out
}

/// 取（必要时增量刷新）某文件的条目索引。
/// 增量条件：文件变大且 mtime 前进（追加场景）——重扫起点取「最后一条的起点」，
/// 因为该条上次索引时可能尚未写完；其余情况（截断 / 被替换 / 时间戳未更新）整份重建。
fn proxy_file_index(path: &Path) -> Result<Arc<ProxyFileIndex>, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("读取日志文件属性失败: {e}"))?;
    let len = meta.len();
    let mtime_ms = file_mtime_ms(&meta);
    let key = path.to_string_lossy().to_string();

    let cached = {
        let cache = proxy_index_cache().lock().unwrap_or_else(|e| e.into_inner());
        cache.get(&key).cloned()
    };
    if let Some(idx) = &cached {
        if idx.len == len && idx.mtime_ms == mtime_ms {
            return Ok(idx.clone());
        }
    }

    let (mut entries, from) = match &cached {
        Some(idx) if idx.len < len && idx.mtime_ms < mtime_ms => {
            let mut kept = idx.entries.clone();
            let from = kept.pop().map(|e| e.start).unwrap_or(0);
            (kept, from)
        }
        _ => (Vec::new(), 0),
    };
    if from < len {
        let bytes = read_range(path, from, len)?;
        entries.extend(scan_index_entries(&bytes, from));
    }
    let idx = Arc::new(ProxyFileIndex { len, mtime_ms, entries });
    proxy_index_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, idx.clone());
    Ok(idx)
}

/// 读取条目正文（trim 后，语义同原实现的 `chunk.trim()`）
fn read_entry_text(path: &Path, entry: &ProxyIndexedEntry) -> Result<String, String> {
    let bytes = read_range(path, entry.start, entry.end)?;
    Ok(String::from_utf8_lossy(&bytes).trim().to_string())
}

fn ts_in_range(ts: &str, start: &str, end: &str) -> bool {
    if !start.is_empty() && ts < start {
        return false;
    }
    if !end.is_empty() && ts > end {
        return false;
    }
    true
}

/// 列表命令（`async` 属性 → 独立线程执行：冷启动首次建索引要顺序读日志，不占 UI 线程）
#[tauri::command(async)]
pub fn proxy_logs_list(
    state: State<AppState>,
    opts: ProxyLogQueryOpts,
) -> Result<ProxyLogListResult, String> {
    let log_dir = proxy_log_dir(&state);
    proxy_logs_list_impl(&log_dir, &opts)
}

/// 收集一批文件里的候选条目（保持文件升序 + 文件内升序 = 时间升序）。
/// 无关键字：走内存索引，不读正文；有关键字：整文件顺序读一次（不再逐条 parse 成 struct）。
fn collect_proxy_candidates(
    log_dir: &Path,
    files: &[String],
    keyword: &str,
    start: &str,
    end: &str,
) -> Result<Vec<(String, usize, ProxyIndexedEntry)>, String> {
    let mut out = Vec::new();
    for name in files {
        let path = log_dir.join(name);
        if keyword.is_empty() {
            // 健壮性（对齐基线 `Err(_) => continue`）：单文件读失败（被独占锁定、权限变化等）
            // 跳过该文件继续其余文件，不让整个列表失败
            let idx = match proxy_file_index(&path) {
                Ok(idx) => idx,
                Err(_) => continue,
            };
            for (ei, e) in idx.entries.iter().enumerate() {
                if ts_in_range(e.ts_str(), start, end) {
                    out.push((name.clone(), ei, *e));
                }
            }
        } else {
            // 同上：单文件读失败跳过（基线关键字路径同样是整读，读失败 continue）
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(_) => continue,
            };
            for (ei, e) in scan_index_entries(&bytes, 0).iter().enumerate() {
                if !ts_in_range(e.ts_str(), start, end) {
                    continue;
                }
                let text = String::from_utf8_lossy(&bytes[e.start as usize..e.end as usize]);
                if text.to_lowercase().contains(keyword) {
                    out.push((name.clone(), ei, *e));
                }
            }
        }
    }
    Ok(out)
}

/// 列表核心实现（拆出来便于单测直接喂临时目录）
fn proxy_logs_list_impl(
    log_dir: &Path,
    opts: &ProxyLogQueryOpts,
) -> Result<ProxyLogListResult, String> {
    if !log_dir.exists() {
        return Ok(ProxyLogListResult {
            entries: vec![],
            total: 0,
        });
    }
    let keyword = opts.keyword.as_deref().unwrap_or("").trim().to_lowercase();
    let start = opts.start_time.as_deref().unwrap_or("");
    let end = opts.end_time.as_deref().unwrap_or("");
    let offset = opts.offset.unwrap_or(0);
    let limit = opts.limit.unwrap_or(50);

    let files = proxy_log_files(log_dir)?;
    retain_alive_indexes(log_dir, &files);

    // 候选收集：多线程（文件之间互不依赖；冷建索引与关键字全文扫描都是 IO+CPU 密集）。
    // 分片按文件名升序切分，拼接后仍是「文件升序 + 文件内升序」= 时间升序
    let threads = files.len().min(4).max(1);
    let chunk = files.len().div_ceil(threads).max(1);
    let keyword_ref: &str = &keyword;
    let parts: Vec<Result<Vec<(String, usize, ProxyIndexedEntry)>, String>> =
        std::thread::scope(|scope| {
            let handles: Vec<_> = files
                .chunks(chunk)
                .map(|c| {
                    scope.spawn(move || {
                        collect_proxy_candidates(log_dir, c, keyword_ref, start, end)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_else(|_| Err("日志扫描线程异常退出".to_string())))
                .collect()
        });
    let mut candidates: Vec<(String, usize, ProxyIndexedEntry)> = Vec::new();
    for part in parts {
        candidates.extend(part?);
    }
    candidates.reverse();

    // total 口径 = 非空块数（与列表 id 的「文件内原始分块序号」语义绑定）：
    // 极少数解析失败的畸形块计入 total 但页内跳过（末页可短）；若在索引构建期
    // 过滤会移动原始序号、破坏详情 id 对齐，故保留该口径
    let total = candidates.len();
    let entries: Vec<ProxyLogEntry> = candidates
        .into_iter()
        .skip(offset)
        .take(limit)
        .filter_map(|(name, ei, e)| {
            // 单条读取失败（候选收集后文件恰被删除/轮转）降级跳过该条，不让整页失败
            let text = read_entry_text(&log_dir.join(&name), &e).ok()?;
            parse_proxy_entry(&text, &name, ei)
        })
        .collect();
    Ok(ProxyLogListResult { entries, total })
}

/// 详情命令（`async` 属性 → 独立线程执行）
#[tauri::command(async)]
pub fn proxy_log_detail(state: State<AppState>, id: String) -> Result<String, String> {
    proxy_log_detail_impl(&proxy_log_dir(&state), &id)
}

/// 详情核心实现（拆出来便于单测直接喂临时目录）
fn proxy_log_detail_impl(log_dir: &Path, id: &str) -> Result<String, String> {
    // id 格式: "filename:index"
    let parts: Vec<&str> = id.splitn(2, ':').collect();
    if parts.len() != 2 {
        return Err("无效的日志 ID".into());
    }
    let file_name = parts[0];
    let index: usize = parts[1].parse().map_err(|_| "无效的索引")?;
    // 审查修复（任意文件读取/路径遍历）：文件名与列表接口同一白名单，
    // 并拒绝路径分隔符——"..\..\x:0" 类输入此前可直接 join 读任意文件
    if !file_name.starts_with("proxy_req_")
        || !file_name.ends_with(".log")
        || file_name.contains('\\')
        || file_name.contains('/')
        || file_name.contains("..")
    {
        return Err("无效的日志文件名".into());
    }

    let path = log_dir.join(file_name);
    // 性能（2026-10-07）：只读该条所在字节区间（原实现为取 1 条读完整文件，单文件最大 60MB）。
    // 序号口径不变 = 文件内**原始**非空分块序号（与列表 id 一致）。
    let idx = proxy_file_index(&path)?;
    let entry = idx
        .entries
        .get(index)
        .ok_or_else(|| "找不到指定的日志条目".to_string())?;
    read_entry_text(&path, entry)
}

// ---------------- JWT 解析 ----------------

#[derive(Serialize)]
pub struct JwtParseResult {
    pub user_id: Option<String>,
    pub exp_hours: Option<f64>,
    pub exp_timestamp: Option<i64>,
    pub status: String,
}

#[tauri::command]
pub fn jwt_parse(_app: AppHandle, _state: State<AppState>, jwt: String) -> JwtParseResult {
    let info = jwt::parse(&jwt);
    let status = jwt::status_of(info.exp_hours).to_string();
    JwtParseResult {
        user_id: info.user_id,
        exp_hours: info.exp_hours,
        exp_timestamp: info.exp_timestamp,
        status,
    }
}

// ---------------- 日志 ----------------

#[derive(Deserialize)]
pub struct LogsOpts {
    #[serde(default)]
    pub log_type: Option<String>,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub keyword: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Serialize)]
pub struct LogLine {
    pub time: String,
    pub log_type: String,
    pub message: String,
}

#[tauri::command]
pub fn logs_query(state: State<AppState>, opts: LogsOpts) -> Vec<LogLine> {
    let files = [
        ("proxy", "proxy.log"),
        ("checkin", "checkin.log"),
        ("switch", "switcher.log"),
        ("app", "app.log"),
    ];
    let mut out = Vec::new();
    for (t, fname) in files {
        if let Some(ref want) = opts.log_type {
            if want != "all" && want != t {
                continue;
            }
        }
        let p = state.path("logs").join(fname);
        if let Ok(bytes) = std::fs::read(&p) {
            let content = String::from_utf8_lossy(&bytes);
            for raw in content.lines() {
                let (time, msg) = split_time(raw);
                if let Some(ref date) = opts.date {
                    if !time.starts_with(date) {
                        continue;
                    }
                }
                if let Some(kw) = &opts.keyword {
                    if !msg.contains(kw) && !time.contains(kw) {
                        continue;
                    }
                }
                out.push(LogLine {
                    time,
                    log_type: t.to_string(),
                    message: msg,
                });
            }
        }
    }
    out.sort_by(|a, b| b.time.cmp(&a.time));
    let limit = opts.limit.unwrap_or(500);
    out.into_iter().take(limit).collect()
}

fn split_time(raw: &str) -> (String, String) {
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    if raw.starts_with('[') {
        if let Some(end) = raw.find("] ") {
            let time = raw[1..end].to_string();
            return (time, raw[end + 2..].to_string());
        }
    }
    ("".to_string(), raw.to_string())
}

/// 清理指定类型日志文件（proxy/checkin/switch/app；all 为全清）。
/// 各写入方均为「每次追加时重新打开」，删除后文件按需自动重建，无需特殊处理。
/// 返回实际删除的文件数。
#[tauri::command]
pub fn logs_clear(state: State<AppState>, log_type: String) -> Result<u32, String> {
    let files = [
        ("proxy", "proxy.log"),
        ("checkin", "checkin.log"),
        ("switch", "switcher.log"),
        ("app", "app.log"),
    ];
    let mut removed = 0u32;
    for (t, fname) in files {
        if log_type != "all" && log_type != t {
            continue;
        }
        let p = state.path("logs").join(fname);
        match std::fs::remove_file(&p) {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("删除 {fname} 失败: {e}")),
        }
    }
    crate::fs_utils::app_log(
        &state.data_dir,
        &format!("已清理日志: {log_type}（删除 {removed} 个文件）"),
    );
    Ok(removed)
}

// ---------------- 设置 ----------------

#[tauri::command]
pub fn settings_get(state: State<AppState>) -> Settings {
    state.settings()
}

/// 通知渠道「发送测试」（F-19）：按当前已保存配置经全部已配置渠道推送一条测试消息。
/// 渠道失败汇总返回（如全部渠道未配置则提示先配置），不影响主流程。
#[tauri::command(async)]
pub fn notify_test(state: State<AppState>) -> Result<String, String> {
    let s = state.settings();
    let channels = crate::notify::NotifyChannels {
        bark_url: s.notify_bark_url.clone().filter(|x| !x.trim().is_empty()),
        wechat_webhook: s.notify_webhook_url.clone().filter(|x| !x.trim().is_empty()),
        serverchan_sendkey: s.notify_serverchan_sendkey.clone().filter(|x| !x.trim().is_empty()),
    };
    if channels.is_empty() {
        return Err("尚未配置任何通知渠道（Bark / 通用 Webhook / Server酱）".into());
    }
    if !s.notify_enabled {
        return Err("通知推送总开关未启用".into());
    }
    let title = "AI Work Assistant 测试通知";
    let body = "这是一条测试消息：如果你收到它，说明对应通知渠道配置生效。";
    let mut errs: Vec<String> = Vec::new();
    if let Some(bark) = channels.bark_url.as_deref().filter(|x| !x.trim().is_empty()) {
        if let Err(e) = crate::notify::notify_bark(bark, title, body) {
            errs.push(format!("Bark: {e}"));
        }
    }
    if let Some(wh) = channels.wechat_webhook.as_deref().filter(|x| !x.trim().is_empty()) {
        if let Err(e) = crate::notify::notify_wechat_webhook(wh, title, body) {
            errs.push(format!("Webhook: {e}"));
        }
    }
    if let Some(sk) = channels.serverchan_sendkey.as_deref().filter(|x| !x.trim().is_empty()) {
        if let Err(e) = crate::notify::notify_serverchan(sk, title, body) {
            errs.push(format!("Server酱: {e}"));
        }
    }
    if errs.is_empty() {
        Ok("测试消息已发送至全部已配置渠道".into())
    } else {
        Err(errs.join("；"))
    }
}

#[tauri::command]
pub fn settings_set(state: State<AppState>, patch: serde_json::Value) -> Result<(), String> {
    // SQLite 化（P2）：app_settings 入 kv 文档（patch 合并语义不变）
    let store = crate::store::db(&state.data_dir);
    let mut current: serde_json::Value = store.kv_get("app_settings");
    // 内容为 null 时初始化为空对象，避免 patch 被丢弃
    if !current.is_object() {
        current = serde_json::json!({});
    }
    if let (Some(current_obj), Some(patch_obj)) =
        (current.as_object_mut(), patch.as_object())
    {
        for (k, v) in patch_obj {
            // 调度时刻防御性校验：空 hhmm 的既有语义是「从未配置」（state.rs 据此回填
            // 默认开+默认时刻），显式存空串会破坏该前提（回填覆盖显式关闭）；
            // 非空值必须为合法 HH:MM（schtasks 还原/调度器均依赖该格式）。
            if k.ends_with("_hhmm") {
                match v.as_str() {
                    Some(s) if s.trim().is_empty() => continue,
                    Some(s) => validate_hhmm(s)?,
                    // 非字符串（含 null）不写入，避免破坏 Settings 的 String 反序列化
                    None => continue,
                }
            }
            current_obj.insert(k.clone(), v.clone());
        }
    }
    store.kv_set("app_settings", &current)
}

// ---------------- 积分历史（供看板/趋势图） ----------------

#[tauri::command]
pub fn credits_history(state: State<AppState>) -> Vec<CreditRecord> {
    crate::store::docs::credits_history_load(&crate::store::db(&state.data_dir)).records
}

// ---------------- 邀请 ----------------

#[derive(Serialize)]
pub struct Invite {
    pub url: String,
}

#[tauri::command]
pub fn invite_link(_app: AppHandle, _state: State<AppState>) -> Invite {
    Invite {
        url: INVITE_LINK.to_string(),
    }
}

// ---------------- 文件导出 ----------------

/// 可执行/脚本扩展名（小写无点）：导出到应用数据目录之外一律拒绝，
/// 防止「用户自选导出路径」被滥用为开机/登录持久化植入
const BLOCKED_EXEC_EXTS: &[&str] = &[
    "exe", "dll", "bat", "cmd", "ps1", "vbs", "vbe", "js", "jse",
    "wsf", "hta", "scr", "lnk", "msi", "com", "pif",
];

/// Windows 路径归一化（仅用于前缀比较）：分隔符统一为 `\`、剥离 verbatim
/// `\\?\` / `\\?\UNC\` 前缀、去尾部分隔符、转小写（Windows 不区分大小写）
#[cfg(windows)]
fn normalize_win_path(p: &std::path::Path) -> String {
    let s = p.to_string_lossy().replace('/', "\\");
    let s = if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        s
    };
    s.trim_end_matches('\\').to_lowercase()
}

/// 前缀比较用归一化（check_export_path 唯一入口）：
/// Windows 走 normalize_win_path；mac 只统一分隔符与尾部斜杠（保留大小写，
/// APFS 大小写敏感卷上小写折叠会误判）
#[cfg(windows)]
fn normalize_for_prefix(p: &std::path::Path) -> String {
    normalize_win_path(p)
}

#[cfg(target_os = "macos")]
fn normalize_for_prefix(p: &std::path::Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    s.trim_end_matches('/').to_string()
}

/// 目录前缀匹配：相等，或 path 位于 prefix 的子目录内
/// （避免 `C:\Windows` 误伤 `C:\Windows-Empire` 这类兄弟目录）
#[cfg(windows)]
fn starts_with_dir(path_norm: &str, prefix_norm: &str) -> bool {
    if prefix_norm.is_empty() {
        return false;
    }
    path_norm == prefix_norm || path_norm.starts_with(&format!("{prefix_norm}\\"))
}

#[cfg(target_os = "macos")]
fn starts_with_dir(path_norm: &str, prefix_norm: &str) -> bool {
    if prefix_norm.is_empty() {
        return false;
    }
    path_norm == prefix_norm || path_norm.starts_with(&format!("{prefix_norm}/"))
}

/// 收集需禁止写入的系统目录前缀（Windows）：系统目录、Program Files、
/// 开始菜单（含用户/公共启动文件夹）。环境变量缺失时用常见默认值兜底
#[cfg(windows)]
fn blocked_system_dirs() -> Vec<std::path::PathBuf> {
    let env_or = |k: &str, fb: &str| std::env::var(k).unwrap_or_else(|_| fb.to_string());
    let mut v = Vec::new();
    v.push(std::path::PathBuf::from(env_or("SystemRoot", r"C:\Windows")));
    v.push(std::path::PathBuf::from(env_or("ProgramFiles", r"C:\Program Files")));
    v.push(std::path::PathBuf::from(env_or("ProgramFiles(x86)", r"C:\Program Files (x86)")));
    let program_data = env_or("ProgramData", r"C:\ProgramData");
    v.push(std::path::PathBuf::from(&program_data).join(r"Microsoft\Windows\Start Menu"));
    v.push(std::path::PathBuf::from(&program_data)
        .join(r"Microsoft\Windows\Start Menu\Programs\Startup"));
    if let Ok(appdata) = std::env::var("APPDATA") {
        v.push(std::path::PathBuf::from(appdata)
            .join(r"Microsoft\Windows\Start Menu\Programs\Startup"));
    }
    v
}

/// 收集需禁止写入的系统目录前缀（macOS）：SIP 保护域与全机 Library、私有根、
/// /etc（/private/etc 符链），另加用户级持久化目录 ~/Library/LaunchAgents 与
/// ~/Library/LaunchDaemons（审查 P2：LaunchAgent 当前用户可写，是用户级
/// 开机自启持久化滥用面——语义对齐 Windows 分支的启动文件夹封禁；
/// LaunchDaemons 需 root 但一并封禁防提权写入）——HOME 缺失时跳过用户级两项
#[cfg(target_os = "macos")]
fn blocked_system_dirs() -> Vec<std::path::PathBuf> {
    let mut v = vec![
        std::path::PathBuf::from("/System"),
        std::path::PathBuf::from("/Library"),
        std::path::PathBuf::from("/private"),
        std::path::PathBuf::from("/etc"),
    ];
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            v.push(std::path::PathBuf::from(&home).join("Library/LaunchAgents"));
            v.push(std::path::PathBuf::from(&home).join("Library/LaunchDaemons"));
        }
    }
    v
}

/// 导出路径校验（canonicalize 失败时对原路径做前缀判断）：
/// ① 命中系统目录/启动文件夹前缀 → 拒绝；
/// ② 可执行/脚本扩展名（或无扩展名）且不在应用数据目录下 → 拒绝
///（审查 P1-2：无扩展名文件此前直接放行，可写 `.git/hooks/pre-commit`、
/// `.bashrc`、`authorized_keys` 等无扩展名持久化/注入目标，按高风险处理）
fn check_export_path(
    path: &std::path::Path,
    data_dir: &std::path::Path,
    blocked: &[std::path::PathBuf],
) -> Result<(), String> {
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let norm = normalize_for_prefix(&canon);
    for b in blocked {
        if starts_with_dir(&norm, &normalize_for_prefix(b)) {
            return Err(
                "拒绝写入：目标位于系统目录或启动文件夹，为防止持久化滥用不允许导出到该位置".into(),
            );
        }
    }
    let exec_ok = || -> bool {
        let data_canon = std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
        starts_with_dir(&norm, &normalize_for_prefix(&data_canon))
    };
    let is_exec = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| BLOCKED_EXEC_EXTS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(true); // 无扩展名（.git/hooks/pre-commit 等）按高风险处理
    if is_exec && !exec_ok() {
        return Err("拒绝写入：可执行/脚本/无扩展名文件仅允许导出到应用数据目录内".into());
    }
    Ok(())
}

#[tauri::command]
pub fn write_text_file(state: State<AppState>, path: String, content: String) -> Result<(), String> {
    let p = std::path::PathBuf::from(&path);
    check_export_path(&p, &state.data_dir, &blocked_system_dirs())?;
    std::fs::write(&p, content.as_bytes()).map_err(|e| format!("写入文件失败: {e}"))
}

/// 读取允许的文本扩展名白名单（小写无点）：read_text_file 面向「导入账号/配置」
/// 场景（前端 open 对话框过滤 JSON）。白名单防止渲染层被攻陷后读取任意敏感文件
///（SSH 私钥/`.env` 等多为无扩展名或专用扩展名，天然不在白名单内；审查 P1-1）
const ALLOWED_READ_EXTS: &[&str] = &["json", "txt", "log", "csv", "md"];

/// 读取本地文本文件（配合导入账号：文件选择后由 Rust 侧读取，避免前端路径权限问题）
#[tauri::command]
pub fn read_text_file(path: String) -> Result<String, String> {
    let p = std::path::PathBuf::from(&path);
    // 扩展名白名单（大小写不敏感）：无扩展名/未知扩展名一律拒绝
    let ext_ok = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| ALLOWED_READ_EXTS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false);
    if !ext_ok {
        return Err("仅允许读取文本类文件（json/txt/log/csv/md）".into());
    }
    let meta = std::fs::metadata(&path).map_err(|e| format!("读取文件失败: {e}"))?;
    if !meta.is_file() {
        return Err("路径不是常规文件".into());
    }
    // 大小上限 10MB：导入的账号/配置 JSON 远小于此，防止误选超大文件拖垮前端
    if meta.len() > 10 * 1024 * 1024 {
        return Err("文件过大（超过 10MB），请确认选择的是账号/配置 JSON 文件".into());
    }
    let bytes =
        std::fs::read(&path).map_err(|e| format!("读取文件失败: {e}"))?;
    String::from_utf8(bytes).map_err(|_| "文件不是有效的 UTF-8 文本".into())
}

// ---------------- 定时任务 ----------------

/// 新版每日签到计划任务名（品牌 ai-work-assistant）
pub const TASK_NAME: &str = "AIWorkAssistant_DailyCheckin";
/// 旧版计划任务名（品牌迁移前），启动时自动迁移到新任务名
pub const LEGACY_TASK_NAME: &str = "TraeWorkAssistant_DailyCheckin";
/// 豆包会话续期每日计划任务名（P3）
pub const DOUBAO_TASK_NAME: &str = "AIWorkAssistant_DoubaoRenew";
/// 豆包额度巡检每日计划任务名（批量查额度 + 回写缓存/历史 + 用完记录）
pub const DOUBAO_QUOTA_TASK_NAME: &str = "AIWorkAssistant_DoubaoQuotaCheck";

// 运行 schtasks 并正确解码输出。
// 关键：默认控制台代码页是 GBK（中文 Windows），schtasks 的中文报错(如"系统找不到指定的文件")
// 以 GBK 字节输出；若直接 from_utf8_lossy 会读成 ϵͳ... 乱码，导致 "找不到" 永远匹配不上、
// 错误文案变成乱码。前置 `chcp 65001` 让 schtasks 以 UTF-8 输出，从而能正确匹配与展示。
// 返回 (成功?, stdout, stderr)，三者均为 UTF-8 字符串。
/// 严格校验 HH:MM 时间格式（schtasks /ST 参数）。
/// 审查修复（命令注入）：time 经 `cmd /c … && schtasks /ST <time>` 执行，cmd 对不含
/// 空格/引号的参数不做引号包裹，`12:00&calc` 类输入会把 `&` 解释为命令分隔符实现
/// 任意命令执行。白名单校验在 sink 入口统一拦死，调用方（misc/doubao/workbuddy）共用。
pub(crate) fn validate_hhmm(time: &str) -> Result<(), String> {
    let t = time.trim();
    let valid = t.len() == 5
        && t.as_bytes()[2] == b':'
        && t.as_bytes()[..2].iter().all(u8::is_ascii_digit)
        && t.as_bytes()[3..].iter().all(u8::is_ascii_digit)
        && t[..2].parse::<u8>().map(|h| h < 24).unwrap_or(false)
        && t[3..].parse::<u8>().map(|m| m < 60).unwrap_or(false);
    if !valid {
        return Err(format!("时间格式无效: {time}（应为 HH:MM）"));
    }
    Ok(())
}

/// F-75 M2-2.4：schtasks 注册面 mac 明示错误文案（命令级门控统一出口）。
/// mac 定时主路径 = 内置调度器 tasks/scheduler.rs + 开机自启 + 静默签到（设计 §5.4.1，
/// 功能零缺口），schtasks 6 任务名 + .cmd 启动器 + 2 个迁移函数整体 Windows 专属。
#[cfg(not(windows))]
const SCHTASKS_UNSUPPORTED_MSG: &str =
    "系统级计划任务注册仅支持 Windows；macOS 请开启「开机自启 + 静默签到」，应用运行期间由内置调度器按时执行";

/// 命令级 schtasks 门控：mac 恒 Err（明示文案），Windows 恒 Ok。
/// 审查修复（P1）：替代 `#[cfg] { return ...; }` 直排模式——直排块在 mac 构建中
/// 展开为无条件 return，其后命令体被编译器判定不可达 → unreachable_code 警告
/// （零警告红线）。gate 形态下命令体在编译器视角仍可达（Result 非幂类型），
/// 运行时 mac 恒早退，命令体零改动。
#[cfg(not(windows))]
pub(crate) fn schtasks_gate() -> Result<(), String> {
    Err(SCHTASKS_UNSUPPORTED_MSG.to_string())
}
#[cfg(windows)]
pub(crate) fn schtasks_gate() -> Result<(), String> {
    Ok(())
}

pub(crate) fn run_schtasks(args: &[&str]) -> Result<(bool, String, String), String> {
    let mut full: Vec<String> = vec![
        "/c".to_string(),
        "chcp".to_string(),
        "65001".to_string(),
        ">nul".to_string(),
        "&&".to_string(),
        "schtasks".to_string(),
    ];
    for a in args {
        full.push((*a).to_string());
    }
    let out = sys_command("cmd")
        .args(&full)
        .output()
        .map_err(|e| format!("执行 schtasks 失败: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    Ok((out.status.success(), stdout, stderr))
}

/// 把计划任务的长命令写入数据目录的 .cmd 启动器，返回启动器路径。
/// 公共实现（自 doubao.rs 提升，签到任务与豆包续期/额度任务共用）：
/// 背景（实测 2026-09-09）：schtasks /TR 参数上限 **261 字符**，dev 构建的
/// python/脚本绝对路径拼出的命令达 273 字符 → schtasks 报参数错误，注册失败，
/// 而错误 toast 仅显示 4 秒，被用户感知为「点击注册没有反应」。改用启动器后
/// /TR 只需 ~74 字符。
pub(crate) fn write_task_launcher(
    state: &AppState,
    name: &str,
    body: String,
) -> Result<String, String> {
    let path = state.data_dir.join(format!("task_{name}.cmd"));
    std::fs::write(&path, format!("@echo off\r\n{body}\r\n"))
        .map_err(|e| format!("写入任务启动器脚本失败: {e}"))?;
    Ok(path.to_string_lossy().to_string())
}

/// 构造每日签到计划任务的 /TR：主 exe 直调 CLI 任务模式（--task-run checkin，
/// 见 tasks::run_cli_task），不再依赖 python 运行时；schtasks 不继承进程环境变量，
/// cmd /c 内显式 set AIWORKDATA_DIR（与 WorkBuddy 任务注册同款模式）。
fn build_task_tr(state: &AppState) -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("获取主程序路径失败: {e}"))?;
    let data_dir = state.data_dir.to_string_lossy().to_string();
    Ok(format!(
        "cmd /c set \"AIWORKDATA_DIR={}\" && \"{}\" --task-run checkin",
        data_dir,
        exe.to_string_lossy()
    ))
}

/// 注册每日签到任务（新任务名），供命令与旧任务迁移共用
fn register_daily_task(state: &AppState, time: &str) -> Result<(), String> {
    let tr = build_task_tr(state)?;
    let (ok, _stdout, stderr) = run_schtasks(&[
        "/Create",
        "/TN",
        TASK_NAME,
        "/TR",
        tr.as_str(),
        "/SC",
        "DAILY",
        "/ST",
        time,
        "/F",
    ])?;
    if !ok {
        let detail = stderr.trim();
        // 权限不足：最常见的失败原因（/RL HIGHEST 或普通用户受限）
        let is_access_denied = detail.contains("Access is denied")
            || detail.contains("ERROR: Access is denied")
            || detail.contains("拒绝访问")
            || detail.contains("权限");
        if is_access_denied {
            let exe = std::env::current_exe()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| "<主程序路径>".into());
            return Err(format!(
                "权限不足（Access Denied）。\n\n\
                 解决方法（任选其一）：\n\
                 1. 右键 AI Work 助手 →「以管理员身份运行」后重新点击「注册任务」\n\
                 2. 打开「管理员命令提示符」手动执行：\n\
                    schtasks /Create /TN {TASK_NAME} /TR \"cmd /c set \\\"AIWORKDATA_DIR={}\\\" && \\\"{}\\\" --task-run checkin\" /SC DAILY /ST {time} /F\n\
                 3. 如不需最高权限，可去掉 /RL HIGHEST 后重试",
                state.data_dir.to_string_lossy(),
                exe
            ));
        }
        return Err(detail.to_string());
    }
    Ok(())
}

// 计划任务命令含 schtasks 子进程调用（可达数秒），标记 async 派发到线程池执行，避免阻塞 UI
#[tauri::command(async)]
pub fn task_register(state: State<AppState>, time: String) -> Result<(), String> {
    schtasks_gate()?;
    validate_hhmm(&time)?;
    register_daily_task(&state, &time)?;
    // 注册成功后清理旧版计划任务（品牌迁移），失败不影响本次注册
    let _ = run_schtasks(&["/Delete", "/TN", LEGACY_TASK_NAME, "/F"]);
    Ok(())
}

/// 查询计划任务是否存在
fn task_exists(name: &str) -> bool {
    let (ok, _stdout, _stderr) = match run_schtasks(&["/Query", "/TN", name, "/FO", "LIST"]) {
        Ok(v) => v,
        Err(_) => return false,
    };
    ok
}

/// 导出计划任务 XML 并解析每日触发时间（HH:MM）。
/// schtasks /XML 输出为 UTF-16LE（带 BOM），需按 UTF-16 解码；解析失败返回 None。
fn legacy_task_start_time(name: &str) -> Option<String> {
    let out = sys_command("cmd")
        .args([
            "/c",
            "chcp",
            "65001",
            ">nul",
            "&&",
            "schtasks",
            "/Query",
            "/TN",
            name,
            "/XML",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let xml = if out.stdout.starts_with(&[0xFF, 0xFE]) {
        let units: Vec<u16> = out.stdout[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    let start = xml
        .split("<StartBoundary>")
        .nth(1)?
        .split("</StartBoundary>")
        .next()?;
    // 形如 2026-09-07T09:30:00 → 取 T 后的 HH:MM
    let time = start.split('T').nth(1)?;
    let hh_mm = time.get(0..5)?;
    let ok = hh_mm.len() == 5
        && hh_mm.as_bytes()[2] == b':'
        && hh_mm.chars().all(|c| c.is_ascii_digit() || c == ':');
    if ok { Some(hh_mm.to_string()) } else { None }
}

/// 旧版计划任务自动迁移（品牌迁移，**并存语义**）：
/// 检测到 TraeWorkAssistant_DailyCheckin（指向旧 exe/旧数据目录）时，
/// 按其原触发时间重建 AIWorkAssistant_DailyCheckin；**旧任务始终保留**，
/// 供老应用继续使用（两版并存，各自独立签到）。任何一步失败都静默跳过。
pub fn try_migrate_legacy_task(state: &AppState) -> Option<String> {
    let legacy = task_exists(LEGACY_TASK_NAME);
    if !legacy {
        return None;
    }
    if task_exists(TASK_NAME) {
        // 新旧并存（如用户手动注册过新任务）：旧任务属老应用，保留不动
        return Some(
            "任务迁移：新旧计划任务并存，各自服务对应应用（旧任务保留给老应用）".to_string(),
        );
    }
    let time = match legacy_task_start_time(LEGACY_TASK_NAME) {
        Some(t) => t,
        None => {
            return Some(
                "任务迁移：检测到旧任务 TraeWorkAssistant_DailyCheckin，但未能解析其触发时间，请在设置页手动注册新任务"
                    .to_string(),
            )
        }
    };
    match register_daily_task(state, &time) {
        Ok(()) => Some(format!(
            "任务迁移：已按旧任务触发时间创建新任务 {TASK_NAME}（每日 {time}）；旧任务保留供老应用继续使用"
        )),
        Err(e) => Some(format!(
            "任务迁移：检测到旧任务 {LEGACY_TASK_NAME}，创建新任务失败（{e}），可在设置页手动注册"
        )),
    }
}

#[tauri::command(async)]
pub fn task_status(state: State<AppState>, _app: AppHandle) -> Result<String, String> {
    schtasks_gate()?;
    let (ok, stdout, stderr) = run_schtasks(&["/Query", "/TN", TASK_NAME, "/FO", "LIST"])?;
    if !ok {
        let detail = if !stderr.trim().is_empty() {
            stderr.trim()
        } else {
            stdout.trim()
        };
        // 任务本就不存在：返回友好提示而非带乱码的错误，避免前端叠加"查询失败："前缀
        if detail.contains("can't find")
            || detail.contains("找不到")
            || detail.contains("does not exist")
            || detail.contains("ERROR: The system cannot find")
            || detail.contains("系统找不到")
        {
            // 尝试顺带迁移旧任务；迁移成功则再次查询
            if try_migrate_legacy_task(&state).is_some() && task_exists(TASK_NAME) {
                let (ok2, stdout2, _e2) =
                    run_schtasks(&["/Query", "/TN", TASK_NAME, "/FO", "LIST"])?;
                if ok2 {
                    return Ok(stdout2);
                }
            }
            return Ok("未注册每日签到任务（请先在设置页点击「注册任务」）。".to_string());
        }
        return Err(detail.to_string());
    }
    Ok(stdout)
}

#[tauri::command(async)]
pub fn task_unregister(_app: AppHandle, _state: State<AppState>) -> Result<(), String> {
    schtasks_gate()?;
    // 只删除本应用的新任务名；旧任务 TraeWorkAssistant_DailyCheckin 属老应用，
    // 两版并存时不得越权删除（老应用的签到计划需继续工作）
    let mut last_detail = String::new();
    let mut deleted = false;
    for name in [TASK_NAME] {
        let (ok, _stdout, stderr) = run_schtasks(&["/Delete", "/TN", name, "/F"])?;
        if ok {
            deleted = true;
            continue;
        }
        let detail = stderr.trim();
        // 任务本就不存在：视为已删除，不报错
        if detail.contains("can't find")
            || detail.contains("找不到")
            || detail.contains("does not exist")
            || detail.contains("ERROR: The system cannot find")
            || detail.contains("系统找不到")
        {
            continue;
        }
        last_detail = detail.to_string();
    }
    if !deleted && !last_detail.is_empty() {
        return Err(last_detail);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 临时目录（独占 tag，避免测试间互踩）
    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("misc_export_test_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 拒绝写入启动文件夹下的 .json（拦截的是位置而非扩展名）
    #[test]
    fn export_rejects_json_under_startup() {
        let base = tmp_dir("startup");
        let startup = base.join("Startup");
        std::fs::create_dir_all(&startup).unwrap();
        let err = check_export_path(&startup.join("cfg.json"), &base, &[startup]).unwrap_err();
        assert!(err.contains("启动文件夹"), "实际错误: {err}");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 拒绝在应用数据目录之外导出可执行/脚本类型
    #[test]
    fn export_rejects_bat_outside_data_dir() {
        let data = tmp_dir("datadir");
        let other = tmp_dir("other");
        let err = check_export_path(&other.join("evil.bat"), &data, &[]).unwrap_err();
        assert!(err.contains("可执行"), "实际错误: {err}");
        let _ = std::fs::remove_dir_all(&data);
        let _ = std::fs::remove_dir_all(&other);
    }

    /// 放行普通目录下的 .json 导出
    #[test]
    fn export_allows_plain_json() {
        let dir = tmp_dir("plain");
        assert!(check_export_path(&dir.join("export.json"), &dir, &[]).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 审查 P1-2 回归：无扩展名文件（.git/hooks/pre-commit、.bashrc 类）在
    /// 应用数据目录之外一律拒绝
    #[test]
    fn export_rejects_extensionless_outside_data_dir() {
        let data = tmp_dir("datadir2");
        let repo = tmp_dir("repo");
        let git_hooks = repo.join(".git").join("hooks");
        std::fs::create_dir_all(&git_hooks).unwrap();
        let err = check_export_path(&git_hooks.join("pre-commit"), &data, &[]).unwrap_err();
        assert!(err.contains("无扩展名") || err.contains("可执行"), "实际错误: {err}");
        let _ = std::fs::remove_dir_all(&data);
        let _ = std::fs::remove_dir_all(&repo);
    }

    // ---------------- 抓包日志：条目索引 / 分页 / 筛选 / 增量 ----------------

    const PROXY_SEP_LINE: &str =
        "================================================================================";

    fn proxy_entry_text(ts: &str, method: &str, host_path: &str, status: &str, extra: &str) -> String {
        format!("[{ts}] {method} {host_path}\n--- Request Headers ---\nx: y\n--- Response: {status} ---\n{extra}\n")
    }

    fn write_proxy_log(dir: &std::path::Path, name: &str, entries: &[String]) -> std::path::PathBuf {
        let p = dir.join(name);
        let mut s = String::new();
        for e in entries {
            s.push_str(e);
            s.push_str(PROXY_SEP_LINE);
            s.push('\n');
        }
        std::fs::write(&p, s).unwrap();
        p
    }

    fn proxy_opts(
        offset: usize,
        limit: usize,
        keyword: Option<&str>,
        start: Option<&str>,
        end: Option<&str>,
    ) -> ProxyLogQueryOpts {
        ProxyLogQueryOpts {
            keyword: keyword.map(str::to_string),
            start_time: start.map(str::to_string),
            end_time: end.map(str::to_string),
            offset: Some(offset),
            limit: Some(limit),
        }
    }

    /// 列表：新→旧排序、id 用文件内原始序号、分页、时间/关键字筛选、空块不计入、
    /// 详情与列表 id 对齐（原实现带筛选时 id 会错位）、追加后增量可见。
    #[test]
    fn proxy_logs_list_pages_filters_and_keeps_raw_index() {
        let dir = tmp_dir("proxy_logs");
        write_proxy_log(
            &dir,
            "proxy_req_2026-10-01.log",
            &[
                proxy_entry_text("2026-10-01 10:00:00", "GET", "a.example.com/x", "200 OK", "body-one"),
                proxy_entry_text("2026-10-01 11:00:00", "POST", "b.example.com/y", "500", "needle-here"),
            ],
        );
        let day2 = write_proxy_log(
            &dir,
            "proxy_req_2026-10-02.log",
            &[proxy_entry_text("2026-10-02 09:00:00", "GET", "c.example.com/z", "200 OK", "body-three")],
        );

        // 全量：新→旧；id 为文件内**原始**序号（不是筛选后的序号）
        let all = proxy_logs_list_impl(&dir, &proxy_opts(0, 50, None, None, None)).unwrap();
        assert_eq!(all.total, 3);
        assert_eq!(all.entries[0].id, "proxy_req_2026-10-02.log:0");
        assert_eq!(all.entries[0].timestamp, "2026-10-02 09:00:00");
        assert_eq!(all.entries[1].id, "proxy_req_2026-10-01.log:1");
        assert_eq!(all.entries[1].host, "b.example.com");
        assert_eq!(all.entries[1].status, "500");
        assert_eq!(all.entries[2].id, "proxy_req_2026-10-01.log:0");

        // 详情按 id 取到的就是同一条
        let detail = proxy_log_detail_impl(&dir, "proxy_req_2026-10-01.log:1").unwrap();
        assert!(detail.contains("needle-here"), "详情应为该条原文: {detail}");

        // 分页：offset=2 / limit=1 → 最旧那条
        let page = proxy_logs_list_impl(&dir, &proxy_opts(2, 1, None, None, None)).unwrap();
        assert_eq!(page.total, 3);
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].id, "proxy_req_2026-10-01.log:0");

        // 时间筛选（字符串比较口径同原实现）
        let only2 = proxy_logs_list_impl(
            &dir,
            &proxy_opts(
                0,
                50,
                None,
                Some("2026-10-02 00:00:00"),
                Some("2026-10-02 23:59:59"),
            ),
        )
        .unwrap();
        assert_eq!(only2.total, 1);
        assert_eq!(only2.entries[0].timestamp, "2026-10-02 09:00:00");

        // 关键字筛选（大小写不敏感）：命中条目的 id 仍是原始序号 1
        let hit =
            proxy_logs_list_impl(&dir, &proxy_opts(0, 50, Some("Needle-Here"), None, None)).unwrap();
        assert_eq!(hit.total, 1);
        assert_eq!(hit.entries[0].id, "proxy_req_2026-10-01.log:1");

        // 追加新条目 → 索引增量续扫，立即可见
        let mut s = std::fs::read_to_string(&day2).unwrap();
        s.push_str(&proxy_entry_text(
            "2026-10-02 10:00:00",
            "GET",
            "d.example.com/w",
            "201 Created",
            "body-four",
        ));
        s.push_str(PROXY_SEP_LINE);
        s.push('\n');
        std::fs::write(&day2, s).unwrap();
        let after = proxy_logs_list_impl(&dir, &proxy_opts(0, 50, None, None, None)).unwrap();
        assert_eq!(after.total, 4);
        assert_eq!(after.entries[0].id, "proxy_req_2026-10-02.log:1");
        assert_eq!(after.entries[0].path, "/w");

        // 连续分隔行 / 纯空白块不计入条目（对齐原实现 `chunk.trim()` 判空）
        let empty = dir.join("proxy_req_2026-10-03.log");
        std::fs::write(&empty, format!("{PROXY_SEP_LINE}\n\n{PROXY_SEP_LINE}\n")).unwrap();
        let with_empty = proxy_logs_list_impl(&dir, &proxy_opts(0, 50, None, None, None)).unwrap();
        assert_eq!(with_empty.total, 4, "空块不应计入条目数");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 诊断（手动跑）：在真实日志目录上量「冷建索引 / 热命中 / 深翻页 / 关键字筛选」耗时。
    /// 用法：`cargo test probe_proxy_logs_real_dir -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn probe_proxy_logs_real_dir_timing() {
        let dir = std::path::PathBuf::from(std::env::var("APPDATA").unwrap())
            .join("AIWorkAssistant")
            .join("logs");
        assert!(dir.is_dir(), "日志目录不存在: {}", dir.display());
        let mut total_bytes = 0u64;
        for name in proxy_log_files(&dir).unwrap() {
            total_bytes += std::fs::metadata(dir.join(&name)).map(|m| m.len()).unwrap_or(0);
        }

        let t0 = std::time::Instant::now();
        let cold = proxy_logs_list_impl(&dir, &proxy_opts(0, 30, None, None, None)).unwrap();
        let cold_ms = t0.elapsed().as_millis();

        let t1 = std::time::Instant::now();
        let warm = proxy_logs_list_impl(&dir, &proxy_opts(0, 30, None, None, None)).unwrap();
        let warm_ms = t1.elapsed().as_millis();

        let t2 = std::time::Instant::now();
        let deep = proxy_logs_list_impl(&dir, &proxy_opts(5000, 30, None, None, None)).unwrap();
        let deep_ms = t2.elapsed().as_millis();

        let t3 = std::time::Instant::now();
        let kw = proxy_logs_list_impl(&dir, &proxy_opts(0, 30, Some("openai"), None, None)).unwrap();
        let kw_ms = t3.elapsed().as_millis();

        println!(
            "日志目录 {} 个文件 / {}MB，条目 {}（冷建索引 {}ms / 热命中 {}ms / 深翻页 {}ms / 关键字 {}ms，关键字命中 {}）",
            proxy_log_files(&dir).unwrap().len(),
            total_bytes / 1024 / 1024,
            cold.total,
            cold_ms,
            warm_ms,
            deep_ms,
            kw_ms,
            kw.total
        );
        assert_eq!(cold.total, warm.total);
        assert_eq!(deep.entries.len().min(30), deep.entries.len());
    }

    /// 索引时间戳口径 = 原实现筛选分支：块首行（trim 后）的 1..20 字节
    #[test]
    fn proxy_index_ts_matches_first_line_slice() {
        let dir = tmp_dir("proxy_index_ts");
        write_proxy_log(
            &dir,
            "proxy_req_2026-10-05.log",
            &[
                proxy_entry_text("2026-10-05 08:00:00", "GET", "e.example.com/a", "200 OK", "x"),
                // 首行非时间戳形态（不足 20 字节）→ 时间戳为空串，带时间筛选时应被排除
                "short\n--- Response: 200 OK ---\n".to_string() + PROXY_SEP_LINE + "\n",
            ],
        );
        let all = proxy_logs_list_impl(&dir, &proxy_opts(0, 50, None, None, None)).unwrap();
        assert_eq!(all.total, 2);
        let filtered = proxy_logs_list_impl(
            &dir,
            &proxy_opts(0, 50, None, Some("2026-01-01 00:00:00"), None),
        )
        .unwrap();
        assert_eq!(filtered.total, 1, "时间戳为空串的块在带起始时间时应被排除");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 健壮性：单文件读失败跳过该文件，列表/关键字路径均不整体报错（对齐基线
    /// `Err(_) => continue`；此处用同名子目录模拟读失败——Windows 打开目录报
    /// Access denied，Unix read 目录报 EISDIR，两平台都走不到正文）
    #[test]
    fn proxy_logs_list_skips_unreadable_file() {
        let dir = tmp_dir("proxy_logs_skip");
        write_proxy_log(
            &dir,
            "proxy_req_2026-10-01.log",
            &[proxy_entry_text("2026-10-01 10:00:00", "GET", "a.example.com/x", "200 OK", "body-one")],
        );
        // 目录名匹配 proxy_req_*.log → 会进文件列表，但读内容必然失败
        std::fs::create_dir(dir.join("proxy_req_2026-10-02.log")).unwrap();

        let all = proxy_logs_list_impl(&dir, &proxy_opts(0, 50, None, None, None)).unwrap();
        assert_eq!(all.total, 1, "坏文件应被跳过而非整个列表失败");
        assert_eq!(all.entries[0].id, "proxy_req_2026-10-01.log:0");

        // 关键字路径同样跳过
        let kw = proxy_logs_list_impl(&dir, &proxy_opts(0, 50, Some("body"), None, None)).unwrap();
        assert_eq!(kw.total, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
