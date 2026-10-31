//! WorkBuddy 上游请求层（T2.1/F-28 + T2.6 双源保活）
//!
//! headers 三铁律（§3.9 ①）：
//! 1. `Origin` + `Referer` 必带，按账号 region 切换（CN=codebuddy.cn 系 /
//!    Global=workbuddy.ai）；缺 Origin/Referer 被网关拒；
//! 2. 缺省字段显式 `X-No-*: 1` 占位（User-Id/Enterprise-Id/Department-Info）；
//! 3. **红线：chat 请求绝不携带 `X-Refresh-Token`**——该头只允许出现在
//!    refresh 端点（配 `X-Auth-Refresh-Source: workbuddy`），带此头触发安全拦截。
//!
//! 上游只回 SSE：非流式由 wb_sse::aggregate 本地聚合（payload 层强制 stream:true）。

use std::io::{BufRead, BufReader, Read};
use std::time::Duration;

use crate::fs_utils;
use crate::tasks::wb_common;

/// 对话上游 base（按区域）
pub const WB_CHAT_HOST_CN: &str = "https://copilot.tencent.com";
pub const WB_CHAT_HOST_GLOBAL: &str = "https://www.workbuddy.ai";
pub const WB_CHAT_PATH: &str = "/v2/chat/completions";
/// UA 伪装（F-28）
pub const WB_UA: &str = "CLI/2.63.2 CodeBuddy/2.63.2";
/// CLI 客户端版本（与 WB_UA 版本一致；X-IDE-Version 指纹头用，issue #48）
pub const WB_CLI_VERSION: &str = "2.63.2";
/// 刷新端点（红线：唯一允许携带 X-Refresh-Token 的地方）
pub const WB_REFRESH_URL: &str = "https://www.codebuddy.cn/v2/plugin/auth/token/refresh";
/// 首字超时（F-34）：上游建连后 10s 内未产出任何字节 → 故障转移
pub const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(10);
/// SSE 消费端停滞轮询步长（单一事实来源，sse.rs 转换循环 / wb_sse.rs
/// next_event_polling 共用）：上游停滞期间每 500ms 醒来检查一次客户端
/// 是否断连（sender.is_closed()），避免「断连 + 上游停滞」时死等下一行
/// （最长 300s 读超时）才释放账号并发槽
pub const LINE_POLL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Default)]
pub struct WbCreds {
    /// 工具侧账号 id（wb-xxx，token store 键；预留日志/刷新定位用）
    #[allow(dead_code)]
    pub id: String,
    #[allow(dead_code)]
    pub name: String,
    pub uid: String,
    pub token: String,
    /// token domain 字段（决定区域与 X-Domain）
    pub domain: String,
    /// 企业 id（可空 → X-No-Enterprise-Id: 1）
    pub enterprise_id: String,
    /// Global 区：domain 含 `.workbuddy.ai`（F-36 域名路由 §5.2）
    pub global_region: bool,
}

impl WbCreds {
    pub fn chat_base(&self) -> &'static str {
        if self.global_region {
            WB_CHAT_HOST_GLOBAL
        } else {
            WB_CHAT_HOST_CN
        }
    }
}

/// WB SSE Agent：连接 10s / 写 30s / 空闲读 300s
/// （ureq 2.12 未启用 proxy-from-env，直连，不会回环到本网关端口）
pub fn wb_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_read(Duration::from_secs(300))
        .timeout_write(Duration::from_secs(30))
        .timeout_connect(Duration::from_secs(10))
        .max_idle_connections(20)
        .max_idle_connections_per_host(20)
        .build()
}

/// 构建对话请求头（三铁律落点；返回 (header, value) 列表便于断言与复用）
pub fn build_chat_headers(c: &WbCreds) -> Vec<(&'static str, String)> {
    let base = c.chat_base();
    let referer = format!("{}{}", base, WB_CHAT_PATH);
    let mut h: Vec<(&'static str, String)> = vec![
        ("content-type", "application/json".into()),
        ("accept", "text/event-stream".into()),
        ("user-agent", WB_UA.into()),
        // 铁律 1：Origin/Referer 必带
        ("origin", base.into()),
        ("referer", referer),
        ("X-Requested-With", "XMLHttpRequest".into()),
        ("Authorization", format!("Bearer {}", c.token)),
        // 铁律 2：缺省字段显式 X-No-* 占位
        (
            if c.uid.is_empty() { "X-No-User-Id" } else { "X-User-Id" },
            if c.uid.is_empty() { "1".into() } else { c.uid.clone() },
        ),
        (
            if c.enterprise_id.is_empty() { "X-No-Enterprise-Id" } else { "X-Enterprise-Id" },
            if c.enterprise_id.is_empty() { "1".into() } else { c.enterprise_id.clone() },
        ),
        (
            if c.domain.is_empty() { "X-No-Department-Info" } else { "X-Domain" },
            if c.domain.is_empty() { "1".into() } else { c.domain.clone() },
        ),
        ("X-Product", "SaaS".into()),
        // 客户端指纹（issue #48）：CLI 身份头（与 UA 版本一致）+ 请求标识——
        // 个人中心「请求明细」按这些头识别客户端，缺失时客户端列显示 "-"
        ("X-IDE-Type", "CLI".into()),
        ("X-IDE-Name", "CLI".into()),
        ("X-IDE-Version", WB_CLI_VERSION.into()),
        // X-Request-ID：每请求随机 32 位 hex。用 ThreadRng（rand::random，CSPRNG、
        // OsRng 种子）替代 main 的 uuid::Uuid::new_v4().simple()，免去新增 uuid
        // 依赖；不走 commands::oauth::random_hex——其为 panic 形态（OsRng 失败即
        // 失败，无退化路径），此处内联 rand::random 语义相同且更直观（审查 No.2）
        ("X-Request-ID", format!("{:032x}", rand::random::<u128>())),
    ];
    // 设备/会话指纹（issue #48）：账号级稳定派生（同账号恒定、跨账号隔离）；
    // uid 缺失即不带，不伪造
    if let (Some(mid), Some(sid)) = (
        wb_common::derive_device_fingerprint(&c.uid, "machine"),
        wb_common::derive_device_fingerprint(&c.uid, "session"),
    ) {
        h.push(("X-Machine-ID", mid));
        h.push(("X-Session-ID", sid));
    }
    // 铁律 3（红线）：chat 请求绝不携带 X-Refresh-Token——构造器根本不产出该头，
    // debug_assert 兜底防未来误加。
    debug_assert!(
        !h.iter().any(|(k, _)| k.eq_ignore_ascii_case("x-refresh-token")),
        "红线：chat 请求禁止携带 X-Refresh-Token"
    );
    h
}

/// 会话头三件套（dsh-xdpool#58 实证 2026-10-19：国际版网关硬校验，缺任一即
/// 400 11128 "request illegal"；值不校验仅查存在性，官方客户端每个 chat 请求
/// 都带）。X-Conversation-ID 取调用方传入的 conv_id——make_wb_request 收到
/// 的与 prepare 注入请求体的是同一份（调用方作用域现成有，免得此处再全量
/// 反序列化一遍 body），同会话稳定，上游前缀缓存不被打散；空串兜底随机
/// hex。另两个头每请求随机。仅 chat 路径附带——生图/目录（wb_images/
/// wb_catalog 复用 build_chat_headers）非 chat 网关不校验，不扩散。
fn conversation_headers(conv_id: &str) -> Vec<(&'static str, String)> {
    let cid = if conv_id.is_empty() {
        format!("{:032x}", rand::random::<u128>())
    } else {
        conv_id.to_string()
    };
    vec![
        ("X-Conversation-ID", cid),
        (
            "X-Conversation-Request-ID",
            format!("{:032x}", rand::random::<u128>()),
        ),
        (
            "X-Conversation-Message-ID",
            format!("{:032x}", rand::random::<u128>()),
        ),
    ]
}

/// 上游请求错误：(HTTP 状态 | 502 传输错误, body 摘要, Retry-After 秒)
pub type UpstreamErr = (u16, String, Option<u64>);

/// 发起 WB 对话请求，返回 SSE 流 reader。conv_id 为调用方 prepare 时注入
/// 请求体的同一份会话 id
pub fn make_wb_request(
    c: &WbCreds,
    conv_id: &str,
    body: &[u8],
) -> Result<Box<dyn Read + Send>, UpstreamErr> {
    let url = format!("{}{}", c.chat_base(), WB_CHAT_PATH);
    let mut req = wb_agent().post(&url);
    for (k, v) in build_chat_headers(c) {
        req = req.set(k, &v);
    }
    // dsh-xdpool#58：X-Conversation 三头缺一即 11128（国际版硬校验）
    for (k, v) in conversation_headers(conv_id) {
        req = req.set(k, &v);
    }
    match req.send_bytes(body) {
        Ok(r) => Ok(Box::new(r.into_reader())),
        Err(ureq::Error::Status(code, resp)) => {
            let retry_after = resp
                .header("retry-after")
                .and_then(|v| v.trim().parse::<u64>().ok());
            // 响应体读取失败须带错误标记（对齐 qoder_upstream 红线#7 同款修复）：
            // 吞为空串会让错误分类无特征可判，排障信息全失
            let body_text = resp
                .into_string()
                .unwrap_or_else(|e| format!("<响应体读取失败: {e}>"));
            Err((code, body_text, retry_after))
        }
        Err(e) => {
            let s = format!("{}", e);
            let detail = if s.contains("dns") || s.contains("resolve") || s.contains("name resolution") {
                format!("DNS解析失败（{}），请检查网络: {}", c.chat_base(), e)
            } else if s.contains("timed out") || s.contains("timeout") {
                format!("连接超时（{} 10秒内未响应）: {}", c.chat_base(), e)
            } else if s.contains("tls") || s.contains("certificate") || s.contains("ssl") {
                format!("TLS证书验证失败: {}", e)
            } else {
                format!("传输错误: {}", e)
            };
            Err((502, detail, None))
        }
    }
}

/// 可中断行源：基于转发线程 + channel receiver 的行迭代器，额外提供带超时的
/// 取行（next_timeout）。供 sse.rs 流式转换循环在「上游停滞」期间周期性醒来
/// 检查客户端断连（sender.is_closed()），避免僵尸流死等上游下一行
/// （最长 300s 读超时）才退出、期间一直占用账号并发槽。
pub struct InterruptibleLines {
    rx: std::sync::mpsc::Receiver<std::io::Result<String>>,
    /// 首行（from_reader 首字窗口内产出；Iterator/next_timeout 下优先取出）
    pending: Option<String>,
    /// 首个读错误（issue #158 报错A，P2-1）：转发线程上抛的 io::Error 此前
    /// 被 next_timeout/Iterator::next 折叠为 EOF，长流被上游 h2 RST_STREAM
    /// 掐断时静默伪装成正常完成、完全不可观测。此处留存错误本体供 sse 层
    /// 经 take_read_error 取出识别（行为不变：对外仍折叠为 EOF）
    last_read_error: Option<std::io::Error>,
}

impl InterruptibleLines {
    /// reader → 可中断行源：首行以 FIRST_BYTE_TIMEOUT 超时等待（F-34 首字
    /// 超时 → 故障转移），Err(()) = 首字超时/首行前出错。后续行无超时
    /// （正常流式出包不受限）；转发线程 detach（受 Agent 300s 读超时兜底
    /// 自然退出，不无限泄漏）。
    pub fn from_reader<R: Read + Send + 'static>(reader: R) -> Result<Self, ()> {
        let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<String>>();
        relay_lines(reader, tx);
        // 首字（可为空行/注释行，均视为"已开始产出"）
        match rx.recv_timeout(FIRST_BYTE_TIMEOUT) {
            Ok(Ok(first)) => Ok(Self {
                rx,
                pending: Some(first),
                last_read_error: None,
            }),
            _ => Err(()),
        }
    }

    /// 任意行迭代器 → 可中断行源（转发线程桥接；测试与内存切分形态复用）
    pub fn from_iterator(inner: Box<dyn Iterator<Item = String> + Send>) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<String>>();
        std::thread::spawn(move || {
            for line in inner {
                if tx.send(Ok(line)).is_err() {
                    break;
                }
            }
        });
        Self { rx, pending: None, last_read_error: None }
    }

    /// 取出留存的首个读错误（issue #158 报错A，P2-1）：仅捕获一次，取出后
    /// 置 None。sse 层在 EOF 兜底处调用，按传输掐断特征识别上抛哨兵
    pub fn take_read_error(&mut self) -> Option<std::io::Error> {
        self.last_read_error.take()
    }

    /// 带超时取行：
    /// - Ok(Some(line))：取到一行
    /// - Ok(None)：流结束（读错误折叠为 EOF；转发线程退出等同 EOF，与既有语义一致）
    /// - Err(())：dur 窗口内无行（上游停滞，非 EOF）。调用方可借此检查
    ///   客户端断连（sender.is_closed()）后继续等待
    pub fn next_timeout(&mut self, dur: Duration) -> Result<Option<String>, ()> {
        if let Some(line) = self.pending.take() {
            return Ok(Some(line));
        }
        match self.rx.recv_timeout(dur) {
            Ok(Ok(line)) => Ok(Some(line)),
            Ok(Err(e)) => {
                // 读错误留存供 take_read_error 取出（issue #158 报错A），
                // 对外语义不变：仍折叠为 EOF
                if self.last_read_error.is_none() {
                    self.last_read_error = Some(e);
                }
                Ok(None)
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(()),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Ok(None),
        }
    }
}

impl Iterator for InterruptibleLines {
    type Item = String;
    fn next(&mut self) -> Option<String> {
        if let Some(line) = self.pending.take() {
            return Some(line);
        }
        match self.rx.recv() {
            Ok(Ok(line)) => Some(line),
            Ok(Err(e)) => {
                // 读错误留存供 take_read_error 取出（issue #158 报错A），
                // 对外语义不变：仍折叠为 EOF
                if self.last_read_error.is_none() {
                    self.last_read_error = Some(e);
                }
                None
            }
            Err(_) => None, // 转发线程退出 = EOF
        }
    }
}

/// 行迭代封装：首行以 FIRST_BYTE_TIMEOUT 超时等待（F-34 首字超时 → 故障转移），
/// 后续行无超时（正常流式出包不受限）。
///
/// 实现：转发线程 + channel。首字超时返回 Err；转发线程 detach（受 Agent
/// 300s 读超时兜底自然退出，不无限泄漏）。
pub fn lines_with_first_byte_timeout<R: Read + Send + 'static>(
    reader: R,
) -> Result<Box<dyn Iterator<Item = String> + Send>, ()> {
    Ok(Box::new(InterruptibleLines::from_reader(reader)?))
}

// ==================== 读错误共享槽（issue #158 P2-2）====================

/// 读错误共享槽：relay 线程的 io::Error 需要跨所有权边界读取时使用——
/// 行源被再包裹（竞速 `chain_rest` 的 `filter_map(.ok())` 折叠 /
/// `Box<dyn Iterator>` 擦拭 `InterruptibleLines` 类型 / QoderTranslate 桥接）
/// 后，`InterruptibleLines::last_read_error` 随所有权下沉不可达。共享槽让
/// 错误在折叠点留存，最终消费方（wb_sse / 路由层）在 EOF 兜底处取出上抛
/// 传输掐断哨兵。槽存 Display 文本（io::Error 不可 Clone，消费方只做特征
/// 识别与日志，不需要错误本体）。
pub type ReadErrSlot = std::sync::Arc<std::sync::Mutex<Option<String>>>;

/// 新建空读错误槽
pub fn new_read_err_slot() -> ReadErrSlot {
    std::sync::Arc::new(std::sync::Mutex::new(None))
}

/// 取出留存的首个读错误文本（仅捕获一次，取出后置 None）
pub fn take_read_error_slot(slot: &ReadErrSlot) -> Option<String> {
    slot.lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// 带 读错误槽 的首字超时行封装：与 `lines_with_first_byte_timeout` 等价，
/// 额外把后续行的读错误写入共享槽（供被 Box 擦拭类型的行源跨层读取）。
/// 首字窗口内的读错误仍返回 Err(())（与 from_reader 语义一致：故障转移）
pub fn lines_with_first_byte_timeout_tracked<R: Read + Send + 'static>(
    reader: R,
    read_slot: ReadErrSlot,
) -> Result<Box<dyn Iterator<Item = String> + Send>, ()> {
    let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<String>>();
    relay_lines(reader, tx);
    match rx.recv_timeout(FIRST_BYTE_TIMEOUT) {
        Ok(Ok(first)) => Ok(chain_rest(first, rx, read_slot)),
        _ => Err(()),
    }
}

/// 同 lines_with_first_byte_timeout，但返回可中断行源：sse.rs 流式转换
/// 需要在上游停滞期间周期性检查客户端断连（next_timeout + sender.is_closed）
pub fn lines_with_first_byte_timeout_interruptible<R: Read + Send + 'static>(
    reader: R,
) -> Result<InterruptibleLines, ()> {
    InterruptibleLines::from_reader(reader)
}

/// reader → channel 行转发线程（首字超时/竞速共用）
///
/// 读错误（对端 FIN 后 read 恒 Err、h2 RST_STREAM 等）转发一次后立即退出：
/// 不 break 则对恒错误 reader 全速空转（Err → send → Err → …），若下游
/// 消费者吞掉 Err（如 `filter_map(.ok())`）即形成双线程死循环、CPU 恒烧
/// 不自愈（issue #164）。转发一次保证错误仍达折叠点（read_slot /
/// last_read_error），随后 reader 随线程退出 Drop——CLOSE-WAIT fd 同步关闭。
/// 消费者侧（InterruptibleLines / chain_rest）本就把 Err 折叠为 EOF，
/// 转发线程退出 → Disconnected → 消费者自然收尾，语义不变。
fn relay_lines<R: Read + Send + 'static>(
    reader: R,
    tx: std::sync::mpsc::Sender<std::io::Result<String>>,
) {
    std::thread::spawn(move || {
        let br = BufReader::new(reader);
        for line in br.lines() {
            let midstream_err = line.is_err();
            if tx.send(line).is_err() {
                break;
            }
            if midstream_err {
                break;
            }
        }
    });
}

/// 首行 + 剩余行组装为行迭代器（竞速对冲路径使用：胜者 receiver 直接折叠）。
/// 胜者行源后续行携带的读错误（h2 RST_STREAM 等传输层掐断）写入共享槽——
/// `filter_map(.ok())` 折叠处是错误本体的最后可见点，此处不留存则彻底丢失、
/// 掐断长流被伪装成正常完成（issue #158 P2-2）
fn chain_rest(
    first: String,
    rx: std::sync::mpsc::Receiver<std::io::Result<String>>,
    read_slot: ReadErrSlot,
) -> Box<dyn Iterator<Item = String> + Send> {
    Box::new(std::iter::once(first).chain(rx.into_iter().scan(
        false,
        move |ended, r| {
            if *ended {
                return None; // 已 EOF：不再拉取底层（rx 随链 Drop → relay 退出）
            }
            match r {
                Ok(line) => Some(line),
                Err(e) => {
                    {
                        let mut slot = read_slot.lock().unwrap_or_else(|p| p.into_inner());
                        if slot.is_none() {
                            *slot = Some(e.to_string());
                        }
                    }
                    *ended = true; // 对外语义不变：读错误折叠为 EOF，且立即终止拉取
                    None
                }
            }
        },
    ))) as Box<dyn Iterator<Item = String> + Send>
}

// ==================== F-76③ 慢请求竞速对冲 ====================

/// 首字竞速结果（F-76③）。泛型 `T` 为对冲侧上下文（wb_route 传入计数租约
/// HedgeLease）：未触发/双败路径在本函数内 Drop（计数自动释放，无泄漏），
/// Ok 路径随结果移交调用方，由调用方在竞速结束后释放（settle_hedge）
pub struct RaceOutcome<T> {
    /// 竞速胜者的行迭代器（含首行）
    pub lines: Box<dyn Iterator<Item = String> + Send>,
    /// 对冲是否接管（true = lines 来自对冲请求；false = 主请求胜出或未触发）
    pub takeover: bool,
    /// 对冲侧上下文：触发过对冲时 Some，未触发为 None
    pub hedge: Option<T>,
    /// 审查修复（L2）：主请求侧已死（EOF/IO 错误/超窗，败于竞速）——接管成功
    /// 时主侧败者无失败痕迹，调用方据此补记错误/冷却，账单可解释
    pub primary_failed: bool,
    /// 审查修复（L2）：对冲请求侧已死（EOF/IO 错误/超窗，败于竞速）——
    /// 主胜成功时对冲侧败者同理补记
    pub hedge_failed: bool,
    /// 胜者行源的读错误共享槽（issue #158 P2-2）：胜者流中途被掐断（h2 RST
    /// 等）时错误本体留存于此，调用方在 EOF 兜底处取出上抛传输掐断哨兵；
    /// 败者侧槽随竞速 Drop（败者死亡已由 primary_failed/hedge_failed 记账）
    pub read_slot: ReadErrSlot,
}

/// 慢请求竞速对冲（F-76③，P2C 调度理念的请求级延伸）：
///
/// 主请求首字节超过 `hedge_delay_ms` 未到 → 调 `spawn_backup` 取第二账号发起对冲
/// 请求，先出首字者胜、另一侧 reader 被 Drop（连接关闭，被取消一侧不计 usage）。
/// 主请求仍受原 FIRST_BYTE_TIMEOUT 约束（从发起计），对冲请求自带完整首字窗口。
///
/// `hedge_delay_ms` 收敛到 [1s, 8s]：保证首字超时（10s）前留出竞速窗口，
/// 过小会对亚秒抖动误触发，过大则永远轮不到对冲。
///
/// 计数配对保证（F-77）：`spawn_backup` 返回的 `T`（计数租约）在所有退出路径
/// 恰好释放一次——未触发/无对冲账号（闭包返回 None，租约由闭包自释放）、
/// 双败 Err（本函数 Drop）、竞速胜出（移交调用方）
pub fn lines_with_first_byte_hedged<T>(
    primary: Box<dyn Read + Send>,
    hedge_delay_ms: u64,
    mut spawn_backup: impl FnMut() -> Option<(Box<dyn Read + Send>, T)>,
) -> Result<RaceOutcome<T>, ()> {
    use std::sync::mpsc::{Receiver, TryRecvError};
    use std::time::Instant;

    const POLL: Duration = Duration::from_millis(20);
    let hedge_delay = Duration::from_millis(hedge_delay_ms.clamp(1000, 8000));
    let started = Instant::now();
    let (tx1, rx1) = std::sync::mpsc::channel::<std::io::Result<String>>();
    relay_lines(primary, tx1);
    // 主/对冲两侧各自持一个读错误槽：胜者槽随 RaceOutcome 带出，败者槽
    // 随竞速 Drop（败者死亡已由 primary_failed/hedge_failed 记账，不缺观测）
    let slot1 = new_read_err_slot();
    let slot2 = new_read_err_slot();

    // 阶段一：对冲窗口内主请求先出首字 → 未触发对冲，与原首字超时语义一致
    match rx1.recv_timeout(hedge_delay) {
        Ok(Ok(first)) => {
            return Ok(RaceOutcome {
                lines: chain_rest(first, rx1, slot1.clone()),
                takeover: false,
                hedge: None,
                primary_failed: false,
                hedge_failed: false,
                read_slot: slot1,
            })
        }
        // 主请求窗口内即失败（EOF/IO 错误）或窗口耗尽 → 进入对冲路径
        Ok(Err(_)) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
    }

    let Some((backup, hedge)) = spawn_backup() else {
        // 无可用对冲账号（含对冲建连失败，租约已由闭包 Drop 释放）：
        // 退回纯首字超时（剩余窗口）
        let deadline = started + FIRST_BYTE_TIMEOUT;
        return match rx1.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Ok(first)) => Ok(RaceOutcome {
                lines: chain_rest(first, rx1, slot1.clone()),
                takeover: false,
                hedge: None,
                primary_failed: false,
                hedge_failed: false,
                read_slot: slot1,
            }),
            _ => Err(()),
        };
    };
    let (tx2, rx2) = std::sync::mpsc::channel::<std::io::Result<String>>();
    relay_lines(backup, tx2);

    // 阶段二：竞速。主请求维持原 10s 上限（started+FIRST_BYTE_TIMEOUT），
    // 对冲请求自带完整 10s 首字窗口；20ms 轮询合并两个 std channel（无 select
    // 原语，轮询开销可忽略）。任一侧 EOF/出错标记 dead；双 dead → Err
    //（hedge 租约随局部变量 Drop 释放——修复双败路径计数泄漏）。
    let primary_deadline = started + FIRST_BYTE_TIMEOUT;
    let hedge_deadline = Instant::now() + FIRST_BYTE_TIMEOUT;
    let mut primary_dead = false;
    let mut hedge_dead = false;
    loop {
        let now = Instant::now();
        if !primary_dead {
            if now >= primary_deadline {
                primary_dead = true; // 超原首字上限：不再考虑主请求
            } else {
                match rx1.try_recv() {
                    Ok(Ok(line)) => {
                        return Ok(RaceOutcome {
                            lines: chain_rest(line, rx1, slot1.clone()),
                            takeover: false,
                            hedge: Some(hedge),
                            primary_failed: false,
                            // 审查修复（L2）：对冲侧已死（EOF/IO/超窗）而主胜出——
                            // 对冲败者带标记返回，由 settle_hedge 补记错误
                            hedge_failed: hedge_dead,
                            read_slot: slot1,
                        })
                    }
                    Ok(Err(_)) | Err(TryRecvError::Disconnected) => primary_dead = true,
                    Err(TryRecvError::Empty) => {}
                }
            }
        }
        if !hedge_dead {
            if now >= hedge_deadline {
                hedge_dead = true;
            } else {
                match rx2.try_recv() {
                    Ok(Ok(line)) => {
                        return Ok(RaceOutcome {
                            lines: chain_rest(line, rx2, slot2.clone()),
                            takeover: true,
                            hedge: Some(hedge),
                            // 审查修复（L2）：主侧已死（EOF/IO/超窗）而对冲接管——
                            // 主败者带标记返回，由 settle_hedge 补记错误
                            primary_failed: primary_dead,
                            hedge_failed: false,
                            read_slot: slot2,
                        })
                    }
                    Ok(Err(_)) | Err(TryRecvError::Disconnected) => hedge_dead = true,
                    Err(TryRecvError::Empty) => {}
                }
            }
        }
        if primary_dead && hedge_dead {
            return Err(());
        }
        // 仅剩一侧：退化为阻塞等待该侧剩余窗口
        if primary_dead {
            return wait_single(rx2, hedge_deadline, slot2.clone()).map(|lines| RaceOutcome {
                lines,
                takeover: true,
                hedge: Some(hedge),
                primary_failed: true,
                hedge_failed: false,
                read_slot: slot2,
            });
        }
        if hedge_dead {
            return wait_single(rx1, primary_deadline, slot1.clone()).map(|lines| RaceOutcome {
                lines,
                takeover: false,
                hedge: Some(hedge),
                primary_failed: false,
                hedge_failed: true,
                read_slot: slot1,
            });
        }
        std::thread::sleep(POLL);
    }

    fn wait_single(
        rx: Receiver<std::io::Result<String>>,
        deadline: Instant,
        read_slot: ReadErrSlot,
    ) -> Result<Box<dyn Iterator<Item = String> + Send>, ()> {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Ok(first)) => Ok(chain_rest(first, rx, read_slot)),
            _ => Err(()),
        }
    }
}

// ==================== T2.6 双源 token 保活（网关 401 路径） ====================

/// 忙态哨兵错误（移植 main 8628629）：他方进程（GUI/CLI）正在刷新同一账号——
/// 区别于真实刷新失败。调用方（wb_route 401 自愈）应据此本轮换号且不记 401
/// 致命错误（防瞬时忙态被 classify_error 误判 SessionDead → disabled 踢出轮转）；
/// 对方刷新落库后，下轮请求经持锁二次检查/stale 对比直接复用新凭证。
pub const ERR_PEER_REFRESHING: &str = "他方进程正在刷新该账号凭证，本次跳过";

/// 网关侧刷新（T2.6）：读 token store → POST refresh → 原子回写工具侧副本。
/// 与 commands::workbuddy::workbuddy_refresh_token 同一端点/红线；此处面向
/// API 网关 401 自动续期（无 Tauri State 依赖，仅 data_dir）。
/// 审查加固（H-1 对齐）：与 tasks 侧刷新共用**每账号锁 + 跨进程锁 + 持锁二次检查**
/// ——网关 401 自愈与 wb-renew/wb-checkin/积分刷新对同一账号并发刷新时，服务端
/// 一次性轮换下后落库者覆盖先落库者（H-1 事故场景），此前网关路径未闭合；
/// 表锁同步一至 `WB_TOKEN_STORE_LOCK`（与 tasks/commands 写方共锁，防同账号
/// 读-改-写互相覆盖，P2 双锁分裂修复）。
/// `stale_access`：本次请求 401 所用凭证（调用方失效凭证）——store 中已是不同
/// 新值（他人已刷新落库）时直接复用，不发网络请求。成功返回新 accessToken；
/// 失败返回 Err（调用方按 SwitchKey 换号）。
pub fn refresh_access_token(
    data_dir: &std::path::Path,
    account_id: &str,
    stale_access: Option<&str>,
) -> Result<String, String> {
    // SQLite 化（P4）：死引用修复——原读写 data_dir **根**路径的 workbuddy_token_store.json
    // （正牌在 data/ 子目录，此分叉使网关 401 刷新永远读写错位文件），现统一走 store wb_tokens 表。
    // 锁序约定（单向）：每账号刷新锁 → WB_TOKEN_STORE_LOCK（与 tasks 侧一致，无环）
    let lock = crate::tasks::wb_common::refresh_lock_for(account_id);
    let _refresh_guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    let Some(_cross) = crate::tasks::wb_common::cross_refresh_lock(data_dir, account_id) else {
        // 他方进程刷新中：返回忙态哨兵（调用方换号不禁用，下次 401 经二次检查复用），
        // 不与真实刷新失败混同
        return Err(ERR_PEER_REFRESHING.to_string());
    };

    // 持锁二次检查 + 凭证收敛读取（secure 回填：DB 占位 + vault 明文内存态）
    let (store_access, refresh) = {
        let _table = crate::tasks::wb_common::WB_TOKEN_STORE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let store: serde_json::Value =
            crate::tasks::wb_common::token_store_load_secure(data_dir);
        let rec = store
            .get("tokens")
            .and_then(|t| t.get(account_id))
            .cloned()
            .unwrap_or_default();
        let access = rec
            .get("access_token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let refresh = rec
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or("该账号无 refreshToken（不可刷新，需重新登录）")?
            .to_string();
        (access, refresh)
    };
    // 二次检查：他人已刷新落库（store access ≠ 调用方失效凭证）→ 直接复用
    if let Some(stale) = stale_access {
        if !store_access.is_empty() && store_access != stale {
            return Ok(store_access);
        }
    }

    // 红线：X-Refresh-Token 仅出现在 refresh 端点
    let resp = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .post(WB_REFRESH_URL)
        .set("Authorization", "Bearer")
        .set("User-Agent", wb_common::WB_DESKTOP_UA)
        .set("X-Refresh-Token", &refresh)
        .set("X-Auth-Refresh-Source", "workbuddy")
        .set("Content-Type", "application/json")
        .send_string("{}");
    let body: serde_json::Value = match resp {
        // 传输层故障如实上抛：unwrap_or_default 会把解码失败落 Value::Null，
        // 最终误报「刷新响应中无 accessToken」伪装成凭证缺失，误导用户重新登录
        Ok(r) => r
            .into_json()
            .map_err(|e| format!("刷新响应读取失败: {e}"))?,
        Err(ureq::Error::Status(code, r)) => {
            // issue #118：读错误响应体透传上游 error/error_description，便于区分会话失效与账号封禁
            return Err(wb_common::refresh_fail_msg(
                code,
                r,
                &format!("刷新失败（HTTP {code}）：refresh token 可能已失效，需重新登录"),
            ));
        }
        Err(e) => return Err(format!("刷新请求失败: {e}")),
    };
    let dig_str = |v: &serde_json::Value, keys: &[&str]| -> Option<String> {
        keys.iter().find_map(|k| v.get(k).and_then(|x| x.as_str())).map(str::to_string)
    };
    let new_access = dig_str(&body, &["accessToken", "data", "accessToken"])
        .or_else(|| {
            body.get("data")
                .map(|d| dig_str(d, &["accessToken"]))
                .unwrap_or(None)
        })
        .ok_or("刷新响应中无 accessToken")?;
    let new_refresh = dig_str(&body, &["refreshToken"]).or_else(|| {
        body.get("data")
            .map(|d| dig_str(d, &["refreshToken"]))
            .unwrap_or(None)
    });
    let expires_in = body
        .pointer("/expiresIn")
        .or_else(|| body.pointer("/data/expiresIn"))
        .and_then(|v| v.as_i64());
    let refresh_expires_in = body
        .pointer("/refreshExpiresIn")
        .or_else(|| body.pointer("/data/refreshExpiresIn"))
        .and_then(|v| v.as_i64());
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    // 原子回写工具侧副本（F-10 双源谁新用谁：expiresAtMs 更晚者胜出）：
    // 锁内重读最新 store 再合并写回（网络段已释放锁），并发刷新不丢他账号更新
    {
        let _guard = crate::tasks::wb_common::WB_TOKEN_STORE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut r = {
            // 凭证收敛（P0-1）：secure 回填（vault 明文内存态参与 merge）
            let store: serde_json::Value =
                crate::tasks::wb_common::token_store_load_secure(data_dir);
            store
                .get("tokens")
                .and_then(|t| t.get(account_id))
                .cloned()
                .unwrap_or(serde_json::json!({}))
        };
        if let Some(rm) = r.as_object_mut() {
            rm.insert("access_token".into(), serde_json::json!(new_access));
            if let Some(nr) = &new_refresh {
                if !nr.is_empty() {
                    rm.insert("refresh_token".into(), serde_json::json!(nr));
                }
            }
            if let Some(s) = expires_in {
                rm.insert("expires_at_ms".into(), serde_json::json!(now_ms + s * 1000));
            }
            if let Some(s) = refresh_expires_in {
                rm.insert("refresh_expires_at_ms".into(), serde_json::json!(now_ms + s * 1000));
            }
            rm.insert("updated_at".into(), serde_json::json!(fs_utils::now_iso()));
        }
        // 凭证收敛（P0-1）：secure upsert（敏感字段进 vault、DB 占位）。
        // 审查修复（M10）：回写失败原实现仅日志并照常返回新 token——刷新端点可能
        // 一次性轮换 refresh_token，新 rt 只存在于本函数局部变量，返回 Ok 即随栈帧
        // 永久丢失：下一轮 401 用 store 里的旧 rt 刷新必然失败，账号被迫重新登录。
        // 修复：先退避重试消化 vault/DB 瞬时故障；重试耗尽后按 rt 是否被轮换分流——
        // 已轮换 → 上抛 Err（调用方换号/禁用是正确语义，该账号下次刷新注定失败）；
        // 未轮换 → 仅丢 access 更新，下轮 401 重新刷新即可自愈，维持仅日志（原场景）
        let mut upsert_err: Option<String> = None;
        for attempt in 0..3u32 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(200 * attempt as u64));
            }
            match crate::tasks::wb_common::token_store_upsert_secure(data_dir, account_id, &r) {
                Ok(()) => {
                    upsert_err = None;
                    break;
                }
                Err(e) => upsert_err = Some(e),
            }
        }
        if let Some(e) = upsert_err {
            let rt_rotated = new_refresh
                .as_deref()
                .is_some_and(|nr| !nr.is_empty() && nr != refresh);
            let msg = format!("wb 网关刷新回写失败（重试 3 次后）: {e}");
            if rt_rotated {
                let fatal =
                    format!("{msg}；refresh token 已被服务端轮换且无法持久化，账号需重新登录");
                fs_utils::app_log(data_dir, &fatal);
                return Err(fatal);
            }
            fs_utils::app_log(data_dir, &msg);
        }
    }
    Ok(new_access)
}

/// 池感知续期入口（审查 H1 修复）：CN 池走 [`refresh_access_token`]（wb_tokens 表 +
/// CN 刷新端点），buddy-ai 国际池走 buddy_ai_tokens 表 + workbuddy.ai 刷新端点。
/// 原实现 wb_route 三处执行路径统一调 CN 版：buddy-ai 账号 401 时查 wb_tokens
/// 必然无凭证（同 token 双池导入时更会用国内侧 rt 打 CN 端点——跨池凭证串扰），
/// 刷新永不成功 → 账号被错误冷却/禁用
pub fn refresh_access_token_for(
    data_dir: &std::path::Path,
    account_id: &str,
    stale_access: Option<&str>,
    intl: bool,
) -> Result<String, String> {
    if intl {
        refresh_access_token_intl(data_dir, account_id, stale_access)
    } else {
        refresh_access_token(data_dir, account_id, stale_access)
    }
}

/// buddy-ai（国际池）网关 401 续期：镜像 [`refresh_access_token`] 的锁序与二次检查，
/// 表（buddy_ai_tokens + buddy-ai vault ns）/ 端点 / UA 全部国际档。
/// 回写为合并式（保留 rec 其他字段——`last_daily_chat` 打卡幂等戳 / uid 兜底不得抹除）
fn refresh_access_token_intl(
    data_dir: &std::path::Path,
    account_id: &str,
    stale_access: Option<&str>,
) -> Result<String, String> {
    use crate::tasks::wb_common::{buddy_ai_token_store_load, buddy_ai_token_store_upsert};
    // 与 CN 版同款：进程内每账号刷新锁 → 跨进程命名互斥体（GUI/CLI 双进程）
    let lock = wb_common::refresh_lock_for(account_id);
    let _refresh_guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    let Some(_cross) =
        crate::tasks::qoder_common::CrossProcLock::try_acquire_ns(data_dir, "buddy-ai", account_id, 5000).0
    else {
        return Err(ERR_PEER_REFRESHING.to_string());
    };
    let (store_access, refresh) = {
        let rec = buddy_ai_token_store_load(data_dir)
            .get("tokens")
            .and_then(|t| t.get(account_id))
            .cloned()
            .unwrap_or(serde_json::json!({}));
        let access = rec
            .get("access_token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let refresh = rec
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or("该账号无 refreshToken（不可刷新，需重新登录）")?
            .to_string();
        (access, refresh)
    };
    // 持锁二次检查：他人已刷新落库（store access ≠ 调用方失效凭证）→ 直接复用
    if let Some(stale) = stale_access {
        if !store_access.is_empty() && store_access != stale {
            return Ok(store_access);
        }
    }
    // 红线：X-Refresh-Token 仅出现在 refresh 端点；国际端点经 region_refresh_url 解析
    let resp = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .post(wb_common::region_refresh_url(wb_common::REGION_GLOBAL.chat_base))
        .set("Authorization", "Bearer")
        .set("User-Agent", wb_common::REGION_GLOBAL.ua)
        .set("X-Refresh-Token", &refresh)
        .set("X-Auth-Refresh-Source", "workbuddy")
        .set("Content-Type", "application/json")
        .send_string("{}");
    let body: serde_json::Value = match resp {
        // 传输层故障如实上抛：unwrap_or_default 会把解码失败落 Value::Null，
        // 最终误报「刷新响应中无 accessToken」伪装成凭证缺失，误导用户重新登录
        Ok(r) => r
            .into_json()
            .map_err(|e| format!("刷新响应读取失败: {e}"))?,
        Err(ureq::Error::Status(code, r)) => {
            // issue #118：读错误响应体透传上游 error/error_description，便于区分会话失效与账号封禁
            return Err(wb_common::refresh_fail_msg(
                code,
                r,
                &format!("刷新失败（HTTP {code}）：refresh token 可能已失效，需重新登录"),
            ));
        }
        Err(e) => return Err(format!("刷新请求失败: {e}")),
    };
    let dig_str = |v: &serde_json::Value, keys: &[&str]| -> Option<String> {
        keys.iter().find_map(|k| v.get(k).and_then(|x| x.as_str())).map(str::to_string)
    };
    let new_access = dig_str(&body, &["accessToken"]).or_else(|| {
        body.get("data")
            .and_then(|d| dig_str(d, &["accessToken"]))
    }).ok_or("刷新响应中无 accessToken")?;
    let new_refresh = dig_str(&body, &["refreshToken"]).or_else(|| {
        body.get("data").and_then(|d| dig_str(d, &["refreshToken"]))
    });
    let expires_in = body
        .pointer("/expiresIn")
        .or_else(|| body.pointer("/data/expiresIn"))
        .and_then(|v| v.as_i64());
    let refresh_expires_in = body
        .pointer("/refreshExpiresIn")
        .or_else(|| body.pointer("/data/refreshExpiresIn"))
        .and_then(|v| v.as_i64());
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    // 合并写回：锁外重读最新 store，在其上更新四键（并发刷新/打卡幂等戳互不覆盖），
    // rt 轮换响应缺失时保留旧值
    {
        let mut r = buddy_ai_token_store_load(data_dir)
            .get("tokens")
            .and_then(|t| t.get(account_id))
            .cloned()
            .unwrap_or(serde_json::json!({}));
        if let Some(rm) = r.as_object_mut() {
            rm.insert("access_token".into(), serde_json::json!(new_access));
            if let Some(nr) = &new_refresh {
                if !nr.is_empty() {
                    rm.insert("refresh_token".into(), serde_json::json!(nr));
                }
            }
            if let Some(s) = expires_in {
                rm.insert("expires_at_ms".into(), serde_json::json!(now_ms + s * 1000));
            }
            if let Some(s) = refresh_expires_in {
                rm.insert("refresh_expires_at_ms".into(), serde_json::json!(now_ms + s * 1000));
            }
            rm.insert("updated_at".into(), serde_json::json!(fs_utils::now_iso()));
        }
        // secure upsert（敏感字段进 buddy-ai vault 命名空间、DB 占位）。
        // 审查修复（M10）：同 CN 版——回写失败不能仅日志后照常返回新 token，
        // rt 被服务端轮换且未落盘时新 rt 随栈帧丢失、账号被迫重登；先退避重试，
        // 重试耗尽后按 rt 是否轮换分流（已轮换上抛 Err，未轮换仅日志可自愈）
        let mut upsert_err: Option<String> = None;
        for attempt in 0..3u32 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(200 * attempt as u64));
            }
            match buddy_ai_token_store_upsert(data_dir, account_id, &r) {
                Ok(()) => {
                    upsert_err = None;
                    break;
                }
                Err(e) => upsert_err = Some(e),
            }
        }
        if let Some(e) = upsert_err {
            let rt_rotated = new_refresh
                .as_deref()
                .is_some_and(|nr| !nr.is_empty() && nr != refresh);
            let msg = format!("buddy-ai 网关刷新回写失败（重试 3 次后）: {e}");
            if rt_rotated {
                let fatal =
                    format!("{msg}；refresh token 已被服务端轮换且无法持久化，账号需重新登录");
                fs_utils::app_log(data_dir, &fatal);
                return Err(fatal);
            }
            fs_utils::app_log(data_dir, &msg);
        }
    }
    Ok(new_access)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn creds(global: bool, uid: &str, eid: &str, domain: &str) -> WbCreds {
        WbCreds {
            id: "wb-test".into(),
            uid: uid.into(),
            name: "测试".into(),
            token: "tk".into(),
            domain: domain.into(),
            enterprise_id: eid.into(),
            global_region: global,
        }
    }

    #[test]
    fn headers_origin_referer_follow_region() {
        let cn = build_chat_headers(&creds(false, "u", "", ""));
        let base = |h: &[(&str, String)], k: &str| {
            h.iter().find(|(hk, _)| hk.eq_ignore_ascii_case(k)).map(|(_, v)| v.clone()).unwrap()
        };
        assert_eq!(base(&cn, "origin"), "https://copilot.tencent.com");
        assert!(base(&cn, "referer").starts_with("https://copilot.tencent.com/v2/chat/completions"));
        let g = build_chat_headers(&creds(true, "u", "", ""));
        assert_eq!(base(&g, "origin"), "https://www.workbuddy.ai");
    }

    #[test]
    fn headers_no_placeholders_when_missing() {
        let h = build_chat_headers(&creds(false, "", "", ""));
        let get = |k: &str| {
            h.iter().find(|(hk, _)| hk.eq_ignore_ascii_case(k)).map(|(_, v)| v.clone())
        };
        // 铁律 2：缺省字段显式 X-No-* 占位
        assert_eq!(get("X-No-User-Id").as_deref(), Some("1"));
        assert!(get("X-User-Id").is_none());
        assert_eq!(get("X-No-Enterprise-Id").as_deref(), Some("1"));
        assert_eq!(get("X-No-Department-Info").as_deref(), Some("1"));
        // 指纹缺失即不带（uid 为空不伪造，issue #48）
        assert!(get("X-Machine-ID").is_none());
        assert!(get("X-Session-ID").is_none());
        // 完整字段时走正头
        let h = build_chat_headers(&creds(false, "u1", "e1", "d1"));
        let get = |k: &str| {
            h.iter().find(|(hk, _)| hk.eq_ignore_ascii_case(k)).map(|(_, v)| v.clone())
        };
        assert_eq!(get("X-User-Id").as_deref(), Some("u1"));
        assert_eq!(get("X-Enterprise-Id").as_deref(), Some("e1"));
        assert_eq!(get("X-Domain").as_deref(), Some("d1"));
        assert_eq!(get("X-Product").as_deref(), Some("SaaS"));
        // UA 伪装
        assert_eq!(get("user-agent").as_deref(), Some(WB_UA));
        // CLI 身份指纹（issue #48）：与 UA 版本一致
        assert_eq!(get("X-IDE-Type").as_deref(), Some("CLI"));
        assert_eq!(get("X-IDE-Name").as_deref(), Some("CLI"));
        assert_eq!(get("X-IDE-Version").as_deref(), Some(WB_CLI_VERSION));
        // 设备/会话指纹：32 位 hex、machine ≠ session
        let mid = get("X-Machine-ID").expect("X-Machine-ID 必须存在");
        let sid = get("X-Session-ID").expect("X-Session-ID 必须存在");
        assert_eq!(mid.len(), 32);
        assert!(mid.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(mid, sid);
        assert_ne!(
            mid,
            build_chat_headers(&creds(false, "u2", "", ""))
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("X-Machine-ID"))
                .map(|(_, v)| v.clone())
                .unwrap(),
            "跨账号指纹必须隔离防关联"
        );
    }

    #[test]
    fn request_id_varies_per_call() {
        // X-Request-ID 每次请求随机（32 位 hex）
        let c = creds(false, "u", "", "");
        let rid = |h: &[(&'static str, String)]| {
            h.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("X-Request-ID"))
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        let a = rid(&build_chat_headers(&c));
        let b = rid(&build_chat_headers(&c));
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn conversation_headers_stable_per_conversation() {
        // dsh-xdpool#58：调用方 conv_id 稳定透传（同会话上游前缀缓存不打散）；
        // 另两个头每请求随机；空串兜底随机 hex（32 位，与 X-Request-ID 同款）
        let a = conversation_headers("conv-1");
        let b = conversation_headers("conv-1");
        let get = |h: &[(&'static str, String)], k: &str| {
            h.iter().find(|(n, _)| *n == k).unwrap().1.clone()
        };
        assert_eq!(get(&a, "X-Conversation-ID"), "conv-1");
        assert_eq!(get(&b, "X-Conversation-ID"), "conv-1");
        assert_ne!(
            get(&a, "X-Conversation-Request-ID"),
            get(&b, "X-Conversation-Request-ID")
        );
        assert_ne!(
            get(&a, "X-Conversation-Message-ID"),
            get(&b, "X-Conversation-Message-ID")
        );
        let fb = conversation_headers("");
        let id = get(&fb, "X-Conversation-ID");
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn redline_no_refresh_token_header_in_chat() {
        // 铁律 3：chat 头里绝不出现 X-Refresh-Token
        let c = creds(true, "u", "e", "d");
        let h = build_chat_headers(&c);
        assert!(!h.iter().any(|(k, _)| k.eq_ignore_ascii_case("x-refresh-token")));
        assert!(h.iter().any(|(k, v)| k.eq_ignore_ascii_case("authorization") && v.starts_with("Bearer ")));
    }

    #[test]
    fn refresh_requires_stored_refresh_token() {
        // 空 store → 无 refreshToken → 明确报错（不静默）
        let dir = std::env::temp_dir().join(format!("wb_up_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let r = refresh_access_token(&dir, "wb-none", None);
        assert!(r.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ==================== F-76③ 竞速对冲 ====================

    /// 首字节延迟后吐出固定文本的 reader（模拟慢上游）
    struct DelayedRead {
        delay: Option<Duration>,
        data: std::io::Cursor<Vec<u8>>,
    }
    impl DelayedRead {
        fn new(delay_ms: u64, text: &str) -> Self {
            Self {
                delay: if delay_ms == 0 { None } else { Some(Duration::from_millis(delay_ms)) },
                data: std::io::Cursor::new(text.as_bytes().to_vec()),
            }
        }
    }
    impl Read for DelayedRead {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if let Some(d) = self.delay.take() {
                std::thread::sleep(d);
            }
            self.data.read(buf)
        }
    }

    fn collect(lines: Box<dyn Iterator<Item = String> + Send>) -> Vec<String> {
        lines.collect()
    }

    // ==================== 读错误共享槽（issue #158 P2-2）====================

    /// 产出一段 SSE 行后抛读错误的 reader（模拟 h2 RST_STREAM 掐断长流）
    struct CutAfterFirstRead {
        n: u8,
    }
    impl Read for CutAfterFirstRead {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.n == 0 {
                self.n = 1;
                let d = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n";
                buf[..d.len()].copy_from_slice(d);
                Ok(d.len())
            } else {
                Err(std::io::Error::other("h2 RST_STREAM pushed"))
            }
        }
    }

    #[test]
    fn tracked_lines_record_read_error_into_slot() {
        let slot = new_read_err_slot();
        let lines = lines_with_first_byte_timeout_tracked(CutAfterFirstRead { n: 0 }, slot.clone())
            .expect("首字窗口内应有产出");
        let got: Vec<String> = lines.collect();
        assert!(!got.is_empty(), "掐断前产出的行应保留（折叠 EOF 但内容不丢）");
        let e = take_read_error_slot(&slot).expect("读错误应留存于共享槽");
        assert!(e.contains("RST_STREAM"), "错误本体应留存，实际: {e}");
        assert!(take_read_error_slot(&slot).is_none(), "仅捕获一次，取出后置 None");
    }

    #[test]
    fn tracked_lines_clean_eof_keeps_slot_empty() {
        let slot = new_read_err_slot();
        let lines = lines_with_first_byte_timeout_tracked(
            std::io::Cursor::new(b"data: [DONE]\n".to_vec()),
            slot.clone(),
        )
        .expect("首字窗口内应有产出");
        let got: Vec<String> = lines.collect();
        assert_eq!(got, vec!["data: [DONE]"], "正常完成行不受影响");
        assert!(take_read_error_slot(&slot).is_none(), "正常完成不留读错误");
    }

    /// read 恒 Err 的 reader（issue #164 触发源：对端 FIN 后每次 read 立即
    /// 返回错误，非阻塞、非 EOF，DeadlineStream 兜不住）
    struct AlwaysErrRead;
    impl Read for AlwaysErrRead {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("connection reset by peer"))
        }
    }

    /// 限时等 Disconnected：超时即视为转发线程未退出（死循环回归）。
    /// 返回 (转发行数, 转发错误数)
    fn wait_relay_disconnected(
        rx: &std::sync::mpsc::Receiver<std::io::Result<String>>,
    ) -> (usize, usize) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let (mut lines, mut errs) = (0usize, 0usize);
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Ok(_)) => lines += 1,
                Ok(Err(_)) => errs += 1,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return (lines, errs),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => assert!(
                    std::time::Instant::now() < deadline,
                    "转发线程 5s 内未退出（issue #164 死循环回归）"
                ),
            }
        }
    }

    #[test]
    fn relay_lines_exits_on_persistent_read_error() {
        // issue #164：恒错误 reader 下转发线程必须有限步退出（不 break 则
        // Err → send → Err → … 全速空转，与吞 Err 的消费者互相泵错误）
        let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<String>>();
        relay_lines(AlwaysErrRead, tx);
        let (lines, errs) = wait_relay_disconnected(&rx);
        assert_eq!(lines, 0, "读错误 reader 不应产出行");
        assert_eq!(errs, 1, "读错误应恰好转发一次供下游折叠点留存");
    }

    #[test]
    fn relay_lines_exits_after_midstream_cut() {
        // 流中途掐断（先产出再恒 Err）：错误转发一次后线程退出 →
        // Disconnected，chain_rest / InterruptibleLines 折叠 EOF 自然收尾
        let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<String>>();
        relay_lines(CutAfterFirstRead { n: 0 }, tx);
        let (lines, errs) = wait_relay_disconnected(&rx);
        assert_eq!(lines, 2, "掐断前产出的行应已转发（数据行 + 尾随空行）");
        assert_eq!(errs, 1, "掐断错误应恰好转发一次");
    }

    /// Drop 副作用探针：验证对冲上下文 T 的 RAII 配对契约
    struct DropProbe(Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    // ==================== 可中断行源（InterruptibleLines） ====================

    #[test]
    fn interruptible_lines_next_timeout_semantics() {
        // 语义契约：Ok(Some)=行 / Ok(None)=EOF / Err=窗口内停滞（非 EOF）
        let mut src = InterruptibleLines::from_iterator(Box::new(
            ["a", "b"].iter().map(|s| s.to_string()),
        ));
        assert_eq!(src.next_timeout(Duration::from_millis(50)), Ok(Some("a".into())));
        assert_eq!(src.next_timeout(Duration::from_millis(50)), Ok(Some("b".into())));
        assert_eq!(src.next_timeout(Duration::from_millis(50)), Ok(None));
    }

    #[test]
    fn interruptible_lines_timeout_then_line_arrives() {
        // 停滞（窗口内无行，Err）→ 数据到达 → 正常取行；Iterator 语义不受影响
        let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<String>>();
        let mut src = InterruptibleLines {
            rx,
            pending: None,
            last_read_error: None,
        };
        assert_eq!(src.next_timeout(Duration::from_millis(30)), Err(()));
        tx.send(Ok("late".into())).unwrap();
        assert_eq!(src.next_timeout(Duration::from_millis(500)), Ok(Some("late".into())));
        drop(tx);
        assert_eq!(src.next_timeout(Duration::from_millis(500)), Ok(None));
        // Iterator 兜底语义：EOF 之后 next() 持续 None
        assert_eq!(src.next(), None);
    }

    #[test]
    fn interruptible_lines_read_error_folds_to_eof() {
        // 读错误折叠为 EOF（与 lines_with_first_byte_timeout 既有语义一致）
        let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<String>>();
        let mut src = InterruptibleLines {
            rx,
            pending: None,
            last_read_error: None,
        };
        tx.send(Err(std::io::Error::other("boom"))).unwrap();
        assert_eq!(src.next_timeout(Duration::from_millis(500)), Ok(None));
        drop(tx); // 释放 sender：否则下方阻塞式 next() 永久等待（recv 无界）
        assert_eq!(src.next(), None);
    }

    #[test]
    fn interruptible_lines_captures_read_error_via_take() {
        // issue #158 P2-1：读错误对外仍折叠为 EOF，但错误本体留存可取——
        // take 一次有值、二次 None（仅捕获首个）；next_timeout 与 Iterator
        // 两条路径均须捕获
        let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<String>>();
        let mut src = InterruptibleLines {
            rx,
            pending: None,
            last_read_error: None,
        };
        tx.send(Ok("line".into())).unwrap();
        tx.send(Err(std::io::Error::other("http2 stream error RST_STREAM"))).unwrap();
        assert_eq!(src.next_timeout(Duration::from_millis(500)), Ok(Some("line".into())));
        assert_eq!(src.next_timeout(Duration::from_millis(500)), Ok(None)); // 折叠为 EOF
        let e = src.take_read_error().expect("读错误应被捕获");
        assert!(
            crate::api_server::is_transport_cut_text(&e.to_string()),
            "错误文本应命中传输掐断特征: {e}"
        );
        assert!(src.take_read_error().is_none(), "仅捕获首个，取出后置 None");
        // Iterator 路径同样捕获
        let (tx2, rx2) = std::sync::mpsc::channel::<std::io::Result<String>>();
        let mut src2 = InterruptibleLines {
            rx: rx2,
            pending: None,
            last_read_error: None,
        };
        tx2.send(Err(std::io::Error::other("connection reset by peer"))).unwrap();
        drop(tx2);
        assert_eq!(src2.next(), None);
        assert!(src2.take_read_error().is_some(), "Iterator 路径也应捕获");
    }

    #[test]
    fn hedge_not_fired_when_primary_fast() {
        // 主请求亚秒出首字 → 未触发对冲（hedge None），内容完整
        let out: RaceOutcome<&str> = lines_with_first_byte_hedged(
            Box::new(DelayedRead::new(0, "data: a\ndata: b\n")),
            2000,
            || None,
        )
        .unwrap();
        assert!(out.hedge.is_none());
        assert!(!out.takeover);
        assert_eq!(collect(out.lines), vec!["data: a".to_string(), "data: b".to_string()]);
    }

    #[test]
    fn hedge_takeover_when_primary_slow() {
        // 主请求 3s 无首字（>1s 对冲窗口）→ 备份立即出首字 → 对冲接管
        let out: RaceOutcome<&str> = lines_with_first_byte_hedged(
            Box::new(DelayedRead::new(3000, "data: slow\n")),
            1000,
            || Some((Box::new(DelayedRead::new(0, "data: fast\n")), "hedge-uid")),
        )
        .unwrap();
        assert_eq!(out.hedge, Some("hedge-uid"));
        assert!(out.takeover);
        assert_eq!(collect(out.lines), vec!["data: fast".to_string()]);
    }

    #[test]
    fn hedge_lost_when_primary_recovers() {
        // 主请求慢但先于备份出首字（备份 3s、主 1.5s，均在窗口内）→ 主胜出
        let out: RaceOutcome<&str> = lines_with_first_byte_hedged(
            Box::new(DelayedRead::new(1500, "data: primary\n")),
            1000,
            || Some((Box::new(DelayedRead::new(3000, "data: backup\n")), "hedge-uid")),
        )
        .unwrap();
        assert_eq!(out.hedge, Some("hedge-uid"));
        assert!(!out.takeover);
        assert_eq!(collect(out.lines), vec!["data: primary".to_string()]);
    }

    #[test]
    fn hedge_fallback_without_backup() {
        // 无可用对冲账号 → 主请求退回纯首字超时窗口（1.2s < 10s 仍成功）
        let out: RaceOutcome<&str> = lines_with_first_byte_hedged(
            Box::new(DelayedRead::new(1200, "data: solo\n")),
            1000,
            || None,
        )
        .unwrap();
        assert!(out.hedge.is_none());
        assert!(!out.takeover);
        assert_eq!(collect(out.lines), vec!["data: solo".to_string()]);
    }

    #[test]
    fn hedge_err_when_both_eof() {
        // 主/对冲均立即 EOF → 双败 Err（泛型版语义回归）
        let out: Result<RaceOutcome<&str>, ()> = lines_with_first_byte_hedged(
            Box::new(std::io::empty()),
            500,
            || Some((Box::new(std::io::empty()), "hedge-uid")),
        );
        assert!(out.is_err());
    }

    #[test]
    fn hedge_ctx_dropped_on_double_failure() {
        // P0 泄漏修复契约：双败路径对冲上下文必须在函数内 Drop（计数释放恰好一次）
        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let d2 = drops.clone();
        let out: Result<RaceOutcome<DropProbe>, ()> = lines_with_first_byte_hedged(
            Box::new(std::io::empty()),
            500,
            move || Some((Box::new(std::io::empty()), DropProbe(d2.clone()))),
        );
        assert!(out.is_err());
        assert_eq!(
            drops.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "双败路径对冲上下文必须恰好 Drop 一次"
        );
    }

    #[test]
    fn hedge_ctx_transferred_on_win() {
        // P0 泄漏修复契约：竞速胜出时上下文随 RaceOutcome 移交调用方，
        // 调用方 drop 后释放恰好一次（wb_route 在 settle_hedge 落定）
        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let d2 = drops.clone();
        let out = lines_with_first_byte_hedged(
            Box::new(DelayedRead::new(3000, "data: slow\n")),
            1000,
            move || Some((Box::new(DelayedRead::new(0, "data: fast\n")), DropProbe(d2.clone()))),
        )
        .unwrap();
        assert_eq!(drops.load(std::sync::atomic::Ordering::Relaxed), 0, "胜出前上下文仍在途");
        drop(out);
        assert_eq!(drops.load(std::sync::atomic::Ordering::Relaxed), 1, "调用方落定后释放恰好一次");
    }
}
