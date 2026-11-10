use std::collections::VecDeque;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use chrono::{Datelike, Local, Timelike};

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

/// 队列上限（M-5 背压）：慢盘场景防内存无界增长，超限丢最旧行
const MAX_QUEUE_LINES: usize = 10_000;

struct LoggerState {
    /// 待落盘日志行（FIFO）
    queue: VecDeque<String>,
    /// 当前打开的日志文件句柄（None = 待打开）
    file: Option<std::fs::File>,
    /// 句柄对应日期（与今日不同则滚动换文件）
    date: String,
    /// 队列溢出一次性观测标记（首次丢最旧行时置位，防 stderr 刷屏）
    overflow_noted: bool,
}

impl LoggerState {
    fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            file: None,
            date: String::new(),
            overflow_noted: false,
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

    /// 查询/列表专用实例（审查 P4 修复）：不 spawn flusher 线程——若沿用 `new`，
    /// 每次调用都会泄漏一个永不退出的常驻线程（flusher_loop 持 Arc 死循环，
    /// api_logs_recent 分页翻页会持续放大泄漏）。查询实例自身无写入、队列恒空，
    /// flush_pending → write_locked 首行判空即返回，真空转无副作用；sync_mode=true
    /// 兜底确保万一有写入也是持锁直写，不会滞留在无 flusher 的队列。读取内容为
    /// 运行时实例已落盘部分（最多滞后一个 100ms 批量落盘周期）。
    pub fn for_query(dir: PathBuf) -> Self {
        fs::create_dir_all(&dir).ok();
        Self {
            dir,
            state: Arc::new(Mutex::new(LoggerState::new())),
            cv: Arc::new(Condvar::new()),
            sync_mode: true,
        }
    }

    fn today_filename() -> String {
        // 使用 chrono 本地时间获取精确日期
        let now = Local::now();
        format!("api_{:04}-{:02}-{:02}.log", now.year(), now.month(), now.day())
    }

    /// 审查加固（docker 本地审查）：外部传入的 date 参数仅接受 `YYYY-MM-DD`
    /// 字面格式（纯数字 + 两个连字符）。所有 `dir.join(format!("api_{date}.log"))`
    /// 形态的路径拼接以此为前置闸，阻断 `../` 路径穿越读取任意 .log 文件
    fn valid_date(date: &str) -> bool {
        let b = date.as_bytes();
        b.len() == 10
            && b.iter().enumerate().all(|(i, &c)| {
                if i == 4 || i == 7 {
                    c == b'-'
                } else {
                    c.is_ascii_digit()
                }
            })
    }

    /// 行内时间戳 HH:MM:SS，取本地时区（与文件名 today_filename 同源）。
    /// 审查修复：原实现 epoch+8h 手算固定东八区，非东八区环境跨日与文件名错位
    fn local_hms() -> (u32, u32, u32) {
        let t = Local::now().time();
        (t.hour(), t.minute(), t.second())
    }

    /// 入队一条日志（异步化热路径：仅内存操作，磁盘 IO 由写入线程承担）
    fn enqueue(&self, line: String) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.queue.push_back(line);
        // M-5：队列上限背压——超限丢最旧行，防慢盘场景无界内存增长；
        // 首次触发 stderr 观测一次（不落 app_log，防递归噪音）
        if st.queue.len() > MAX_QUEUE_LINES {
            st.queue.pop_front();
            if !st.overflow_noted {
                st.overflow_noted = true;
                eprintln!("[api_logger] 日志队列溢出（>{MAX_QUEUE_LINES} 行），丢弃最旧行（慢盘/杀毒扫描？）");
            }
        }
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
        // B-7/M-3：正文段清洗防锚字面量误锚；保留行首 [DEBUG] 结构前缀
        //（整行清洗会破坏 search_log/reverse_log_blocks 的块检测）
        let line = match line.strip_prefix("[DEBUG] ") {
            Some(body) => format!("[DEBUG] {}", sanitize_log_value(body)),
            None => sanitize_log_value(&line),
        };
        let line = if line.ends_with('\n') { line } else { format!("{line}\n") };
        self.enqueue(line);
    }

    /// 记录一条 API 请求日志
    /// `pool`：资源标识（"trae" / "buddy"），区分该请求由哪个上游资源池服务
    /// `key_name`：请求所用 API Key 展示名（空 → "-"）
    /// `acct_name`：实际服务的资源池账号名称（空 → "-"）
    #[allow(clippy::too_many_arguments)]
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
        key_name: &str,
        acct_name: &str,
        error: Option<&str>,
    ) {
        self.log_request_inner(pool, method, path, model, stream, status, uid, duration_ms, None, key_name, acct_name, error, None)
    }

    /// 记录一条 API 请求日志（流式请求带 TTFB 首字耗时字段）
    /// `ttfb_ms`：请求发起 → 上游首行到达的耗时；None 不输出该字段（旧格式兼容）
    #[allow(clippy::too_many_arguments)]
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
        key_name: &str,
        acct_name: &str,
        error: Option<&str>,
    ) {
        self.log_request_inner(pool, method, path, model, stream, status, uid, duration_ms, ttfb_ms, key_name, acct_name, error, None)
    }

    /// 记录一条 API 请求日志（附请求阶段字段，issue #149 P1，custom 池用）：
    /// `stage`：请求阶段标签（main_first / main / tool / unknown），None 不输出该字段
    #[allow(clippy::too_many_arguments)]
    pub fn log_request_stage(
        &self,
        pool: &str,
        method: &str,
        path: &str,
        model: &str,
        stream: bool,
        status: u16,
        uid: &str,
        duration_ms: u64,
        key_name: &str,
        acct_name: &str,
        stage: Option<&str>,
        error: Option<&str>,
    ) {
        self.log_request_inner(pool, method, path, model, stream, status, uid, duration_ms, None, key_name, acct_name, error, stage)
    }

    #[allow(clippy::too_many_arguments)]
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
        key_name: &str,
        acct_name: &str,
        error: Option<&str>,
        stage: Option<&str>,
    ) {
        let (h, m, s) = Self::local_hms();

        // B-7/M-3：落行前清洗用户可控值（key 名/账号名/模型名/错误消息/uid）——
        // 值内含解析锚字面量（" key=" 等 / [SCHED]）会导致 parse_log_line 字段
        // 错位或整行被误排除；ZWSP 破坏锚匹配，读侧无需改动。
        // method/path/结构化数字字段（status/duration/ttfb/stage）不清洗
        let uid_short = sanitize_log_value(&uid[..uid.len().min(12)]);
        let ttfb_part = match ttfb_ms {
            Some(t) => format!(" ttfb={t}ms"),
            None => String::new(),
        };
        let stage_part = match stage {
            Some(s) => format!(" stage={s}"),
            None => String::new(),
        };
        let err_part = match error {
            Some(e) => format!(" error={}", sanitize_log_value(e)),
            None => String::new(),
        };
        // 空名称（匿名 Key / 无可用账号）统一显示 "-"，保持字段可检索
        let key_disp = if key_name.is_empty() { "-".to_string() } else { sanitize_log_value(key_name) };
        let acct_disp = if acct_name.is_empty() { "-".to_string() } else { sanitize_log_value(acct_name) };
        let model = sanitize_log_value(model);

        let line = format!(
            "[{:02}:{:02}:{:02}] {} {} key={} pool={} model={} acct={} uid={} stream={} status={} {}ms{}{}{}\n",
            h, m, s,
            method, path, key_disp, pool, model, acct_disp, uid_short, stream, status, duration_ms, ttfb_part, stage_part, err_part,
        );

        self.enqueue(line);
    }

    /// 记录调度事件日志（F-77）：sticky_yield 让位 / busy 降级取号 / 对冲接管等，
    /// 单行 `[SCHED]` 前缀写入当日日志，供用量页日志检索与调度行为观测
    pub fn log_sched_event(&self, event: &str) {
        let (h, m, s) = Self::local_hms();
        // B-7/M-3：message 含 [SCHED]/字段锚字面量时清洗，防行级锚 contains 误匹配
        let line = format!(
            "[{:02}:{:02}:{:02}] [SCHED] {}\n",
            h, m, s, sanitize_log_value(event)
        );
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
        let (h, m, s) = Self::local_hms();

        // B-7/M-3：uid 截断同样清洗；正文段（请求/响应体）为用户可控内容，
        // 清洗防正文行被误解析为请求条目
        let uid_short = sanitize_log_value(&uid[..uid.len().min(12)]);
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
        lines.push(sanitize_log_value(&req_str));

        // 响应体（截取前 8KB）
        if let Some(resp) = resp_body {
            let resp_preview = if resp.len() > 8192 {
                &resp[..8192]
            } else {
                resp
            };
            let resp_str = String::from_utf8_lossy(resp_preview);
            lines.push(format!("--- Response Body ({} bytes, preview {}B) ---", resp.len(), resp_preview.len()));
            lines.push(sanitize_log_value(&resp_str));
        }

        if let Some(e) = error {
            lines.push(format!("--- Error: {} ---", sanitize_log_value(e)));
        }

        lines.push(String::new()); // 空行分隔

        let data = lines.join("\n");
        self.enqueue(data);
    }

    /// 读取指定日期的日志文件内容（按时间倒序排列）
    pub fn read_log(&self, date: &str) -> Option<String> {
        // 审查加固（docker 本地审查）：date 仅接受 YYYY-MM-DD——该参数经命令桥
        // 直达此处，原实现 `api_{date}.log` 直接拼路径，`../../x` 形态输入可
        // 穿越 logs 目录读取任意 .log 后缀文件（认证后信息泄露面）
        if !Self::valid_date(date) {
            return None;
        }
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
                    // 与 read_log 的 valid_date 同口径（afe1966 安全收口配套）：畸形文件名
                    //（如手动备份 api_备份.log）不入列表——否则前端出现可点但恒无内容
                    // 的死条目，非 ASCII 名倒序后还会抢占默认选中
                    if Self::valid_date(date) {
                        dates.push(date.to_string());
                    }
                }
            }
        }
        dates.sort();
        dates.reverse();
        dates.truncate(max);
        dates
    }

    /// F-84：结构化「最近请求」查询——解析最近 N 天请求行为条目（新→旧），分页 + 池/结果筛选。
    /// 复用文本日志为唯一事实源（零格式变更、零 DDL）；SCHED/DEBUG 块与不可解析行跳过。
    pub fn recent_entries(
        &self,
        days: u32,
        page: u32,
        page_size: u32,
        pool: Option<&str>,
        result: Option<&str>,
    ) -> ApiLogRecentResult {
        // B-10/L-4：result 未知值前置校验（零 IO，先于 flush/扫描）——拼写错误
        // 不再静默放行显示全量，直接返回空结果
        if let Some(r) = result {
            if r != "ok" && r != "fail" {
                return ApiLogRecentResult { rows: Vec::new(), total: 0, truncated: false };
            }
        }
        self.flush_pending();
        // B-9：自然日窗口——按日期过滤而非「最近 N 个文件」；应用停用一段时间后
        // days=3 不再返回 40 天前的旧数据（YYYY-MM-DD 字典序即时间序）
        let today = Local::now().date_naive();
        let cutoff = today - chrono::Duration::days(days.clamp(1, 30) as i64 - 1);
        let cutoff_str = cutoff.format("%Y-%m-%d").to_string();
        let dates: Vec<String> = self
            .list_dates(30)
            .into_iter()
            .filter(|d| d.as_str() >= cutoff_str.as_str())
            .collect();
        let mut all: Vec<ApiLogEntry> = Vec::new();
        let mut truncated = false;
        'outer: for d in &dates {
            // M-4：read_log 全量读入前按字节预检（date 来自 list_dates 必合法，
            // 与 read_log 同款 valid_date 口径），超限文件跳过并标记截断，
            // 防单文件一次性读入造成内存放大
            let path = self.dir.join(format!("api_{d}.log"));
            if let Ok(meta) = fs::metadata(&path) {
                if meta.len() > MAX_RECENT_SCAN_BYTES {
                    truncated = true;
                    continue;
                }
            }
            let Some(content) = self.read_log(d) else { continue };
            for line in content.lines() {
                if all.len() >= MAX_RECENT_SCAN_LINES {
                    truncated = true;
                    break 'outer;
                }
                if let Some(e) = parse_log_line(line, d) {
                    all.push(e);
                }
            }
        }
        // 筛选：pool 精确匹配；result "ok"=2xx / "fail"=其余
        let filtered: Vec<ApiLogEntry> = all
            .into_iter()
            .filter(|e| {
                if let Some(p) = pool {
                    if e.pool != p {
                        return false;
                    }
                }
                match result {
                    Some("ok") => (200..300).contains(&e.status),
                    Some("fail") => !(200..300).contains(&e.status),
                    _ => true,
                }
            })
            .collect();
        let total = filtered.len();
        let page_size = page_size.clamp(1, 200) as usize;
        let page = page.max(1) as usize;
        let start = (page - 1).saturating_mul(page_size);
        let rows = filtered.into_iter().skip(start).take(page_size).collect();
        ApiLogRecentResult { rows, total, truncated }
    }
}

// ==================== F-84 结构化请求条目 ====================

/// 扫描双上限（B-8/M-4，替代旧「单日解析行数保护」）：全局扫描行数上限 +
/// 单文件字节预检上限，防超大日志拖慢查询；行数达上限或单文件超字节上限
/// 均 truncated=true（total 为已扫部分计数）
const MAX_RECENT_SCAN_LINES: usize = 800_000;
const MAX_RECENT_SCAN_BYTES: u64 = 256 * 1024 * 1024;

/// F-84：网关单条请求的结构化视图（由当日文本日志行解析，最新在前分页返回）
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiLogEntry {
    /// 日志文件日期（YYYY-MM-DD）
    pub date: String,
    /// 行首时间（HH:MM:SS）
    pub time: String,
    pub method: String,
    pub path: String,
    /// 网关 API Key 展示名（匿名 = "-"）
    pub key_name: String,
    /// 实际服务池（trae/buddy/buddy-ai/qoder/custom；调度早退行 = "-"）
    pub pool: String,
    pub model: String,
    /// 实际服务账号名（未取号 = "-"）
    pub acct: String,
    /// 上游账号 uid 前 12 位
    pub uid: String,
    pub stream: bool,
    pub status: u16,
    pub duration_ms: u64,
    /// TTFT（首字延迟，仅流式成功路径记录）
    pub ttfb_ms: Option<u64>,
    /// custom 池阶段标记
    pub stage: Option<String>,
    /// 失败原因（既有管线脱敏口径，原样透出）
    pub error: Option<String>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiLogRecentResult {
    pub rows: Vec<ApiLogEntry>,
    pub total: usize,
    /// true = 触发扫描双上限之一（行数/字节），total 仅统计已扫部分
    pub truncated: bool,
}

/// 行内已知 `字段=` token（按落行顺序；key/acct/model 值可含空格，靠相邻 token 定界）
const PARSE_TOKENS: [&str; 8] = [
    " key=", " pool=", " model=", " acct=", " uid=", " stream=", " status=", " error=",
];

/// 全量解析/行级锚（B-7/M-3 清洗依据）：PARSE_TOKENS 的 8 个空格前缀字段锚
/// + ttfb/stage 2 个空格前缀字段锚 + [DEBUG]/[SCHED] 2 个行级块锚（无前缀空格）
const LOG_PARSE_TOKENS: [&str; 12] = [
    " key=", " pool=", " model=", " acct=", " uid=", " stream=", " status=", " error=",
    " ttfb=", " stage=", "[DEBUG]", "[SCHED]",
];

/// 清洗用户可控值（key 名/账号名/模型名/错误消息/uid/日志正文）中与解析锚冲突的
/// 字面量（B-7/M-3）：空格前缀锚（10 个）把命中处的首空格替换为 U+200B 零宽空格
///（" key=" → "\u{200B}key="）；行级锚 "[DEBUG]"/"[SCHED]" 在 "[" 之后插 ZWSP
///（"[\u{200B}DEBUG]"），保留 [ 完整形态。写侧清洗后读侧 parse_log_line 不再
/// 误锚/误排除，故 parse_log_line 本体无需改动。
/// pub(crate)：auth::log_reject 的 401 拒绝行 path 取自 request.uri().path()，
/// 为请求行中唯一客户端可控且 logger 侧不清洗的字段，须在调用点按同口径清洗。
pub(crate) fn sanitize_log_value(v: &str) -> String {
    let mut s = v.to_string();
    for tok in LOG_PARSE_TOKENS.iter() {
        // 循环处理多次出现；替换后从 ZWSP 之后重新 find，避免死循环
        let mut search_from = 0;
        while let Some(rel) = s[search_from..].find(tok) {
            let pos = search_from + rel;
            // 空格前缀锚：首空格（1 字节）替换为 ZWSP；行级锚：在 "[" 后插入 ZWSP
            let (start, end) = if tok.starts_with(' ') {
                (pos, pos + 1)
            } else {
                (pos + 1, pos + 1)
            };
            s.replace_range(start..end, "\u{200B}");
            search_from = start + 3; // ZWSP 占 3 字节（U+200B = E2 80 8B）
        }
    }
    s
}

/// F-84：解析单行请求日志为结构化条目。非请求行（[SCHED]/[DEBUG] 块正文/不可解析）返回 None。
/// 宽容语义：任一必备字段缺失或类型不符 → None（跳过该行，不中断）。
pub fn parse_log_line(line: &str, date: &str) -> Option<ApiLogEntry> {
    let t = line.trim();
    if !t.starts_with('[') || t.contains("[SCHED]") || t.contains("[DEBUG]") {
        return None;
    }
    // [HH:MM:SS] 前缀
    let close = t.find(']')?;
    let time = &t[1..close];
    if time.len() != 8 || time.as_bytes()[2] != b':' || time.as_bytes()[5] != b':' {
        return None;
    }
    if !time.bytes().all(|b| b.is_ascii_digit() || b == b':') {
        return None;
    }
    let rest = t[close + 1..].trim_start();

    // 定位各 token 位置（首次出现；error= 之后的文本可能含其他 token 字样，
    // 因此先找 error= 的位置并将其后的 token 搜索截止）
    let err_tok = rest.find(" error=");
    let search_end = err_tok.unwrap_or(rest.len());
    let mut positions: Vec<(usize, usize)> = Vec::new(); // (token 序号, 字节位置)
    for (i, tok) in PARSE_TOKENS.iter().enumerate() {
        if *tok == " error=" {
            continue;
        }
        let hay = &rest[..search_end];
        let pos = hay.find(tok)?;
        positions.push((i, pos));
    }
    positions.sort_by_key(|(_, p)| *p);
    if positions.len() != 7 {
        return None;
    }

    // 首段 = "METHOD /path"
    let first_tok_pos = positions[0].1;
    let head = rest[..first_tok_pos].trim();
    let (method, path) = head.split_once(' ')?;
    if method.is_empty() || !path.starts_with('/') {
        return None;
    }

    // 逐 token 提值：值域 = 本 token 值起点 → 下一 token 位置（error 取到行尾）
    let get_value = |idx: usize| -> Option<String> {
        let (tok_i, pos) = positions.iter().find(|(i, _)| *i == idx)?;
        let vs = pos + PARSE_TOKENS[*tok_i].len();
        let ve = positions
            .iter()
            .filter(|(_, p)| *p > *pos)
            .map(|(_, p)| *p)
            .min()
            .unwrap_or(search_end);
        Some(rest[vs..ve].trim().to_string())
    };

    let key_name = get_value(0)?;
    let pool = get_value(1)?;
    let model = get_value(2)?;
    let acct = get_value(3)?;
    let uid = get_value(4)?;
    let stream = match get_value(5)?.as_str() {
        "true" => true,
        "false" => false,
        _ => return None,
    };
    // status 段原始值域为 "200 43765ms"（duration 紧随 status，均以空格分隔）
    let status_pos = positions.iter().find(|(i, _)| *i == 6)?.1;
    let tail_raw = &rest[status_pos + PARSE_TOKENS[6].len()..search_end];
    let mut seg_parts = tail_raw.split_whitespace();
    let status: u16 = seg_parts.next()?.parse().ok()?;
    let duration_ms: u64 = seg_parts
        .find_map(|p| p.strip_suffix("ms").and_then(|n| n.parse().ok()))?;
    // error 取到行尾（可能含空格与 token 字样）
    let error = err_tok.map(|p| rest[p + " error=".len()..].trim().to_string());
    // ttfb/stage 在 status 段之后的剩余文本里找
    let ttfb_ms = tail_raw
        .split(" ttfb=")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.strip_suffix("ms"))
        .and_then(|n| n.parse().ok());
    let stage = tail_raw
        .split(" stage=")
        .nth(1)
        .map(|s| s.split_whitespace().next().unwrap_or("").to_string())
        .filter(|s| !s.is_empty());

    Some(ApiLogEntry {
        date: date.to_string(),
        time: time.to_string(),
        method: method.to_string(),
        path: path.to_string(),
        key_name,
        pool,
        model,
        acct,
        uid,
        stream,
        status,
        duration_ms,
        ttfb_ms,
        stage,
        error,
    })
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
            Err(e) => {
                // 审查修复（L4）：打开失败不再无痕丢整批——stderr + app_log 落一条
                // 观测行（含丢弃行数与目标路径），防磁盘满/权限故障期间日志静默丢失
                let msg = format!(
                    "[api_logger] 打开日志文件失败，丢弃本批 {} 行（path={}）: {e}",
                    st.queue.len(),
                    path.display()
                );
                eprintln!("{msg}");
                if let Some(data_dir) = dir.parent() {
                    crate::fs_utils::app_log(data_dir, &msg);
                }
                st.queue.clear();
                st.date = today;
                return;
            }
        }
    }
    let Some(f) = st.file.as_mut() else { return };
    while let Some(line) = st.queue.pop_front() {
        if let Err(e) = f.write_all(line.as_bytes()) {
            // 审查修复（L-3）：写失败不再无痕置空句柄——stderr 观测一次（含
            // io error 与剩余队列行数）；不落 app_log（防递归噪音，app_log
            // 写入路径可能再触发本 logger）。保留「句柄失效，下次重开」逻辑
            eprintln!(
                "[api_logger] 写入日志失败: {e}，句柄失效待重开，剩余队列 {} 行",
                st.queue.len()
            );
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

    /// 审查加固回归：read_log 对非 `YYYY-MM-DD` 的 date 一律 None——
    /// `../../x` 形态输入不得作为路径段拼接（防穿越读任意 .log 文件）
    #[test]
    fn read_log_rejects_non_date_input() {
        let dir = tmp_dir("traversal");
        let logger = ApiLogger::new(dir.clone());
        logger.log_request("buddy", "POST", "/v1/x", "m", false, 200, "u", 1, "k", "a", None);
        logger.flush_pending();
        // 合法格式：正常读到内容
        assert!(logger.read_log(&today()).is_some());
        // 穿越/畸形格式：一律 None（无论目标文件是否存在）
        assert_eq!(logger.read_log("../../secret"), None);
        assert_eq!(logger.read_log(&format!("../../{}", today())), None);
        assert_eq!(logger.read_log("2026-1-1"), None);
        assert_eq!(logger.read_log("20260101"), None);
        assert_eq!(logger.read_log(""), None);
    }

    /// 安全收口配套（afe1966）：list_dates 与 read_log 校验同口径——畸形文件名不入日期列表
    #[test]
    fn list_dates_filters_malformed_filenames() {
        let dir = tmp_dir("dates");
        let logger = ApiLogger::new(dir.clone());
        logger.log_request("buddy", "POST", "/v1/x", "m", false, 200, "u", 1, "k", "a", None);
        logger.flush_pending();
        // 模拟外来畸形文件混入日志目录（用户手动备份等）
        std::fs::write(dir.join("api_备份.log"), "x").unwrap();
        std::fs::write(dir.join("api_20260814.log"), "x").unwrap();
        let dates = logger.list_dates(30);
        assert!(dates.contains(&today()), "合法日期应在列表: {dates:?}");
        assert!(dates.iter().all(|d| ApiLogger::valid_date(d)), "列表不得混入畸形日期: {dates:?}");
    }

    /// F-84：完整成功行解析（含 TTFT、中文 Key/账号名、uid 截断）
    #[test]
    fn parse_log_line_success_with_ttfb() {
        let line = "[09:30:01] POST /v2/chat/completions key=测试Key pool=buddy model=glm-5.3 acct=账号A uid=user-123456 stream=true status=200 43765ms ttfb=812ms";
        let e = parse_log_line(line, "2026-11-08").expect("应解析成功");
        assert_eq!(e.time, "09:30:01");
        assert_eq!(e.method, "POST");
        assert_eq!(e.path, "/v2/chat/completions");
        assert_eq!(e.key_name, "测试Key");
        assert_eq!(e.pool, "buddy");
        assert_eq!(e.model, "glm-5.3");
        assert_eq!(e.acct, "账号A");
        assert_eq!(e.uid, "user-123456");
        assert!(e.stream);
        assert_eq!(e.status, 200);
        assert_eq!(e.duration_ms, 43765);
        assert_eq!(e.ttfb_ms, Some(812));
        assert!(e.stage.is_none());
        assert!(e.error.is_none());
    }

    /// F-84：失败行（stage + error 含空格与 token 字样）与非请求行跳过
    #[test]
    fn parse_log_line_failure_and_skips_non_request_lines() {
        let line = "[22:05:11] POST /v1/messages key=- pool=qoder model=gpt-5 acct=- uid=- stream=false status=503 120ms stage=dispatch error=no healthy account (model=x pool=trae)";
        let e = parse_log_line(line, "2026-11-08").expect("应解析成功");
        assert_eq!(e.status, 503);
        assert_eq!(e.duration_ms, 120);
        assert_eq!(e.stage.as_deref(), Some("dispatch"));
        assert_eq!(
            e.error.as_deref(),
            Some("no healthy account (model=x pool=trae)")
        );
        assert_eq!(e.key_name, "-");
        assert!(!e.stream);

        for bad in [
            "[09:00:00] [SCHED] 落池: pool=buddy model=glm-5.3 bind=None fallback_from=None",
            "[09:00:01] [DEBUG] uid=abc status=200",
            "--- Request Body (10 bytes, preview 10B) ---",
            "任意非日志行",
            "[09:00:02] GET /health",
        ] {
            assert!(parse_log_line(bad, "2026-11-08").is_none(), "应跳过: {bad}");
        }
    }

    /// F-84：custom 池带 stage 行 + 无 ttfb 聚合行解析
    #[test]
    fn parse_log_line_custom_stage_and_aggregate() {
        let line = "[12:00:00] POST /v1/chat/completions key=k1 pool=custom model=m1 acct=甲 uid=abcdef123456 stream=false status=200 900ms stage=upstream_1";
        let e = parse_log_line(line, "2026-11-08").expect("应解析成功");
        assert_eq!(e.stage.as_deref(), Some("upstream_1"));
        assert_eq!(e.ttfb_ms, None);
        assert_eq!(e.duration_ms, 900);
    }

    /// F-84：recent_entries 分页 + result 筛选（写线程异步落盘，用 flush_pending 兜底）
    #[test]
    fn recent_entries_pages_and_filters() {
        let dir = tmp_dir("recent");
        let logger = ApiLogger::new(dir.clone());
        for i in 0..7 {
            logger.log_request(
                if i % 2 == 0 { "buddy" } else { "trae" },
                "POST",
                "/v1/chat/completions",
                "glm-5.3",
                false,
                if i < 5 { 200 } else { 503 },
                "uid-x",
                100 + i,
                "k",
                "a",
                if i < 5 { None } else { Some("exhausted") },
            );
        }
        logger.flush_pending();
        // 全量：7 行，新→旧（后写的在前）
        let r = logger.recent_entries(3, 1, 50, None, None);
        assert_eq!(r.total, 7);
        assert!(!r.truncated);
        assert_eq!(r.rows.len(), 7);
        assert_eq!(r.rows[0].duration_ms, 106);
        assert_eq!(r.rows[6].duration_ms, 100);
        // 第 2 页（size=3）跨页取值
        let r2 = logger.recent_entries(3, 2, 3, None, None);
        assert_eq!(r2.rows.len(), 3);
        assert_eq!(r2.rows[0].duration_ms, 103);
        // fail 筛选：仅 2 条 503
        let rf = logger.recent_entries(3, 1, 50, None, Some("fail"));
        assert_eq!(rf.total, 2);
        assert!(rf.rows.iter().all(|e| e.status == 503));
        // ok 筛选：5 条 200
        let ro = logger.recent_entries(3, 1, 50, None, Some("ok"));
        assert_eq!(ro.total, 5);
        // pool 筛选：buddy = 偶数下标 4 条
        let rp = logger.recent_entries(3, 1, 50, Some("buddy"), None);
        assert_eq!(rp.total, 4);
        // B-10：未知 result 值前置校验 → 空集（不静默放行全量）
        let rb = logger.recent_entries(3, 1, 50, None, Some("success"));
        assert_eq!(rb.total, 0);
        assert!(rb.rows.is_empty());
        assert!(!rb.truncated);
        let _ = today();
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
            "user-1234567890abcdef", 42, "测试Key", "账号A", None,
        );
        logger.log_request(
            "trae", "POST", "/v1/chat/completions", "glm-5.2", false, 503,
            "none", 8, "", "", Some("no healthy account"),
        );
        let content = logger.read_log(&today()).expect("log written");
        assert!(content.contains("pool=buddy model=glm-5.3"), "buddy 行需含资源标识与模型: {content}");
        assert!(content.contains("pool=trae model=glm-5.2"), "trae 行需含资源标识与模型: {content}");
        assert!(content.contains("key=测试Key"), "需含 API Key 名: {content}");
        assert!(content.contains("acct=账号A"), "需含账号名: {content}");
        assert!(content.contains("key=- "), "空名称 Key 需显示占位符: {content}");
        assert!(content.contains(" acct=- "), "空名称账号需显示占位符: {content}");
        assert!(content.contains("error=no healthy account"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sched_event_line_format() {
        let dir = tmp_dir("sched");
        let logger = ApiLogger::new(dir.clone());
        logger.log_sched_event("sticky_yield uid=abc inflight=1 limit=1");
        let content = logger.read_log(&today()).expect("log written");
        // M-3 清洗：message 内 " uid=" 字面量锚首空格被替换为 ZWSP，防读侧误锚
        assert!(
            content.contains("[SCHED] sticky_yield\u{200B}uid=abc inflight=1 limit=1"),
            "需含 [SCHED] 前缀与清洗后正文: {content}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// B-7/M-3：用户可控值内含解析锚字面量时，落行前清洗破坏锚匹配——
    /// 行不被排除、字段不错位（读回值与写入值只差 ZWSP，锚定字段正确）
    #[test]
    fn sanitize_log_value_防解析锚错位() {
        let dir = tmp_dir("sanitize");
        let logger = ApiLogger::new(dir.clone());
        // key 名含 " key=" 与 " pool="；模型名含 " ttfb="；错误消息含 [SCHED] 与 " key="
        let key_with_anchor = "我的 key=假 pool=x";
        let model_with_anchor = "glm-5.3 ttfb=1ms";
        let error_with_anchor = "上游异常 [SCHED] 内部 key=y";
        logger.log_request(
            "buddy",
            "POST",
            "/v1/chat/completions",
            model_with_anchor,
            false,
            502,
            "uid-sanitize",
            88,
            key_with_anchor,
            "账号A",
            Some(error_with_anchor),
        );
        logger.flush_pending();

        let r = logger.recent_entries(3, 1, 50, None, None);
        assert_eq!(r.total, 1, "行不得因值内锚字面量被误排除: {:?}", r.rows);
        let e = &r.rows[0];
        // 锚定字段（结构化）不受值内锚污染
        assert_eq!(e.pool, "buddy");
        assert_eq!(e.status, 502);
        assert_eq!(e.uid, "uid-sanitize");
        assert_eq!(e.acct, "账号A");
        assert_eq!(e.duration_ms, 88);
        // 值还原：与写入值只差 ZWSP（空格前缀锚首空格被替换 / [ 后插入 ZWSP）
        let expected_key = key_with_anchor
            .replace(" key=", "\u{200B}key=")
            .replace(" pool=", "\u{200B}pool=");
        assert_eq!(e.key_name, expected_key);
        let expected_model = model_with_anchor.replace(" ttfb=", "\u{200B}ttfb=");
        assert_eq!(e.model, expected_model);
        let expected_error = error_with_anchor
            .replace(" key=", "\u{200B}key=")
            .replace("[SCHED]", "[\u{200B}SCHED]");
        assert_eq!(e.error.as_deref(), Some(expected_error.as_str()));
        let _ = fs::remove_dir_all(&dir);
    }

    /// B-7/M-3 补口（auth::log_reject）：401 拒绝行 path 取自 request.uri().path()，
    /// 为请求行唯一客户端可控值——调用点按同口径清洗后行不被误排除、字段不错位；
    /// 对照：未清洗的原始 path（内嵌 [SCHED]，无需空格）会被 parse_log_line 整行排除
    #[test]
    fn log_request_path_with_anchor_清洗后可解析() {
        let dir = tmp_dir("path-anchor");
        let logger = ApiLogger::new(dir.clone());
        // 模拟 auth::log_reject 的 path 清洗口径（sanitize_log_value 于调用点应用）
        let path = sanitize_log_value("/v1/[SCHED]x key=假");
        logger.log_request(
            "-", "GET", &path, "-", false, 401, "-", 0, "", "",
            Some("invalid api key"),
        );
        logger.flush_pending();

        let r = logger.recent_entries(3, 1, 50, None, None);
        assert_eq!(r.total, 1, "清洗后含锚 path 的 401 行不得被误排除: {:?}", r.rows);
        let e = &r.rows[0];
        assert_eq!(e.status, 401);
        assert_eq!(e.pool, "-");
        assert_eq!(e.error.as_deref(), Some("invalid api key"));

        // 对照：未清洗 path 直落行 → 行级锚 [SCHED] 命中 → 整行被误排除
        let raw_line = "[00:00:01] GET /v1/[SCHED]x key=假 key=- pool=- model=- acct=- uid=- stream=false status=401 0ms error=invalid api key\n";
        assert!(
            parse_log_line(raw_line, &today()).is_none(),
            "未清洗 path 应被行级锚误排除（证明清洗必要性）"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// B-9：自然日窗口——手工放置 40 天前日志文件（模拟应用长期停用），
    /// days=3 查询不得返回窗口外的旧数据
    #[test]
    fn recent_entries_natural_day_window_filters_old_dates() {
        let dir = tmp_dir("window");
        let logger = ApiLogger::new(dir.clone());
        logger.log_request("buddy", "POST", "/v1/chat/completions", "m", false, 200, "u", 5, "k", "a", None);
        logger.flush_pending();
        let old_date = (Local::now().date_naive() - chrono::Duration::days(40))
            .format("%Y-%m-%d")
            .to_string();
        std::fs::write(
            dir.join(format!("api_{old_date}.log")),
            "[09:00:00] GET /old key=k pool=buddy model=m acct=a uid=u stream=false status=200 1ms\n",
        )
        .unwrap();
        let r = logger.recent_entries(3, 1, 50, None, None);
        assert_eq!(
            r.total, 1,
            "自然日窗口应过滤 40 天前文件，仅剩今日 1 条: {:?}",
            r.rows.iter().map(|e| e.date.as_str()).collect::<Vec<_>>()
        );
        assert!(r.rows.iter().all(|e| e.date == today()));
        // days=1 同样只含今日
        let r1 = logger.recent_entries(1, 1, 50, None, None);
        assert_eq!(r1.total, 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
