//! 上游连接（device_proxy.py 迁移，P4-4）。
//!
//! 路由策略（2026-09-15 重构：上游优先，镜像客户端无代理时的正常路径）：
//! - 配置了上游代理（用户 VPN）时，**所有转发流量一律先经上游**（CONNECT 隧道 /
//!   SOCKS5 / http 绝对形式），失败回退直连 —— 不开启本代理时客户端流量本就
//!   走系统代理（用户 VPN），MITM 只是解密层，出口路径必须与其一致。
//!   实测教训（proxy.log 22:21）：VPN 客户端接管路由/DNS 的环境下本进程直连
//!   目标域 47 个请求 0 响应（TCP 可连但数据黑洞），而经 127.0.0.1:7890 全程正常；
//!   语义对齐 mitmproxy `--mode upstream:` / Charles 上游代理。
//! - 未配置上游：全部直连（无 VPN 用户，行为不变）。
//!
//! `UpstreamConnector` 实现 `tower::Service<Uri>`（hyper legacy Client 的连接器接口）：
//! - https 目标：TCP（经隧道或直连）→ rustls（webpki roots，ALPN 仅 http/1.1，对齐 Python 无 h2）
//! - http 目标经 HTTP 代理：直连代理并标记 is_proxied → hyper 自动改发绝对 URL 形式（对齐 Python）
//! - http 目标经 SOCKS5/直连：普通连接，origin-form

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use hyper::Uri;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use hyper_util::client::legacy::connect::{Connected, Connection};
use hyper_util::rt::TokioIo;

/// 建连超时（对齐 Python 各处 socket timeout=30s）
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// 上游代理：addr 为 "host:port"（默认端口 http=8080 / socks5=1080，对齐 _split_host_port）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamProxy {
    Http(String),
    Socks5(String),
}

impl UpstreamProxy {
    /// 展示用地址（日志）
    pub fn addr(&self) -> &str {
        match self {
            UpstreamProxy::Http(a) | UpstreamProxy::Socks5(a) => a,
        }
    }
}

/// 域名后缀匹配（对齐 handler::ProxyCtx::host_in_targets 语义）：
/// `d` 命中根域自身与任意层级子域，大小写不敏感
pub fn host_matches_domain(host: &str, domain: &str) -> bool {
    let h = host.to_ascii_lowercase();
    h == domain || h.ends_with(&format!(".{domain}"))
}

/// 解析上游代理规格，对齐 Python `_parse_upstream`：
/// 兼容 Windows 系统代理 ProxyServer 的多种写法：
/// - `127.0.0.1:7890`               -> http
/// - `http=127.0.0.1:7890`          -> http
/// - `socks=127.0.0.1:7891`         -> socks5
/// - `http=...;https=...;socks=...` -> 优先 socks5，其次 http
pub fn parse_upstream(spec: &str) -> Option<UpstreamProxy> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }
    let mut socks: Option<String> = None;
    let mut http: Option<String> = None;
    for p in spec.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        if let Some((k, v)) = p.split_once('=') {
            match k.trim().to_ascii_lowercase().as_str() {
                "socks" | "socks5" => socks = Some(v.trim().to_string()),
                "http" | "https" => http = http.clone().or_else(|| Some(v.trim().to_string())),
                _ => {}
            }
        } else {
            http = http.clone().or_else(|| Some(p.to_string()));
        }
    }
    if let Some(s) = socks {
        return Some(UpstreamProxy::Socks5(s));
    }
    http.map(UpstreamProxy::Http)
}

/// "host:port" 拆分；无端口用 default_port（对齐 Python `_split_host_port`）
fn split_host_port(addr: &str, default_port: u16) -> (String, u16) {
    let addr = addr.trim();
    match addr.rsplit_once(':') {
        Some((h, p)) => match p.parse::<u16>() {
            Ok(p) => (h.to_string(), p),
            Err(_) => (addr.to_string(), default_port),
        },
        None => (addr.to_string(), default_port),
    }
}

/// 经上游代理建立到 (host, port) 的 TCP 隧道（对齐 Python `connect_via_upstream`）。
/// 支持 HTTP 代理的 CONNECT，以及 SOCKS5（无认证 / 用户名密码）。
pub async fn connect_via_upstream(host: &str, port: u16, upstream: &UpstreamProxy) -> Result<TcpStream, String> {
    match upstream {
        UpstreamProxy::Http(addr) => connect_http_tunnel(host, port, addr).await,
        UpstreamProxy::Socks5(addr) => connect_socks5(host, port, addr).await,
    }
}

/// HTTP 代理 CONNECT 隧道
async fn connect_http_tunnel(host: &str, port: u16, addr: &str) -> Result<TcpStream, String> {
    let (uh, up) = split_host_port(addr, 8080);
    let mut s = timeout(CONNECT_TIMEOUT, TcpStream::connect((uh.as_str(), up)))
        .await
        .map_err(|_| format!("连接上游代理 {uh}:{up} 超时"))?
        .map_err(|e| format!("连接上游代理 {uh}:{up} 失败: {e}"))?;
    // 握手阶段限时（审查修复：原读响应无超时，代理僵死时任务永久挂起占住并发槽）
    timeout(CONNECT_TIMEOUT, http_connect_handshake(&mut s, host, port))
        .await
        .map_err(|_| format!("上游代理 {uh}:{up} CONNECT 握手超时"))??;
    Ok(s)
}

/// HTTP 代理 CONNECT 握手：发送 CONNECT + 读响应头 + 校验 200
async fn http_connect_handshake(s: &mut TcpStream, host: &str, port: u16) -> Result<(), String> {
    let req = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\nProxy-Connection: keep-alive\r\n\r\n");
    s.write_all(req.as_bytes())
        .await
        .map_err(|e| format!("发送 CONNECT 失败: {e}"))?;

    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 4096];
    loop {
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        let n = s.read(&mut chunk).await.map_err(|e| format!("读 CONNECT 响应失败: {e}"))?;
        if n == 0 {
            return Err("上游代理无响应".to_string());
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > 16 * 1024 {
            return Err("上游代理 CONNECT 响应头过长".to_string());
        }
    }
    let head = String::from_utf8_lossy(&buf).to_string();
    let status = head.lines().next().unwrap_or("");
    if !(status.starts_with("HTTP/1.1 200") || status.starts_with("HTTP/1.0 200")) {
        return Err(format!("上游代理拒绝 CONNECT: {status}"));
    }
    Ok(())
}

/// SOCKS5 隧道（无认证 / 用户名密码；认证取 UPSTREAM_PROXY_USER/PASS 环境变量，对齐 Python）
async fn connect_socks5(host: &str, port: u16, addr: &str) -> Result<TcpStream, String> {
    let (uh, up) = split_host_port(addr, 1080);
    let mut s = timeout(CONNECT_TIMEOUT, TcpStream::connect((uh.as_str(), up)))
        .await
        .map_err(|_| format!("连接 SOCKS5 代理 {uh}:{up} 超时"))?
        .map_err(|e| format!("连接 SOCKS5 代理 {uh}:{up} 失败: {e}"))?;
    // 握手阶段整体限时（审查修复：原各 read_exact 无超时，代理僵死时任务永久挂起）
    timeout(CONNECT_TIMEOUT, socks5_handshake(&mut s, host, port))
        .await
        .map_err(|_| format!("SOCKS5 代理 {uh}:{up} 握手超时"))??;
    Ok(s)
}

/// SOCKS5 握手：方法协商（无认证/用户名密码）→ CONNECT → 跳过 BND 尾部
async fn socks5_handshake(s: &mut TcpStream, host: &str, port: u16) -> Result<(), String> {
    // 握手：提供 无认证 + 用户名密码 两种方式
    s.write_all(&[0x05, 0x02, 0x00, 0x02])
        .await
        .map_err(|e| format!("SOCKS5 发送握手失败: {e}"))?;
    let mut greet = [0u8; 2];
    s.read_exact(&mut greet).await.map_err(|e| format!("SOCKS5 握手失败: {e}"))?;
    if greet[0] != 0x05 {
        return Err("SOCKS5 握手失败".to_string());
    }
    match greet[1] {
        0x02 => {
            // 用户名密码子协商（RFC 1929）
            let user = std::env::var("UPSTREAM_PROXY_USER").unwrap_or_default().into_bytes();
            let pwd = std::env::var("UPSTREAM_PROXY_PASS").unwrap_or_default().into_bytes();
            let mut auth = vec![0x01u8, user.len().min(255) as u8];
            auth.extend_from_slice(&user[..user.len().min(255)]);
            auth.push(pwd.len().min(255) as u8);
            auth.extend_from_slice(&pwd[..pwd.len().min(255)]);
            s.write_all(&auth).await.map_err(|e| format!("SOCKS5 发送认证失败: {e}"))?;
            let mut rep = [0u8; 2];
            s.read_exact(&mut rep).await.map_err(|e| format!("SOCKS5 认证读响应失败: {e}"))?;
            if rep[1] != 0x00 {
                return Err("SOCKS5 认证失败".to_string());
            }
        }
        0x00 => {}
        m => return Err(format!("SOCKS5 不支持的认证方式: {m}")),
    }

    // CONNECT（域名寻址；IPv6 字面量不支持，对齐 Python）
    if host.contains(':') {
        return Err("暂不支持 SOCKS5 IPv6".to_string());
    }
    let host_b = host.as_bytes();
    let mut req = vec![0x05u8, 0x01, 0x00, 0x03, host_b.len() as u8];
    req.extend_from_slice(host_b);
    req.extend_from_slice(&port.to_be_bytes());
    s.write_all(&req).await.map_err(|e| format!("SOCKS5 发送 CONNECT 失败: {e}"))?;

    let mut head = [0u8; 4];
    s.read_exact(&mut head).await.map_err(|e| format!("SOCKS5 CONNECT 读响应失败: {e}"))?;
    if head[1] != 0x00 {
        return Err(format!("SOCKS5 CONNECT 失败: code={}", head[1]));
    }
    // 跳过 BND.ADDR + BND.PORT
    match head[3] {
        0x01 => {
            let mut rest = [0u8; 6];
            s.read_exact(&mut rest).await.map_err(|e| format!("SOCKS5 读尾部失败: {e}"))?;
        }
        0x03 => {
            let mut n = [0u8; 1];
            s.read_exact(&mut n).await.map_err(|e| format!("SOCKS5 读域名长度失败: {e}"))?;
            let mut rest = vec![0u8; n[0] as usize + 2];
            s.read_exact(&mut rest).await.map_err(|e| format!("SOCKS5 读尾部失败: {e}"))?;
        }
        0x04 => {
            let mut rest = [0u8; 18];
            s.read_exact(&mut rest).await.map_err(|e| format!("SOCKS5 读尾部失败: {e}"))?;
        }
        _ => return Err("SOCKS5 CONNECT 响应地址类型非法".to_string()),
    }
    Ok(())
}

/// 直连目标（MITM/WS 上游与回退路径共用）
pub async fn connect_direct(host: &str, port: u16) -> Result<TcpStream, String> {
    timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port)))
        .await
        .map_err(|_| format!("连接 {host}:{port} 超时"))?
        .map_err(|e| format!("连接 {host}:{port} 失败: {e}"))
}

/// 在既有 TCP 流上完成 TLS 握手（webpki roots + ALPN http/1.1，与 MITM 转发路径
/// 同款客户端配置），限时 30s（对齐 Python wrap_socket 共享 socket timeout）
async fn tls_wrap(
    host: &str,
    port: u16,
    tcp: TcpStream,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>, String> {
    let server_name = tokio_rustls::rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| format!("目标主机名非法 {host}: {e}"))?;
    let connector = tokio_rustls::TlsConnector::from(Arc::new(build_client_tls_config()));
    timeout(CONNECT_TIMEOUT, connector.connect(server_name, tcp))
        .await
        .map_err(|_| format!("与 {host}:{port} 的 TLS 握手超时"))?
        .map_err(|e| format!("与 {host}:{port} 的 TLS 握手失败: {e}"))
}

/// 直连目标并完成 TLS 握手（无上游配置时的 WS 升级路径）
pub async fn connect_tls_direct(
    host: &str,
    port: u16,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>, String> {
    let tcp = connect_direct(host, port).await?;
    tls_wrap(host, port, tcp).await
}

/// 上游优先建立 TLS 连接（WS 升级路径，路由对齐 MITM 转发：见模块注释）。
/// 白名单域（direct_domains 命中）直连优先、失败回退上游；其余有上游 →
/// CONNECT/SOCKS5 隧道后包 TLS，失败回退直连；无上游 → 直连。
pub async fn connect_tls_upstream_first(
    host: &str,
    port: u16,
    upstream: Option<&UpstreamProxy>,
    direct_domains: &[String],
    log: &crate::device_proxy::logger::ProxyLog,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>, String> {
    // 直连白名单域（p3-2e）：qoder 等国内域经用户 VPN 实测全挂、直连恢复
    //（2026-09-30 IDE chat 链路），命中域先直连
    if direct_domains.iter().any(|d| host_matches_domain(host, d)) {
        return match connect_tls_direct(host, port).await {
            Ok(t) => Ok(t),
            Err(e) => {
                log.log(&format!(
                    "  [upstream] 直连白名单域 {host}:{port} 失败({e})，回退上游"
                ));
                match upstream {
                    Some(up) => match connect_via_upstream(host, port, up).await {
                        Ok(tcp) => tls_wrap(host, port, tcp).await,
                        Err(e2) => Err(format!("直连({e})与上游({e2})均失败")),
                    },
                    None => Err(e),
                }
            }
        };
    }
    match upstream {
        None => connect_tls_direct(host, port).await,
        Some(up) => match connect_via_upstream(host, port, up).await {
            Ok(tcp) => tls_wrap(host, port, tcp).await,
            // 上游不可达回退直连（对齐其余路径回退语义）；直连也失败时报直连
            // 错误（此时网络本身不可达，直连错误信息对用户更有诊断价值）
            Err(_) => connect_tls_direct(host, port).await,
        },
    }
}

// ---------------- hyper legacy Client 连接器 ----------------

/// 上游连接流：供 hyper legacy Client 使用（MITM 解密后向真实服务器发起请求）
pub enum UpstreamStream {
    Plain(TokioIo<TcpStream>),
    /// http 明文经 HTTP 上游代理转发（hyper 据此改发绝对 URL 形式，对齐 Python）
    Proxied(TokioIo<TcpStream>),
    Tls(TokioIo<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl Connection for UpstreamStream {
    fn connected(&self) -> Connected {
        match self {
            UpstreamStream::Proxied(_) => Connected::new().proxy(true),
            // 隧道内/TLS 流均为对目标主机的 origin-form
            UpstreamStream::Plain(_) | UpstreamStream::Tls(_) => Connected::new(),
        }
    }
}

// hyper legacy Client 要求连接器响应流实现 hyper::rt::Read/Write（TokioIo 均已实现，
// 此处按变体逐一分发委托）
impl hyper::rt::Read for UpstreamStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            // Plain/Proxied 内型一致可合并；Tls 内型不同必须单列（| 模式要求绑定同型）
            UpstreamStream::Plain(io) | UpstreamStream::Proxied(io) => Pin::new(io).poll_read(cx, buf),
            UpstreamStream::Tls(io) => Pin::new(io).poll_read(cx, buf),
        }
    }
}

impl hyper::rt::Write for UpstreamStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            UpstreamStream::Plain(io) | UpstreamStream::Proxied(io) => Pin::new(io).poll_write(cx, buf),
            UpstreamStream::Tls(io) => Pin::new(io).poll_write(cx, buf),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            UpstreamStream::Plain(io) | UpstreamStream::Proxied(io) => {
                Pin::new(io).poll_write_vectored(cx, bufs)
            }
            UpstreamStream::Tls(io) => Pin::new(io).poll_write_vectored(cx, bufs),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            UpstreamStream::Plain(io) | UpstreamStream::Proxied(io) => io.is_write_vectored(),
            UpstreamStream::Tls(io) => io.is_write_vectored(),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            UpstreamStream::Plain(io) | UpstreamStream::Proxied(io) => Pin::new(io).poll_flush(cx),
            UpstreamStream::Tls(io) => Pin::new(io).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            UpstreamStream::Plain(io) | UpstreamStream::Proxied(io) => Pin::new(io).poll_shutdown(cx),
            UpstreamStream::Tls(io) => Pin::new(io).poll_shutdown(cx),
        }
    }
}

impl std::fmt::Debug for UpstreamStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpstreamStream::Plain(_) => f.write_str("UpstreamStream::Plain"),
            UpstreamStream::Proxied(_) => f.write_str("UpstreamStream::Proxied"),
            UpstreamStream::Tls(_) => f.write_str("UpstreamStream::Tls"),
        }
    }
}

/// hyper legacy Client 连接器：按 dst 的 scheme 决定是否包 TLS（webpki roots，ALPN 仅 http/1.1）。
/// 路由（见模块注释）：有上游 → 经上游隧道/绝对形式，失败回退直连；无上游 → 直连。
/// - https：经上游 CONNECT/SOCKS5 隧道（或直连）后包 TLS
/// - http：HTTP 上游 -> 直连代理并标记 is_proxied（hyper 发绝对 URL）；
///   SOCKS5 上游/无上游 -> 直连目标普通连接
#[derive(Clone)]
pub struct UpstreamConnector {
    upstream: Option<UpstreamProxy>,
    tls: Arc<tokio_rustls::rustls::ClientConfig>,
    log: crate::device_proxy::logger::ProxyLog,
    /// 直连白名单域（后缀匹配，p3-2e）：https 目标命中时跳过上游 VPN 直接连目标。
    /// qoder 国内域经用户 VPN 实测全挂（IDE chat 链路 2026-09-30）、直连恢复——
    /// MITM 解密层不得改变此类域的出口路径。
    direct_domains: Arc<Vec<String>>,
}

impl UpstreamConnector {
    pub fn new(
        upstream: Option<UpstreamProxy>,
        log: crate::device_proxy::logger::ProxyLog,
        direct_domains: Arc<Vec<String>>,
    ) -> Self {
        Self { upstream, tls: Arc::new(build_client_tls_config()), log, direct_domains }
    }
}

/// rustls 客户端配置：webpki roots + ALPN 仅 http/1.1（对齐 Python：客户端不协商 h2）
fn build_client_tls_config() -> tokio_rustls::rustls::ClientConfig {
    let provider = Arc::new(tokio_rustls::rustls::crypto::aws_lc_rs::default_provider());
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    tokio_rustls::rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth()
}

impl tower_service::Service<Uri> for UpstreamConnector {
    type Response = UpstreamStream;
    type Error = String;
    type Future = Pin<Box<dyn Future<Output = Result<UpstreamStream, String>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, dst: Uri) -> Self::Future {
        let this = self.clone();
        Box::pin(async move { this.connect(dst).await })
    }
}

impl UpstreamConnector {
    async fn connect(self, dst: Uri) -> Result<UpstreamStream, String> {
        let host = dst.host().ok_or("目标 URI 缺少 host")?.to_string();
        let port = dst.port_u16().unwrap_or(if dst.scheme_str() == Some("https") { 443 } else { 80 });
        let use_tls = dst.scheme_str() == Some("https");

        // http 明文 + HTTP 上游：直连代理，由 hyper 以绝对 URL 形式发请求（is_proxied）；
        // 上游不可达时回退直连（对齐 Python handle_plain 的回退语义）
        let mut proxy_fell_back = false;
        if !use_tls {
            if let Some(UpstreamProxy::Http(addr)) = &self.upstream {
                let (uh, up) = split_host_port(addr, 8080);
                match connect_direct(&uh, up).await {
                    Ok(s) => return Ok(UpstreamStream::Proxied(TokioIo::new(s))),
                    Err(e) => {
                        self.log
                            .log(&format!("  [upstream] HTTP 上游 {uh}:{up} 不可达({e})，回退直连"));
                        proxy_fell_back = true;
                    }
                }
            }
        }

        // 建连：直连白名单域优先直连（失败回退上游兜底）；否则直连，或经上游隧道
        //（失败回退直连，对齐 Python 各处回退语义）
        let tcp = if use_tls
            && self.direct_domains.iter().any(|d| host_matches_domain(&host, d))
        {
            match connect_direct(&host, port).await {
                Ok(s) => s,
                Err(e) => {
                    self.log.log(&format!(
                        "  [upstream] 直连白名单域 {host}:{port} 失败({e})，回退上游"
                    ));
                    match &self.upstream {
                        Some(up) => match connect_via_upstream(&host, port, up).await {
                            Ok(s) => s,
                            Err(e2) => return Err(format!("直连({e})与上游({e2})均失败")),
                        },
                        None => return Err(e),
                    }
                }
            }
        } else if proxy_fell_back {
            connect_direct(&host, port).await?
        } else {
            match &self.upstream {
                Some(up) => match connect_via_upstream(&host, port, up).await {
                    Ok(s) => s,
                    Err(e) => {
                        self.log.log(&format!("  [upstream] 经上游建连 {host}:{port} 失败({e})，回退直连"));
                        connect_direct(&host, port).await?
                    }
                },
                None => connect_direct(&host, port).await?,
            }
        };

        if !use_tls {
            return Ok(UpstreamStream::Plain(TokioIo::new(tcp)));
        }
        let server_name = tokio_rustls::rustls::pki_types::ServerName::try_from(host.clone())
            .map_err(|e| format!("目标主机名非法 {host}: {e}"))?;
        let connector = tokio_rustls::TlsConnector::from(self.tls);
        // TLS 握手限时（对齐 connect_tls_direct / Python socket timeout；裸 connect
        // 无超时时 TCP 通但 TLS 僵死会占住请求直至上层 300s 流式超时）
        let tls = timeout(CONNECT_TIMEOUT, connector.connect(server_name, tcp))
            .await
            .map_err(|_| format!("与 {host}:{port} 的 TLS 握手超时"))?
            .map_err(|e| format!("与 {host}:{port} 的 TLS 握手失败: {e}"))?;
        Ok(UpstreamStream::Tls(TokioIo::new(tls)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_upstream_aligns_python() {
        assert_eq!(parse_upstream(""), None);
        assert_eq!(parse_upstream("127.0.0.1:7890"), Some(UpstreamProxy::Http("127.0.0.1:7890".into())));
        assert_eq!(
            parse_upstream("http=127.0.0.1:7890"),
            Some(UpstreamProxy::Http("127.0.0.1:7890".into()))
        );
        assert_eq!(
            parse_upstream("socks=127.0.0.1:7891"),
            Some(UpstreamProxy::Socks5("127.0.0.1:7891".into()))
        );
        // 优先 socks5
        assert_eq!(
            parse_upstream("http=127.0.0.1:7890;https=127.0.0.1:7890;socks=127.0.0.1:7891"),
            Some(UpstreamProxy::Socks5("127.0.0.1:7891".into()))
        );
        // 无 socks 时取首个 http
        assert_eq!(
            parse_upstream("https=10.0.0.2:8443;http=10.0.0.1:8080"),
            Some(UpstreamProxy::Http("10.0.0.2:8443".into()))
        );
    }

    #[test]
    fn split_host_port_defaults() {
        assert_eq!(split_host_port("1.2.3.4", 8080), ("1.2.3.4".to_string(), 8080));
        assert_eq!(split_host_port("1.2.3.4:7897", 8080), ("1.2.3.4".to_string(), 7897));
        // 非数字端口：整体视为 host
        assert_eq!(split_host_port("1.2.3.4:abc", 1080), ("1.2.3.4:abc".to_string(), 1080));
    }
}
