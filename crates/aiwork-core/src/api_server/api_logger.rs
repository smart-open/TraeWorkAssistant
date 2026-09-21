use std::collections::VecDeque;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{Datelike, Local};

/// API 请求日志记录器：按日期分文件，直接存储在 logs/ 目录下（文件名 api_YYYY-MM-DD.log）
///
/// 写入异步化（网关性能批次 B）：`log_request` / `log_sched_event` / `log_debug`
/// 仅将格式化后的行推入内存队列（µs 级、零磁盘 IO），由专用写入线程
/// 每 100ms（或被唤醒时）批量落盘。读路径（read_log/search_log/list_dates）
/// 先同步排空队列再读文件，保证「写后读」一致（测试与 UI 依赖此语义）。
pub struct ApiLogger {
    dir: PathBuf,
    /// 队列 + 文件句柄同锁互斥（写入线程与读路径排空共用一把锁，保证一致）
    state: Arc<Mutex<LoggerState>>,
    cv: Arc<Condvar>,
    /// 写入线程启动失败时降级为同步直写（极端场景兜底）
    sync_mode: bool,
}

struct LoggerState {
    /// 待落盘日志行（FIFO）
    queue: VecDeque<String>,
    /// 当前打开的日志文件句柄（None = 待打开）
    file: Option<std::fs::File>,
    /// 句柄对应日期（与今日不同则滚动换文件）
    date: String,
}

impl LoggerState {
    fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            file: None,
            date: String::new(),
        }
    }
}

impl ApiLogger {
    pub fn new(dir: PathBuf) -> Self {
        fs::create_dir_all(&dir).ok();
        let state = Arc::new(Mutex::new(LoggerState::new()));
        let cv = Arc::new(Condvar::new());
        let sync_mode = {
            let state2 = state.clone();
            let cv2 = cv.clone();
            let dir2 = dir.clone();
            std::thread::Builder::new()
                .name("api-logger".into())
                .spawn(move || flusher_loop(state2, cv2, dir2))
                .is_err()
        };
        Self { dir, state, cv, sync_mode }
    }

    fn today_filename() -> String {
        // 使用 chrono 本地时间获取精确日期
        let now = Local::now();
        format!("api_{:04}-{:02}-{:02}.log", now.year(), now.month(), now.day())
    }

    /// 入队一条日志（异步化热路径：仅内存操作，磁盘 IO 由写入线程承担）
    fn enqueue(&self, line: String) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.queue.push_back(line);
        if self.sync_mode {
            // 写入线程不可用：持锁直写（与读路径同锁，语义一致）
            write_locked(&mut st, &self.dir);
        } else {
            self.cv.notify_one();
        }
    }

    /// 读路径前同步排空队列（保证 read_log / search_log / list_dates 看到最新内容）
    fn flush_pending(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        write_locked(&mut st, &self.dir);
    }

    /// 公开排空（服务 stop / 应用退出时调用）：把队列中未落盘的行写盘
    pub fn flush(&self) {
        self.flush_pending();
    }

    /// 直写一行诊断日志（[DEBUG] 前缀诊断行）：走同一异步队列，
    /// 行尾无换行时自动补 `\n`（替代旧 get_writer 直写句柄方案）
    pub fn log_debug_line(&self, line: String) {
        let line = if line.ends_with('\n') { line } else { format!("{line}\n") };
        self.enqueue(line);
    }

    /// 记录一条 API 请求日志
    /// `pool`：资源标识（"trae" / "buddy"），区分该请求由哪个上游资源池服务
    pub fn log_request(
        &self,
        pool: &str,
        method: &str,
        path: &str,
        model: &str,
        stream: bool,
        status: u16,
        uid: &str,
        duration_ms: u64,
        error: Option<&str>,
    ) {
        self.log_request_inner(pool, method, path, model, stream, status, uid, duration_ms, None, error)
    }

    /// 记录一条 API 请求日志（流式请求带 TTFB 首字耗时字段）
    /// `ttfb_ms`：请求发起 → 上游首行到达的耗时；None 不输出该字段（旧格式兼容）
    pub fn log_request_ttfb(
        &self,
        pool: &str,
        method: &str,
        path: &str,
        model: &str,
        stream: bool,
        status: u16,
        uid: &str,
        duration_ms: u64,
        ttfb_ms: Option<u64>,
        error: Option<&str>,
    ) {
        self.log_request_inner(pool, method, path, model, stream, status, uid, duration_ms, ttfb_ms, error)
    }

    fn log_request_inner(
        &self,
        pool: &str,
        method: &str,
        path: &str,
        model: &str,
        stream: bool,
        status: u16,
        uid: &str,
        duration_ms: u64,
        ttfb_ms: Option<u64>,
        error: Option<&str>,
    ) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let local_ts = now + 8 * 3600;
        let h = (local_ts % 86400) / 3600;
        let m = (local_ts % 3600) / 60;
        let s = local_ts % 60;

        let uid_short = &uid[..uid.len().min(12)];
        let ttfb_part = match ttfb_ms {
            Some(t) => format!(" ttfb={t}ms"),
            None => String::new(),
        };
        let err_part = match error {
            Some(e) => format!(" error={}", e),
            None => String::new(),
        };

        let line = format!(
            "[{:02}:{:02}:{:02}] {} {} pool={} model={} stream={} status={} uid={} {}ms{}{}\n",
            h, m, s,
            method, path, pool, model, stream, status, uid_short, duration_ms, ttfb_part, err_part,
        );

        self.enqueue(line);
    }

    /// 记录调度事件日志（F-77）：sticky_yield 让位 / busy 降级取号 / 对冲接管等，
    /// 单行 `[SCHED]` 前缀写入当日日志，供用量页日志检索与调度行为观测
    pub fn log_sched_event(&self, event: &str) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let local_ts = now + 8 * 3600;
        let h = (local_ts % 86400) / 3600;
        let m = (local_ts % 3600) / 60;
        let s = local_ts % 60;
        let line = format!("[{:02}:{:02}:{:02}] [SCHED] {}\n", h, m, s, event);
        self.enqueue(line);
    }

    /// 记录 Debug 级别的完整请求/响应日志
    pub fn log_debug(
        &self,
        uid: &str,
        req_body: &[u8],
        resp_body: Option<&[u8]>,
        status: u16,
        error: Option<&str>,
    ) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let local_ts = now + 8 * 3600;
        let h = (local_ts % 86400) / 3600;
        let m = (local_ts % 3600) / 60;
        let s = local_ts % 60;

        let uid_short = &uid[..uid.len().min(12)];
        let mut lines = Vec::new();
        lines.push(format!(
            "[{:02}:{:02}:{:02}] [DEBUG] uid={} status={}",
            h, m, s, uid_short, status,
        ));

        // 请求体（截取前 4KB 防止日志爆炸）
        let req_preview = if req_body.len() > 4096 {
            &req_body[..4096]
        } else {
            req_body
        };
        let req_str = String::from_utf8_lossy(req_preview);
        lines.push(format!("--- Request Body ({} bytes, preview {}B) ---", req_body.len(), req_preview.len()));
        lines.push(req_str.to_string());

        // 响应体（截取前 8KB）
        if let Some(resp) = resp_body {
            let resp_preview = if resp.len() > 8192 {
                &resp[..8192]
            } else {
                resp
            };
            let resp_str = String::from_utf8_lossy(resp_preview);
            lines.push(format!("--- Response Body ({} bytes, preview {}B) ---", resp.len(), resp_preview.len()));
            lines.push(resp_str.to_string());
        }

        if let Some(e) = error {
            lines.push(format!("--- Error: {} ---", e));
        }

        lines.push(String::new()); // 空行分隔

        let data = lines.join("\n");
        self.enqueue(data);
    }

    /// 读取指定日期的日志文件内容（按时间倒序排列）
    pub fn read_log(&self, date: &str) -> Option<String> {
        self.flush_pending();
        // date 格式: "2026-08-14"
        let path = self.dir.join(format!("api_{}.log", date));
        match fs::read(&path) {
            Ok(bytes) => {
                let content = String::from_utf8_lossy(&bytes).to_string();
                Some(reverse_log_blocks(&content))
            }
            Err(_) => None,
        }
    }

    /// 按时间段和关键字搜索日志
    ///
    /// - `date`: 日期，格式 "2026-08-14"
    /// - `start_time`: 起始时间，格式 "HH:MM:SS" 或 "HH:MM"，为空则不限
    /// - `end_time`: 结束时间，同上
    /// - `keyword`: 关键字（不区分大小写），为空则不限
    ///
    /// 返回过滤后的日志文本。普通日志条目按行过滤；
    /// Debug 块（多行）只要任一行命中关键字则整块保留。
    pub fn search_log(
        &self,
        date: &str,
        start_time: &str,
        end_time: &str,
        keyword: &str,
    ) -> Option<String> {
        let content = self.read_log(date)?;

        // 预处理时间边界（补全为 HH:MM:SS 格式）
        let start = normalize_time(start_time);
        let end = normalize_time(end_time);
        let kw_lower = keyword.to_lowercase();
        let has_kw = !kw_lower.is_empty();
        let has_time = !start.is_empty() || !end.is_empty();

        if !has_kw && !has_time {
            return Some(content);
        }

        let mut result: Vec<String> = Vec::new();
        let lines: Vec<&str> = content.lines().collect();
        let mut i = 0;

        while i < lines.len() {
            let line = lines[i];

            // 检测 Debug 块：以 [HH:MM:SS] [DEBUG] 开头
            if line.contains("[DEBUG]") && line.starts_with('[') {
                // 收集整个 Debug 块（直到下一个 [ 开头的行或空行后）
                let mut block: Vec<&str> = vec![line];
                let block_time = extract_time(line);
                let mut j = i + 1;
                while j < lines.len() {
                    let next = lines[j];
                    // 遇到新日志条目（以 [HH:MM:SS] 开头且非 --- 开头）则停止
                    if next.starts_with('[') && extract_time(next).is_some() {
                        break;
                    }
                    block.push(next);
                    j += 1;
                }

                // 时间过滤：用块首行的时间
                let time_ok = if has_time {
                    is_time_in_range(&block_time, &start, &end)
                } else {
                    true
                };

                // 关键字过滤：块中任一行命中即可
                let kw_ok = if has_kw {
                    block.iter().any(|l| l.to_lowercase().contains(&kw_lower))
                } else {
                    true
                };

                if time_ok && kw_ok {
                    result.push(block.join("\n"));
                }

                i = j;
            } else {
                // 普通单行日志
                let time = extract_time(line);

                let time_ok = if has_time {
                    is_time_in_range(&time, &start, &end)
                } else {
                    true
                };

                let kw_ok = if has_kw {
                    line.to_lowercase().contains(&kw_lower)
                } else {
                    true
                };

                if time_ok && kw_ok {
                    result.push(line.to_string());
                }

                i += 1;
            }
        }

        if result.is_empty() {
            Some("（无匹配的日志条目）".to_string())
        } else {
            // 倒序排列（最新的在最前面）
            result.reverse();
            Some(result.join("\n"))
        }
    }

    /// 列出所有可用日志日期（最近 N 天）
    pub fn list_dates(&self, max: usize) -> Vec<String> {
        self.flush_pending();
        let mut dates: Vec<String> = Vec::new();
        if let Ok(entries) = fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                // 匹配 api_YYYY-MM-DD.log
                if name.starts_with("api_") && name.ends_with(".log") {
                    let date = &name[4..name.len() - 4];
                    dates.push(date.to_string());
                }
            }
        }
        dates.sort();
        dates.reverse();
        dates.truncate(max);
        dates
    }
}

// ==================== 写入线程与批量落盘 ====================

/// 写入线程主循环：队列空则等条件变量（100ms 超时兜底），非空则持锁批量落盘。
/// 批量写期间日志入队方会短暂等锁（一批几行的 write_all，µs~ms 级），
/// 相比旧实现每次调用 metadata + try_clone + write 三次系统调用大幅缩短临界区。
fn flusher_loop(state: Arc<Mutex<LoggerState>>, cv: Arc<Condvar>, dir: PathBuf) {
    loop {
        let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
        if st.queue.is_empty() {
            let (guard, _) = cv
                .wait_timeout(st, Duration::from_millis(100))
                .unwrap_or_else(|e| e.into_inner());
            st = guard;
        }
        // 持锁批量写（与入队/读路径同锁；锁内不等待，仅执行 write_all）
        write_locked(&mut st, &dir);
    }
}

/// 持锁落盘：排空队列并写入当前日期文件（句柄跨调用缓存，日期滚动才重开）。
/// 写失败（句柄失效等）置 None，下次调用重开新句柄。
fn write_locked(st: &mut LoggerState, dir: &PathBuf) {
    if st.queue.is_empty() {
        return;
    }
    let today = ApiLogger::today_filename();
    if st.file.is_none() || st.date != today {
        let path = dir.join(&today);
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        match fs::OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => {
                st.file = Some(f);
                st.date = today;
            }
            // 打开失败：丢弃本批（与旧实现 write 失败静默语义一致），保留队列外后续行
            Err(_) => {
                st.queue.clear();
                st.date = today;
                return;
            }
        }
    }
    let Some(f) = st.file.as_mut() else { return };
    while let Some(line) = st.queue.pop_front() {
        if f.write_all(line.as_bytes()).is_err() {
            st.file = None; // 句柄失效，下次重开
            return;
        }
    }
}

// ==================== 辅助函数 ====================

/// 从日志行中提取时间部分 "HH:MM:SS"
/// 日志行格式: `[HH:MM:SS] ...` 或 `[HH:MM:SS] [DEBUG] ...`
fn extract_time(line: &str) -> Option<String> {
    if !line.starts_with('[') {
        return None;
    }
    // 提取第一个 ] 之前的内容（不含 [ ）
    let close = line.find(']')?;
    let inner = &line[1..close];
    // 验证格式 HH:MM:SS
    let parts: Vec<&str> = inner.split(':').collect();
    if parts.len() == 3 {
        let h = parts[0].parse::<u32>().ok()?;
        let m = parts[1].parse::<u32>().ok()?;
        let s = parts[2].parse::<u32>().ok()?;
        if h < 24 && m < 60 && s < 60 {
            return Some(format!("{:02}:{:02}:{:02}", h, m, s));
        }
    }
    // 也支持 HH:MM 格式（不含秒）
    if parts.len() == 2 {
        let h = parts[0].parse::<u32>().ok()?;
        let m = parts[1].parse::<u32>().ok()?;
        if h < 24 && m < 60 {
            return Some(format!("{:02}:{:02}:00", h, m));
        }
    }
    None
}

/// 将时间字符串标准化为 "HH:MM:SS" 格式
/// 输入可以是 "HH:MM" 或 "HH:MM:SS"
fn normalize_time(t: &str) -> String {
    let t = t.trim();
    if t.is_empty() {
        return String::new();
    }
    let parts: Vec<&str> = t.split(':').collect();
    match parts.len() {
        2 => {
            let h = parts[0].parse::<u32>().unwrap_or(0);
            let m = parts[1].parse::<u32>().unwrap_or(0);
            format!("{:02}:{:02}:00", h, m)
        }
        3 => {
            let h = parts[0].parse::<u32>().unwrap_or(0);
            let m = parts[1].parse::<u32>().unwrap_or(0);
            let s = parts[2].parse::<u32>().unwrap_or(0);
            format!("{:02}:{:02}:{:02}", h, m, s)
        }
        _ => String::new(),
    }
}

/// 判断时间是否在 [start, end] 范围内
/// start/end 为 "HH:MM:SS" 格式，为空表示不限
fn is_time_in_range(time: &Option<String>, start: &str, end: &str) -> bool {
    let Some(t) = time else {
        // 无法解析时间的行（如空行、分隔线），放行
        return true;
    };
    if !start.is_empty() && t.as_str() < start {
        return false;
    }
    if !end.is_empty() && t.as_str() > end {
        return false;
    }
    true
}

/// 将日志内容按块倒序排列（最新的条目在最前面）
/// 普通单行日志以一个块处理，Debug 多行块（以 [DEBUG] 开头的条目及其后续行）作为整体处理
fn reverse_log_blocks(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let mut blocks: Vec<String> = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];

        // Debug 块：以 [ 开头且包含 [DEBUG]
        if line.contains("[DEBUG]") && line.starts_with('[') {
            let mut block: Vec<&str> = vec![line];
            let mut j = i + 1;
            while j < lines.len() {
                let next = lines[j];
                // 遇到新日志条目（以 [HH:MM:SS] 开头）则停止
                if next.starts_with('[') && extract_time(next).is_some() {
                    break;
                }
                block.push(next);
                j += 1;
            }
            blocks.push(block.join("\n"));
            i = j;
        } else if line.starts_with('[') && extract_time(line).is_some() {
            // 普通单行日志
            blocks.push(line.to_string());
            i += 1;
        } else {
            // 无法解析的行（空行等），合并到前一个块
            if let Some(last) = blocks.last_mut() {
                last.push('\n');
                last.push_str(line);
            } else {
                blocks.push(line.to_string());
            }
            i += 1;
        }
    }

    blocks.reverse();
    blocks.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn today() -> String {
        let now = Local::now();
        format!("{:04}-{:02}-{:02}", now.year(), now.month(), now.day())
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("api_logger_test_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn log_line_contains_pool_and_model() {
        let dir = tmp_dir("pool");
        let logger = ApiLogger::new(dir.clone());
        logger.log_request(
            "buddy", "POST", "/v1/chat/completions", "glm-5.3", true, 200,
            "user-1234567890abcdef", 42, None,
        );
        logger.log_request(
            "trae", "POST", "/v1/chat/completions", "glm-5.2", false, 503,
            "none", 8, Some("no healthy account"),
        );
        let content = logger.read_log(&today()).expect("log written");
        assert!(content.contains("pool=buddy model=glm-5.3"), "buddy 行需含资源标识与模型: {content}");
        assert!(content.contains("pool=trae model=glm-5.2"), "trae 行需含资源标识与模型: {content}");
        assert!(content.contains("error=no healthy account"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sched_event_line_format() {
        let dir = tmp_dir("sched");
        let logger = ApiLogger::new(dir.clone());
        logger.log_sched_event("sticky_yield uid=abc inflight=1 limit=1");
        let content = logger.read_log(&today()).expect("log written");
        assert!(content.contains("[SCHED] sticky_yield uid=abc inflight=1 limit=1"));
        let _ = fs::remove_dir_all(&dir);
    }
}
