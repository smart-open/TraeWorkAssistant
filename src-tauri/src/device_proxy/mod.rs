//! Rust 版 MITM 代理（device_proxy.py 迁移，P4）。
//! 架构：hyper 1.x 协议栈自建（hudsucker 非拦截 CONNECT 隧道硬编码直连、无法透传用户 VPN 上游）。
//! 模块：ca 证书签发 / logger 请求日志 / upstream 上游连接 / handler MITM 改写 / ws 桥接。
//! 本文件：代理生命周期（ProxyServer）+ 主循环（accept → CONNECT 分流 / 明文转发）。

pub mod ca;
#[cfg(test)]
mod e2e;
pub mod handler;
pub mod local_capture;
pub mod logger;
pub mod upstream;
pub mod ws;
pub mod bypass;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::AtomicI64;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use http_body_util::{Full, Limited};
use hyper::body::Bytes;
use hyper::header::{HeaderName, HeaderValue};
use hyper::Request;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_rustls::TlsAcceptor;

use crate::device_proxy::ca::{ensure_ca, CaAuthority};
use crate::device_proxy::handler::{serve_mitm, HOP_BY_HOP_REQ, PIN_MIN_FAILS, ProxyCtx};
use crate::device_proxy::logger::{ProxyLog, RequestLogger};
use crate::device_proxy::upstream::{
    connect_direct, connect_via_upstream, UpstreamConnector, UpstreamProxy,
};

/// 建连/首读超时（对齐 Python `_CONN_TIMEOUT`）
const CONN_TIMEOUT: Duration = Duration::from_secs(300);
/// 明文转发上游超时（对齐 Python handle_plain 的 socket timeout=30s）
const PLAIN_TIMEOUT: Duration = Duration::from_secs(30);
/// 分发阶段头缓冲上限（Python 无上限仅靠超时兜底，此处防御性 64KB）
const MAX_DISPATCH_HEAD: usize = 64 * 1024;
/// 并发连接上限（对齐 Python `_CONN_SEMAPHORE` 信号量 128）。
/// 512：桌面客户端（豆包 = Chromium + cronet/ttnet 双网络栈）全量流量过代理时，
/// 启动风暴轻松超百条并发 CONNECT，另有长轮询/WS 常驻连接占槽——实测 128 上限
/// 启动 1 分钟即打满并触发客户端重试风暴（「以此账号打开豆包」整体卡死，
/// proxy.log 2026-09-15 19:14）。信号量仍兜底防代理自身被打挂。
const MAX_CONNS: usize = 512;
/// [overload] 日志节流秒数：重试风暴下每连接一条会刷屏（实测 40+ 条/秒），节流到 5 秒一条
const OVERLOAD_LOG_INTERVAL_SECS: u64 = 5;
/// CONNECT 200 应答后的 TLS 握手超时（客户端不发 ClientHello 时及时释放连接与并发槽）
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// 默认解密域名白名单（语义对齐 Charles SSL Proxying Locations：列表内域解密，
/// 其余 CONNECT 透明直通；桌面端设置页 PROXY_DOMAINS 可覆盖）。
/// 证书锁定的客户端域（豆包 ttnet 原生栈等）由自适应降级兜底：握手失败样本
/// ≥ PIN_MIN_FAILS 且失败率 >75% 即自动转透明直通（见 ProxyCtx::pin_state，重启代理复位）。
pub const DEFAULT_TARGETS: &[&str] = &[
    "trae.cn",
    "trae.com.cn",
    "mchost.guru",
    "zijieapi.com",
    "bytedance.com",
    "volcengine.com",
    "volces.com",
    "treecode.com",
    "doubao.com",
];

// ---------------- 生命周期 ----------------

/// 代理启动配置（由 commands/proxy.rs 从 AppState / 设置页构造）
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// 监听端口（恒为 127.0.0.1）
    pub port: u16,
    /// 监听域名列表（后缀匹配，构造时统一小写；空则用 [`DEFAULT_TARGETS`]）
    pub targets: Vec<String>,
    /// 自动捕获 JWT 写回 accounts（AUTO_CAPTURE_JWT，默认开）
    pub auto_capture_jwt: bool,
    /// 数据根目录（SQLite 化 P3：accounts/cooldowns/凭证快照均经 store 读写）
    pub data_dir: PathBuf,
    /// data/certs（CA 目录，与 Python 版布局一致）
    pub certs_dir: PathBuf,
    /// logs/proxy.log 操作日志
    pub log_path: PathBuf,
    /// 代理请求抓包日志目录（按日滚动 + 100MB 切分）
    pub req_log_dir: PathBuf,
    /// 上游代理（用户 VPN 梯子；非目标流量经此转出，失败回退直连）
    pub upstream: Option<UpstreamProxy>,
}

/// 运行中的代理句柄。drop shutdown 发送端即触发停止（changed() 出错分支），
/// 但建议显式调用 [`ProxyServer::stop`] 并按需 [`ProxyServer::join`] 等待端口释放。
pub struct ProxyServer {
    pub port: u16,
    pub captured: Arc<AtomicI64>,
    shutdown_tx: watch::Sender<bool>,
    exit_rx: watch::Receiver<bool>,
    task: JoinHandle<()>,
}

impl ProxyServer {
    /// 启动进程内代理（对齐 Python `main()`：ensure_ca → 设备标识同步 → 独占绑定 → accept 循环）
    pub async fn start(cfg: ProxyConfig, app: Option<tauri::AppHandle>) -> Result<ProxyServer, String> {
        // CA 证书：兼容已有 Python 版 RSA CA；缺失则生成（数据目录布局不变）
        let ca = Arc::new(ensure_ca(&cfg.certs_dir)?);

        let captured = Arc::new(AtomicI64::new(0));
        let log = ProxyLog::new(cfg.log_path.clone(), app, Arc::clone(&captured));
        let req_logger = Arc::new(RequestLogger::new(cfg.req_log_dir.clone()));
        let targets = if cfg.targets.is_empty() {
            DEFAULT_TARGETS.iter().map(|s| s.to_string()).collect()
        } else {
            cfg.targets.iter().map(|d| d.to_ascii_lowercase()).collect()
        };
        let ctx = Arc::new(ProxyCtx {
            log: log.clone(),
            req_logger,
            targets,
            auto_capture_jwt: cfg.auto_capture_jwt,
            data_dir: cfg.data_dir.clone(),
            upstream: cfg.upstream.clone(),
            pin_state: Mutex::new(std::collections::HashMap::new()),
        });

        // 升级历史假占位符设备标识（对齐 Python sync_account_devices，仅自动捕获开启时）
        if ctx.auto_capture_jwt {
            sync_account_devices(&ctx);
        }

        // Windows 独占绑定（SO_EXCLUSIVEADDRUSE，issue #7：防孤儿进程「假启动」）
        let listener = bind_listener(cfg.port).await?;

        // 启动横幅（对齐 Python main() 的日志行，前端实时代理面板直接可读）
        log.log(&format!(
            "代理已启动: 127.0.0.1:{}  (TRAE 多域 MITM 拦截 + JWT 自动捕获)",
            cfg.port
        ));
        let list: Vec<String> = ctx.targets.iter().map(|d| format!("*.{d}")).collect();
        log.log(&format!("监听 TRAE 域名: {}", list.join(", ")));
        log.log("  → 命中上述域名的请求会在面板中以 [TRAE] 标记；JWT 捕获不限 host（兼容未列出的子域）");
        log.log("  → 未在监听域名列表中的请求将透明转发（不记录日志），不影响其他 App 正常上网");
        log.log(&format!("accounts: {}", cfg.data_dir.join("accounts（SQLite accounts 表）").display()));
        log.log(&format!("代理请求日志: {} (100MB 滚动)", cfg.req_log_dir.display()));
        log.log(&format!(
            "自动捕获 JWT 写回 accounts.json: {}",
            if cfg.auto_capture_jwt { "开" } else { "关" }
        ));
        if let Some(up) = &cfg.upstream {
            log.log(&format!("上游代理(用户VPN)透传: {}", up.addr()));
        }
        // CA 安装状态如实探测输出（原为无条件提示，已安装也提示安装，误导用户）。
        // 三平台收口走 cert_query（Windows=certutil 根存储 / mac=security find-certificate）
        if crate::platform::cert_ctl::cert_query("TraeDeviceProxyCA") {
            log.log("CA 证书已安装到系统信任域（TraeDeviceProxyCA）✅");
        } else {
            log.log("⚠ CA 证书未安装：请在顶部「证书未信任」徽标或引导页一键安装（certs/ca.cer → 系统信任域），否则被代理的客户端会因证书不受信而无法加载页面");
        }

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        // 退出通知：accept 循环结束（无论主动 stop 还是意外崩溃）时置 true，
        // 供 commands/proxy.rs 看门狗监听并还原系统代理（对齐 Python 版 stdout EOF 看门狗语义）
        let (exit_tx, exit_rx) = watch::channel(false);
        let task = tokio::spawn(async move {
            accept_loop(listener, ctx, ca, cfg.upstream.clone(), shutdown_rx).await;
            let _ = exit_tx.send(true);
        });
        Ok(ProxyServer { port: cfg.port, captured, shutdown_tx, exit_rx, task })
    }

    /// 主动停止：accept 循环退出并中止所有在途连接任务
    pub fn stop(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    /// 任务退出通知（主动 stop 与意外崩溃均会触发；调用方结合「主动停止」标记区分）
    pub fn exit_signal(&self) -> watch::Receiver<bool> {
        self.exit_rx.clone()
    }

    /// 代理任务是否仍在运行（意外崩溃时为 false，供看门狗判定）
    pub fn is_running(&self) -> bool {
        !self.task.is_finished()
    }
}

// ---------------- 主循环 ----------------

async fn accept_loop(
    listener: TcpListener,
    ctx: Arc<ProxyCtx>,
    ca: Arc<CaAuthority>,
    upstream: Option<UpstreamProxy>,
    mut shutdown: watch::Receiver<bool>,
) {
    let permits = Arc::new(Semaphore::new(MAX_CONNS));
    // 明文转发与 MITM 转发共享同一路由策略（2026-09-15 重构，见 upstream.rs 模块注释）：
    // 配置了上游代理（用户 VPN）时一律**上游优先**、失败回退直连——不开启本代理时
    // 客户端流量本就走系统代理（用户 VPN），MITM 只是解密层，出口必须与其一致。
    // 实测教训（proxy.log 22:21）：VPN 接管路由/DNS 的环境下本进程直连目标域
    // 47 请求 0 响应（白屏根因），此前「目标域直连」策略在该环境下全线失效。
    // 语义对齐 mitmproxy `--mode upstream:` / Charles 上游代理。
    // 两个 Client 仅连接池隔离（明文/解密流量互不挤占 keep-alive 连接）。
    let plain_client: Client<UpstreamConnector, Full<Bytes>> =
        Client::builder(TokioExecutor::new()).build(UpstreamConnector::new(upstream.clone(), ctx.log.clone()));
    // MITM 解密后的上游转发 Client（进程级共享）：跨连接复用 hyper 连接池
    // （TCP/TLS keep-alive）。原实现在 serve_mitm 内每连接新建 Client，连接池
    // 无法跨连接复用——桌面客户端高频请求下每请求都重新建连，显著拖慢转发。
    let mitm_client: Client<UpstreamConnector, Full<Bytes>> =
        Client::builder(TokioExecutor::new()).build(UpstreamConnector::new(upstream.clone(), ctx.log.clone()));
    let mut conns: Vec<JoinHandle<()>> = Vec::new();
    // [overload] 日志节流锚点（Unix 秒）
    let mut last_overload_log: u64 = 0;
    // 空闲期定时回收已结束的连接句柄（审查修复：conns 仅在新 accept 时清理，
    // 长连接高频场景下已完成任务的 JoinHandle 会随 Vec 无界增长）
    let mut reap = tokio::time::interval(Duration::from_secs(60));
    loop {
        tokio::select! {
            // stop() 或 ProxyServer 整体 drop（发送端析构 → changed() 报错）都会触发退出
            _ = shutdown.changed() => break,
            _ = reap.tick() => {
                conns.retain(|h| !h.is_finished());
            }
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    // 并发超限（try_acquire 失败）直接关闭新连接，保证代理自身不被打挂。
                    // permit 必须移入任务、持有至连接结束（审查修复：原 try_acquire()
                    // 临时值语句结束即析构，MAX_CONNS 上限完全失效、过载分支不可达）
                    let permit = match Arc::clone(&permits).try_acquire_owned() {
                        Ok(p) => p,
                        Err(_) => {
                            // 节流：重试风暴下同秒可达百条，间隔记一条足够定位
                            let now_secs = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs())
                                .unwrap_or(0);
                            if now_secs >= last_overload_log + OVERLOAD_LOG_INTERVAL_SECS {
                                last_overload_log = now_secs;
                                ctx.log.log(&format!(
                                    "[overload] 并发连接已达上限 {MAX_CONNS}，拒绝来自 {peer} 的新连接（后续同类日志 5 秒节流一条）"
                                ));
                            }
                            continue;
                        }
                    };
                    conns.retain(|h| !h.is_finished());
                    let task_ctx = Arc::clone(&ctx);
                    let task_ca = Arc::clone(&ca);
                    let task_up = upstream.clone();
                    let task_client = plain_client.clone();
                    let task_mitm = mitm_client.clone();
                    conns.push(tokio::spawn(async move {
                        let _guard = permit; // 释放即归还信号量
                        handle_conn(stream, peer, task_ctx, task_ca, task_up, task_client, task_mitm)
                            .await;
                    }));
                }
                Err(e) => {
                    // 单条 accept 出错不应让整个代理退出（否则系统代理仍指向死端口）。
                    // 记录后短暂退避再重试，保持服务可用。
                    ctx.log.log(&format!("[accept] 异常(已忽略并重试): {e}"));
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
        }
    }
    // 停止：中止所有在途连接任务（对齐 Python「进程被杀」的停止语义）
    for h in conns {
        h.abort();
    }
    ctx.log.log("代理已停止");
}

/// 单连接分发（对齐 Python `handle_client`）：
/// - CONNECT + 目标域名 → 200 应答 → TLS(叶子证书) → [`serve_mitm`] 解密改写
/// - CONNECT + 其他域名 → [`tunnel_raw`] 透明隧道（不解密不记日志）
/// - 其余（明文 HTTP）→ [`handle_plain`] 转发
async fn handle_conn(
    mut stream: TcpStream,
    peer: SocketAddr,
    ctx: Arc<ProxyCtx>,
    ca: Arc<CaAuthority>,
    upstream: Option<UpstreamProxy>,
    plain_client: Client<UpstreamConnector, Full<Bytes>>,
    mitm_client: Client<UpstreamConnector, Full<Bytes>>,
) {
    let head = match timeout(CONN_TIMEOUT, read_head(&mut stream)).await {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => {
            ctx.log.log(&format!("[client] {peer} 读头失败: {e}"));
            return;
        }
        Err(_) => {
            ctx.log.log(&format!("[client] {peer} 读头超时 ({}s)", CONN_TIMEOUT.as_secs()));
            return;
        }
    };
    let first = String::from_utf8_lossy(head.split(|&b| b == b'\n').next().unwrap_or(b""))
        .trim()
        .to_string();
    let method = first.split(' ').next().unwrap_or("").to_ascii_uppercase();
    if method == "CONNECT" {
        // CONNECT host:port HTTP/1.1
        let target = first.split(' ').nth(1).unwrap_or("");
        let (host, port) = match target.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse::<u16>().unwrap_or(443)),
            None => (target.to_string(), 443),
        };
        // 审查修复：read_head 按块读会超读 head 之后的字节（客户端在 200 应答前
        // 抢发的 ClientHello / pipelined 数据），必须回放给后续 TLS 握手/隧道，
        // 否则握手从空流开始将挂死（明文路径已用 init 注入，此处此前被直接丢弃）
        let overflow = head_after_head_end(&head);
        let mut client = PrefixedStream::new(stream, overflow);
        if !ctx.host_in_targets(&host) || ctx.is_pinned(&host) {
            // 未配置解密的域名 / 已判定证书锁定的域：透明直通隧道（不解密不记请求日志）。
            // Charles「SSL Proxying Locations」/ mitmproxy「--ignore-hosts」同款语义。
            // 隧道路由与其他路径一致：上游优先、失败回退直连（见 tunnel_raw 注释）。
            tunnel_raw(client, &host, port, &upstream, &ctx.log).await;
            return;
        }
        let matched = ctx
            .targets
            .iter()
            .find(|d| **d == host || host.ends_with(&format!(".{d}")))
            .map(|s| s.as_str())
            .unwrap_or("?");
        // 先握手后记日志（与 Python 一致）：避免日志/证书等任何异常把 CONNECT
        // 握手拖死导致客户端 EOF（issue #7）
        if client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await.is_err() {
            return;
        }
        ctx.log.log(&format!("CONNECT {host}:{port}  [TRAE/MITM] 匹配域名: {matched}"));
        let acceptor = TlsAcceptor::from(ca.gen_server_config(&host));
        // 握手超时（审查修复：原无超时，客户端不发 ClientHello 时任务永久挂起）
        match timeout(HANDSHAKE_TIMEOUT, acceptor.accept(client)).await {
            Ok(Ok(tls)) => {
                ctx.note_handshake_ok(&host);
                serve_mitm(tls, host, port, ctx, mitm_client).await;
            }
            Ok(Err(e)) => {
                // 客户端主动中止（10053/eof）= 证书锁定或连接池竞争；节流记录防刷屏
                let aborted = e.to_string().contains("10053")
                    || e.to_string().contains("handshake eof")
                    || e.to_string().contains("close_notify")
                    || e.to_string().contains("unexpected EOF");
                if aborted && ctx.note_handshake_fail(&host) {
                    // 自适应降级：失败样本 ≥ PIN_MIN_FAILS 且失败率 > 75% → 判定
                    // 证书锁定，后续该域 CONNECT 透明直通（进程内生效，重启复位）
                    ctx.log.log(&format!(
                        "  [MITM] {host} 握手被客户端中止率达阈值（{PIN_MIN_FAILS}+ 次失败、成功率 <25%），疑似证书锁定，已自动降级为透明直通"
                    ));
                } else if !aborted {
                    ctx.log
                        .log(&format!("  [MITM] TLS 握手失败 {host}:{port}: {e}"));
                }
            }
            Err(_) => ctx.log.log(&format!(
                "  [MITM] TLS 握手超时 ({}s) {host}:{port}",
                HANDSHAKE_TIMEOUT.as_secs()
            )),
        }
    } else {
        // 明文 HTTP 请求：全部转发（日志由 handle_plain 内部按目标域名控制）
        handle_plain(stream, head, &plain_client, &ctx).await;
    }
}

/// 分发阶段读请求头（到 \r\n\r\n 或 EOF；EOF 时返回已有内容交由上层判路由，对齐 Python）
async fn read_head<S: AsyncRead + Unpin>(s: &mut S) -> Result<Vec<u8>, String> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    loop {
        if handler::find_head_end(&buf).is_some() {
            return Ok(buf);
        }
        let n = s.read(&mut chunk).await.map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(buf);
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_DISPATCH_HEAD {
            return Err(format!("请求头超过分发缓冲上限 ({MAX_DISPATCH_HEAD})"));
        }
    }
}

/// CONNECT 透明隧道（对齐 Python `tunnel_raw`）：不解密，仅记隧道级日志。
/// 路由与其他路径一致（见 upstream.rs 模块注释）：上游代理（用户 VPN）优先，
/// 失败回退直连——Python 版此路径只服务非目标域，本版还承接被降级为直通的
/// 目标域，路由同样必须镜像客户端正常出口（2026-09-15 实测：直连在此环境
/// 数据黑洞，上游 7890 是唯一活路）。
/// client 为 [`PrefixedStream`]（分发阶段超读字节的回放见 handle_conn 注释）。
async fn tunnel_raw<S>(
    mut client: S,
    host: &str,
    port: u16,
    upstream: &Option<UpstreamProxy>,
    log: &ProxyLog,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut pre: Option<TcpStream> = None;
    if let Some(up) = upstream {
        match connect_via_upstream(host, port, up).await {
            Ok(s) => {
                log.log(&format!("  [raw-tunnel] 经上游代理 {} 建立隧道 {host}:{port}", up.addr()));
                pre = Some(s);
            }
            Err(e) => log.log(&format!("  [raw-tunnel] 上游代理连接失败({e})，回退直连")),
        }
    }
    let mut remote = match pre {
        Some(r) => r,
        None => match connect_direct(host, port).await {
            Ok(s) => s,
            Err(e) => {
                // 建连不可达：明确告知客户端，避免浏览器无限等待
                log.log(&format!("  [raw-tunnel] 隧道建立失败 {host}:{port}: {e}"));
                let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                return;
            }
        },
    };
    // 完成 CONNECT 握手：先回 200，客户端随后才会发送 TLS ClientHello
    if client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await.is_err() {
        return;
    }
    // 双向裸转发（copy_bidirectional 自带半关闭传播，对齐 Python pipe + shutdown(SHUT_WR)）
    let _ = tokio::io::copy_bidirectional(&mut client, &mut remote).await;
}

/// 明文 HTTP 转发（对齐 Python `handle_plain`）：
/// 请求行为代理形式绝对 URL；路由与其他路径一致（上游优先、失败回退直连）。
/// 仅目标域名记操作日志与抓包日志；单请求后关闭连接（对齐 Python 语义）。
async fn handle_plain(
    mut stream: TcpStream,
    head: Vec<u8>,
    plain_client: &Client<UpstreamConnector, Full<Bytes>>,
    ctx: &ProxyCtx,
) {
    // 分发阶段已读取的字节作为初始缓冲注入（可能含 body 前缀）
    let init = bytes::BytesMut::from(&head[..]);
    let Some(req) = (match timeout(CONN_TIMEOUT, handler::read_raw_request_buf(&mut stream, init)).await {
        Ok(Ok(Some(r))) => Some(r),
        _ => None,
    }) else {
        return;
    };

    let Ok(uri) = req.path.parse::<hyper::Uri>() else {
        let _ = handler::send_response(&mut stream, 400, "Bad Request", &[], b"Bad Request").await;
        return;
    };
    let Some(host) = uri.host().map(str::to_string) else {
        let _ = handler::send_response(&mut stream, 400, "Bad Request", &[], b"Bad Request").await;
        return;
    };
    let scheme = uri.scheme_str().unwrap_or("http").to_string();
    let default_port = if scheme == "https" { 443 } else { 80 };
    let port = uri.port_u16().unwrap_or(default_port);
    let path = uri
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let is_target = ctx.host_in_targets(&host);
    if is_target {
        ctx.log.log(&format!("  [plain] {} {scheme}://{host}:{port}{path}", req.method));
    }
    // 上游路由（2026-09-15 重构）：与其他路径一致，上游优先、失败回退直连
    //（连接器内置；连接器按 scheme 自动选择绝对形式/CONNECT 隧道，见 upstream.rs）
    let client = plain_client;

    // 组装上游请求：过滤跳过头（host/content-length 由 hyper 依 URI/body 重写）
    let mut builder = Request::builder().method(req.method.as_str()).uri(uri.clone());
    for (k, v) in &req.headers {
        if HOP_BY_HOP_REQ.iter().any(|h| k.eq_ignore_ascii_case(h)) {
            continue;
        }
        if let (Ok(name), Ok(val)) = (k.parse::<HeaderName>(), v.parse::<HeaderValue>()) {
            builder = builder.header(name, val);
        }
    }
    // Python：GET 请求不带 body
    let body = if req.method.eq_ignore_ascii_case("GET") { Bytes::new() } else { req.body.clone() };
    let request = builder.body(Full::new(body)).expect("plain upstream request build");

    let resp = match timeout(PLAIN_TIMEOUT, client.request(request)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            if is_target {
                ctx.log
                    .log(&format!("  [plain] 错误: {} (host={host}, path={path})", handler::error_chain(&e)));
            }
            let _ = handler::send_response(&mut stream, 502, "Bad Gateway", &[], b"Bad Gateway").await;
            return;
        }
        Err(_) => {
            if is_target {
                ctx.log.log(&format!(
                    "  [plain] 错误: 上游超时 ({}s) (host={host}, path={path})",
                    PLAIN_TIMEOUT.as_secs()
                ));
            }
            let _ = handler::send_response(&mut stream, 504, "Gateway Timeout", &[], b"Gateway Timeout").await;
            return;
        }
    };

    let status = resp.status().as_u16();
    let reason = handler::reason_phrase(status);
    let resp_pairs: Vec<(String, String)> = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).to_string()))
        .collect();
    // 非流式整体缓冲：Limited 上限 + 逐帧空闲超时（差异修复：对齐 Python handle_plain
    // 30s socket timeout 的逐读语义，防 trickling body 挂住连接）
    match crate::device_proxy::handler::collect_body_with_idle_timeout(
        Limited::new(resp.into_body(), crate::device_proxy::handler::MAX_RESP_BODY),
        PLAIN_TIMEOUT,
    )
    .await
    {
        Ok(resp_body) => {
            if is_target {
                ctx.log.log(&format!(
                    "  [plain] <- {status} {reason} ({}) bytes from {host}{path}",
                    resp_body.len()
                ));
            }
            if handler::send_response(&mut stream, status, reason, &resp_pairs, &resp_body)
                .await
                .is_err()
            {
                return;
            }
            // 仅目标域名记录到代理请求日志（对齐 Python handle_plain）
            if is_target {
                ctx.req_logger.log_request(
                    &req.method,
                    &host,
                    &path,
                    &req.headers,
                    &req.body,
                    status,
                    reason,
                    &resp_pairs,
                    &resp_body,
                );
            }
        }
        Err(e) => {
            if is_target {
                ctx.log.log(&format!("  [plain] 错误: 读上游响应失败: {e} (host={host}, path={path})"));
            }
            let _ = handler::send_response(&mut stream, 502, "Bad Gateway", &[], b"Bad Gateway").await;
        }
    }
}

// ---------------- 启动辅助 ----------------

/// 提取请求头之后超读的字节（客户端在 CONNECT 200 应答前抢发的 ClientHello /
/// pipelined 数据），供 [`PrefixedStream`] 回放给后续 TLS 握手/隧道
fn head_after_head_end(head: &[u8]) -> Vec<u8> {
    match handler::find_head_end(head) {
        Some(pos) => head[pos + 4..].to_vec(),
        None => Vec::new(),
    }
}

/// 带前缀缓冲的流：读操作先耗尽前缀（分发阶段超读字节的回放）再透传底层流，
/// 写操作直接透传。审查修复：此前超读字节被直接丢弃，TLS 握手从空流开始会挂死。
struct PrefixedStream<S> {
    inner: S,
    prefix: Vec<u8>,
    pos: usize,
}

impl<S> PrefixedStream<S> {
    fn new(inner: S, prefix: Vec<u8>) -> Self {
        Self { inner, prefix, pos: 0 }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for PrefixedStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.pos < this.prefix.len() {
            let n = (this.prefix.len() - this.pos).min(buf.remaining());
            buf.put_slice(&this.prefix[this.pos..this.pos + n]);
            this.pos += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for PrefixedStream<S> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// 把 checkin_accounts.json 中各账号的 device 字段刷新为当前算法生成的设备标识
///（对齐 Python `sync_account_devices`：升级历史记录中由旧算法生成的假占位符
/// device_id 全 '2' / session_id 全 '5'；仅当字段确实变化时才写盘）
pub fn sync_account_devices(ctx: &ProxyCtx) {
    let _g = handler::accounts_lock().lock().unwrap_or_else(|e| e.into_inner());
    // SQLite 化（P3）：accounts 表 raw 读写（保留扩展字段与数字 uid 兼容语义）
    let mut cfg: serde_json::Value = crate::store::docs::accounts_load_raw(&crate::store::db(&ctx.data_dir));
    if !cfg.get("accounts").map(|a| a.is_array()).unwrap_or(false) {
        return;
    }
    let accounts = cfg
        .get_mut("accounts")
        .and_then(|a| a.as_array_mut())
        .expect("accounts array");
    let mut changed = false;
    for a in accounts.iter_mut() {
        let uid = a
            .get("UserID")
            .or_else(|| a.get("user_id"))
            .map(|v| match v {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Number(n) => n.to_string(),
                _ => String::new(),
            })
            .filter(|s| !s.is_empty());
        let Some(uid) = uid else { continue };
        let dev = crate::commands::accounts::derive_device(&uid);
        let same = a.get("device_id").and_then(|v| v.as_str()) == Some(dev.device_id.as_str())
            && a.get("session_id").and_then(|v| v.as_str()) == dev.session_id.as_deref()
            && a.get("market_user_id").and_then(|v| v.as_str()) == dev.market_user_id.as_deref();
        if same {
            continue;
        }
        a["device_id"] = serde_json::Value::String(dev.device_id.clone());
        a["session_id"] = dev
            .session_id
            .clone()
            .map(serde_json::Value::String)
            .unwrap_or(serde_json::Value::Null);
        a["market_user_id"] = dev
            .market_user_id
            .clone()
            .map(serde_json::Value::String)
            .unwrap_or(serde_json::Value::Null);
        changed = true;
    }
    if changed {
        if crate::store::docs::accounts_save_raw(&crate::store::db(&ctx.data_dir), &cfg).is_ok() {
            ctx.log.log("  [sync] 已刷新账号设备标识字段(旧算法升级)");
        } else {
            ctx.log.log("  [sync] 账号设备标识写入失败（未落库）");
        }
    }
}

/// Windows：WSASocketW + SO_EXCLUSIVEADDRUSE 独占绑定（选项必须在 bind 前设置，
/// 对齐 Python `srv.setsockopt(SOL_SOCKET, SO_EXCLUSIVEADDRUSE, 1)`，issue #7）；
/// 其他平台：常规绑定（不设 SO_REUSEADDR，同样拒绝同端口重复绑定）。
fn bind_exclusive(port: u16) -> Result<std::net::TcpListener, String> {
    #[cfg(target_os = "windows")]
    unsafe {
        use std::os::windows::io::FromRawSocket;
        use windows_sys::Win32::Networking::WinSock::{
            bind as ws_bind, closesocket, listen as ws_listen, setsockopt, WSAGetLastError, WSASocketW,
            AF_INET, IN_ADDR, IN_ADDR_0, IN_ADDR_0_0, INVALID_SOCKET, IPPROTO_TCP, SOCKADDR, SOCKADDR_IN,
            SOCK_STREAM, SO_EXCLUSIVEADDRUSE, SOL_SOCKET, WSA_FLAG_OVERLAPPED,
        };
        let sock = WSASocketW(
            AF_INET as i32,
            SOCK_STREAM,
            IPPROTO_TCP,
            std::ptr::null(),
            0,
            WSA_FLAG_OVERLAPPED,
        );
        if sock == INVALID_SOCKET {
            return Err(format!("创建监听 socket 失败: WSA错误 {}", WSAGetLastError()));
        }
        // 独占绑定：多个 socket 绑定同一端口将明确失败（Python 版同款修复）
        let on: u32 = 1;
        if setsockopt(
            sock,
            SOL_SOCKET,
            SO_EXCLUSIVEADDRUSE,
            &on as *const u32 as *const u8,
            std::mem::size_of::<u32>() as i32,
        ) != 0
        {
            let err = WSAGetLastError();
            closesocket(sock);
            return Err(format!("设置 SO_EXCLUSIVEADDRUSE 失败: WSA错误 {err}"));
        }
        let addr = SOCKADDR_IN {
            sin_family: AF_INET,
            sin_port: port.to_be(),
            sin_addr: IN_ADDR {
                S_un: IN_ADDR_0 {
                    S_un_b: IN_ADDR_0_0 { s_b1: 127, s_b2: 0, s_b3: 0, s_b4: 1 },
                },
            },
            sin_zero: [0; 8],
        };
        if ws_bind(
            sock,
            &addr as *const SOCKADDR_IN as *const SOCKADDR,
            std::mem::size_of::<SOCKADDR_IN>() as i32,
        ) != 0
        {
            let err = WSAGetLastError();
            closesocket(sock);
            return Err(format!("绑定 127.0.0.1:{port} 失败: WSA错误 {err}"));
        }
        if ws_listen(sock, 128) != 0 {
            let err = WSAGetLastError();
            closesocket(sock);
            return Err(format!("listen 失败: WSA错误 {err}"));
        }
        // fd 所有权转交 std（后续转 tokio 异步轮询）
        Ok(std::net::TcpListener::from_raw_socket(sock as u64))
    }
    #[cfg(target_os = "macos")]
    {
        // F-75 M2-2.3 mac 分支：显式 SO_REUSEADDR（防 TIME_WAIT 残留导致代理重启
        // 绑定失败——与 Windows 独占语义方向相反但同为「重启必成功」服务目标）。
        // tokio TcpSocket 实现零新增依赖（socket2/libc 均不必引入）。
        use std::net::Ipv4Addr;
        let socket = tokio::net::TcpSocket::new_v4()
            .map_err(|e| format!("创建监听套接字失败: {e}"))?;
        socket
            .set_reuseaddr(true)
            .map_err(|e| format!("设置 SO_REUSEADDR 失败: {e}"))?;
        let addr = std::net::SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        // 审查修复（P0-1，整体黑盒复审）：tokio 1.53 TcpSocket::bind 仅绑定端口返回
        // io::Result<()>（socket.rs:805），监听器由 listen(backlog) 产出（:906）——
        // 原写法 `bind(...)?.into_std()` 对 () 调用不存在的方法，mac 构建 E0599
        socket
            .bind(addr)
            .map_err(|e| format!("绑定 127.0.0.1:{port} 失败: {e}"))?;
        socket
            .listen(128)
            .map_err(|e| format!("监听 127.0.0.1:{port} 失败: {e}"))?
            .into_std()
            .map_err(|e| format!("监听器转入阻塞模式失败: {e}"))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        std::net::TcpListener::bind(("127.0.0.1", port))
            .map_err(|e| format!("绑定 127.0.0.1:{port} 失败: {e}"))
    }
}

async fn bind_listener(port: u16) -> Result<TcpListener, String> {
    let std_listener = bind_exclusive(port)?;
    // tokio from_std 契约要求非阻塞模式；Windows 上 tokio 无法检测阻塞 socket 而
    // 静默放行（util/blocking_check.rs 对非 unix 直接 Ok）。阻塞式 listener 会让
    // mio 的 accept/read 在无数据时直接阻塞 worker 线程（accepted socket 继承本
    // 模式），空闲预连接打满运行时后 IO driver 停摆、代理整体冻结——e2e 测试挂起
    // 根因，生产环境豆包 preconnect 空闲连接同样会让 worker 逐个阻塞。
    std_listener
        .set_nonblocking(true)
        .map_err(|e| format!("设置监听器非阻塞模式失败: {e}"))?;
    TcpListener::from_std(std_listener).map_err(|e| format!("监听器初始化失败: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aiwork_proxy_test_{}_{}", tag, std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        d
    }

    fn test_ctx(dir: &PathBuf) -> ProxyCtx {
        let captured = Arc::new(AtomicI64::new(0));
        ProxyCtx {
            log: ProxyLog::new(dir.join("proxy.log"), None, captured),
            req_logger: Arc::new(RequestLogger::new(dir.clone())),
            targets: DEFAULT_TARGETS.iter().map(|s| s.to_string()).collect(),
            auto_capture_jwt: true,
            data_dir: dir.clone(),
            upstream: None,
            pin_state: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// 解密白名单宽后缀语义（对齐 Python host_in_targets）：配置 `trae.cn` 需同时
    /// 命中根域名（@/trae.cn 自身）与任意层级子域名（*.trae.cn）；大小写不敏感；
    /// 非相关域名与「以配置域为尾部但不构成独立标签」的域名不得命中
    #[test]
    fn host_in_targets_wide_suffix_semantics() {
        let dir = temp_dir("targets");
        let ctx = test_ctx(&dir);
        // 根域名（@）
        assert!(ctx.host_in_targets("trae.cn"));
        assert!(ctx.host_in_targets("mchost.guru"));
        // 任意层级子域名（*）
        assert!(ctx.host_in_targets("api.trae.cn"));
        assert!(ctx.host_in_targets("a.b.api5-normal.trae.cn"));
        assert!(ctx.host_in_targets("www.doubao.com"));
        // 大小写不敏感（CONNECT host 大小写由客户端决定）
        assert!(ctx.host_in_targets("API.TRAE.CN"));
        assert!(ctx.host_in_targets("Www.Doubao.COM"));
        // 非相关域名
        assert!(!ctx.host_in_targets("example.com"));
        // 尾部字符串相同但非独立标签的域名不得命中（防 eviltrae.cn 绕过/误伤）
        assert!(!ctx.host_in_targets("eviltrae.cn"));
        assert!(!ctx.host_in_targets("notdoubao.com"));
    }

    /// 自适应证书锁定降级（失败率判定）：真锁定域快速降级，投机预连接裁撤
    /// （混布成功/失败）不误判
    #[test]
    fn adaptive_pin_downgrade() {
        let dir = temp_dir("pin");
        let ctx = test_ctx(&dir);
        assert!(!ctx.is_pinned("mcs.doubao.com"));
        // 少量失败不降级（投机预连接裁撤是正常现象）
        for _ in 0..4 {
            ctx.note_handshake_fail("mcs.doubao.com");
        }
        assert!(!ctx.is_pinned("mcs.doubao.com"));
        // 第 5 次失败（成功数 0，失败率 100%）→ 判定锁定
        assert!(ctx.note_handshake_fail("mcs.doubao.com"));
        assert!(ctx.is_pinned("mcs.doubao.com"));
        // 其他域不受影响
        assert!(!ctx.is_pinned("www.doubao.com"));
        // 混布域（webview 成功 + cronet 失败各半）：失败率高也不误判
        for _ in 0..10 {
            ctx.note_handshake_ok("www.doubao.com");
            ctx.note_handshake_fail("www.doubao.com");
        }
        assert!(!ctx.is_pinned("www.doubao.com"));
    }

    /// 假占位符（旧算法）设备字段应被刷新为当前算法值，且二次同步幂等
    #[test]
    fn sync_refreshes_placeholder_devices() {
        let dir = temp_dir("sync");
        let ctx = test_ctx(&dir);
        crate::store::docs::accounts_save_raw(
            &crate::store::db(&dir),
            &serde_json::json!({
                "accounts": [
                    {"name": "a", "UserID": "4487568582777872", "device_id": "222222222222222",
                     "session_id": "55555555555555555555555555555555"},
                    {"name": "b", "user_id": 12345, "device_id": "222222222222222"}
                ]
            }),
        )
        .unwrap();
        sync_account_devices(&ctx);
        let cfg: serde_json::Value =
            crate::store::docs::accounts_load_raw(&crate::store::db(&dir));
        let acc = &cfg["accounts"];
        let dev = crate::commands::accounts::derive_device("4487568582777872");
        assert_eq!(acc[0]["device_id"].as_str().unwrap(), dev.device_id);
        assert_eq!(acc[0]["session_id"].as_str(), dev.session_id.as_deref());
        assert_eq!(
            acc[1]["device_id"].as_str().unwrap(),
            crate::commands::accounts::derive_device("12345").device_id
        );
        // 二次同步应无变化（幂等：设备字段不再变化）
        sync_account_devices(&ctx);
        let after: serde_json::Value = crate::store::docs::accounts_load_raw(&crate::store::db(&dir));
        assert_eq!(cfg["accounts"][0]["device_id"], after["accounts"][0]["device_id"]);
    }

    /// 缺 accounts 数组 / 空表时静默返回，不报错不写库
    #[test]
    fn sync_tolerates_missing_accounts() {
        let dir = temp_dir("sync_empty");
        let ctx = test_ctx(&dir);
        sync_account_devices(&ctx); // 空表
        sync_account_devices(&ctx); // 再次（幂等）
    }

    /// Windows 独占绑定：同端口二次 bind 必须失败（issue #7 防孤儿进程假启动）
    #[cfg(target_os = "windows")]
    #[test]
    fn exclusive_bind_rejects_double_bind() {
        // 测试进程可能尚未初始化 WinSock（WSASocketW 需 WSAStartup，否则 10093）：
        // 先建一个 std 套接字触发进程级初始化，再测独占绑定
        drop(std::net::TcpListener::bind("127.0.0.1:0").unwrap());
        let first = bind_exclusive(0).expect("首次绑定(临时端口)应成功");
        let port = first.local_addr().unwrap().port();
        assert!(bind_exclusive(port).is_err(), "同端口二次绑定应失败");
    }

    /// head 之后的超读字节必须完整提取（空 head / 无超读 / 带超读三态）
    #[test]
    fn head_after_head_end_extracts_overflow() {
        assert_eq!(head_after_head_end(b"CONNECT a.com:443 HTTP/1.1\r\n\r\n"), b"");
        assert_eq!(
            head_after_head_end(b"GET / HTTP/1.1\r\nHost: a\r\n\r\nEXTRA-BYTES"),
            b"EXTRA-BYTES"
        );
        assert_eq!(head_after_head_end(b"partial-no-head-end"), b"");
    }

    /// PrefixedStream：先耗尽前缀（超读字节回放）再透传底层流（审查修复回归）
    #[tokio::test]
    async fn prefixed_stream_replays_overflow_then_inner() {
        let (mut client, server) = tokio::io::duplex(64);
        client.write_all(b"flow").await.unwrap();
        let mut s = PrefixedStream::new(server, b"over".to_vec());
        let mut buf = [0u8; 16];
        let n = s.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"over", "前缀字节优先回放");
        let n = s.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"flow", "前缀耗尽后透传底层流");
    }
}
