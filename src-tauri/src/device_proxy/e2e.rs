//! 端到端测试：真实 [`ProxyServer`] + 假上游代理 + 假后端，模拟客户端走代理完整链路。
//!
//! 背景（豆包白屏问题，proxy.log 22:21）：「目标域直连」策略在用户 VPN 客户端
//! 接管路由/DNS 的环境下全线失效——47 个 MITM 转发请求 0 响应（TCP 可连但数据
//! 黑洞），而经上游 7890 全程正常。修复为「上游优先、失败回退直连」（镜像客户端
//! 无本代理时的正常出口，语义对齐 mitmproxy `--mode upstream:`）。
//!
//! 本组测试锁定路由契约（全部离线，不依赖真实网络）：
//! 1. [`target_plain_http_routes_via_upstream`]：目标域明文请求经上游转发
//!    （修复前走直连、上游标志不亮——本测试在旧代码上必然失败，是真回归测试）
//! 2. [`nontarget_plain_http_routes_via_upstream`]：非目标域经上游（Python 原语义保持）
//! 3. [`connect_tunnel_routes_via_upstream`]：CONNECT 透明隧道（含降级目标域）经上游
//! 4. [`upstream_down_plain_returns_502`]：上游不可达 → 明文路径不挂死、干净报错
//! 5. [`upstream_down_connect_tunnel_returns_502`]：上游不可达 → 隧道路径干净 502
//! 6. `live_*`（#[ignore]，需真实网络）：完整 MITM 链路模拟豆包客户端打真实站点，
//!    覆盖 CONNECT → 叶子证书 TLS → 解密转发 → 响应回传全链路

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::{ProxyConfig, ProxyServer};
use crate::device_proxy::upstream::UpstreamProxy;

// ---------------- 测试桩 ----------------

/// 静态后端：读到请求头后固定回 `200 OK + e2e-ok + Connection: close` 并关闭
async fn spawn_static_backend() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind backend");
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { break };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let mut got = 0;
                loop {
                    let n = match sock.read(&mut buf[got..]).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    got += n;
                    if buf[..got].windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                    if got == buf.len() {
                        return;
                    }
                }
                let body = b"e2e-ok";
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    port
}

/// 假上游 HTTP 代理：支持 CONNECT 隧道与绝对形式请求；目标主机名一律映射到本地
/// 后端端口（测试桩替代 DNS）。`seen` 计收到的连接数（断言「走了上游」的关键）；
/// `dead` 置真后接受连接立即丢弃（模拟上游宕机 → 代理侧读响应失败 → 触发回退）。
struct FakeUpstream {
    port: u16,
    seen: Arc<AtomicU16>,
    dead: Arc<AtomicBool>,
}

async fn spawn_fake_upstream(backend_port: u16) -> FakeUpstream {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind upstream");
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(AtomicU16::new(0));
    let dead = Arc::new(AtomicBool::new(false));
    let (s2, d2) = (seen.clone(), dead.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { break };
            if d2.load(Ordering::SeqCst) {
                // 模拟宕机：立即关闭，代理侧写 CONNECT 后读响应将得到 EOF/重置
                continue;
            }
            s2.fetch_add(1, Ordering::SeqCst);
            let bport = backend_port;
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let mut got = 0;
                loop {
                    let n = match sock.read(&mut buf[got..]).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    got += n;
                    if buf[..got].windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                    if got == buf.len() {
                        return;
                    }
                }
                let head = String::from_utf8_lossy(&buf[..got]);
                let is_connect = head
                    .lines()
                    .next()
                    .map(|l| l.starts_with("CONNECT"))
                    .unwrap_or(false);
                if is_connect {
                    let _ = sock.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await;
                }
                // 目标一律映射本地后端（桩替代 DNS），随后双向透传
                let Ok(mut backend) = TcpStream::connect(("127.0.0.1", bport)).await else {
                    return;
                };
                if !is_connect {
                    let _ = backend.write_all(&buf[..got]).await;
                }
                let _ = tokio::io::copy_bidirectional(&mut sock, &mut backend).await;
            });
        }
    });
    FakeUpstream { port, seen, dead }
}

/// 占用临时端口后立即释放（供代理监听用；轻微竞态可接受）
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("aiwork_e2e_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn start_proxy(tag: &str, targets: Vec<String>, upstream: Option<UpstreamProxy>) -> ProxyServer {
    let dir = temp_dir(tag);
    let cfg = ProxyConfig {
        port: free_port(),
        targets,
        auto_capture_jwt: false,
        data_dir: dir.clone(),
        certs_dir: dir.join("certs"),
        log_path: dir.join("proxy.log"),
        req_log_dir: dir.join("reqlogs"),
        upstream,
    };
    ProxyServer::start(cfg, None).await.expect("proxy start")
}

/// 读到完整 HTTP 头（\r\n\r\n）并返回头部字节，限时兜底防挂死
async fn read_head(io: &mut TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let n = match io.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
    })
    .await
    .expect("读响应头超时（代理链路挂死）");
    buf
}

/// 读完整响应头后短超时补读 body，直到出现 marker——小响应 body 可能与头同段
/// 到达，也可能落在后继 TCP 段（慢 runner 上更常见， read_head 只保头不保 body）
async fn read_head_and_body(io: &mut TcpStream, marker: &[u8]) -> Vec<u8> {
    let mut buf = read_head(io).await;
    if buf.windows(marker.len()).any(|w| w == marker) {
        return buf;
    }
    let _ = tokio::time::timeout(Duration::from_millis(2000), async {
        let mut chunk = [0u8; 1024];
        while let Ok(n) = io.read(&mut chunk).await {
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(marker.len()).any(|w| w == marker) {
                break;
            }
        }
    })
    .await;
    buf
}

// ---------------- 离线端到端用例 ----------------

/// 目标域明文 http 请求必须经上游转发（修复回归测试：旧代码此处直连、seen=0）
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn target_plain_http_routes_via_upstream() {
    let backend = spawn_static_backend().await;
    let up = spawn_fake_upstream(backend).await;
    let server = start_proxy(
        "tgt_plain",
        vec!["e2e.test".into()],
        Some(UpstreamProxy::Http(format!("127.0.0.1:{}", up.port))),
    )
    .await;

    let mut c = TcpStream::connect(("127.0.0.1", server.port)).await.unwrap();
    // 绝对形式请求打到目标域 api.e2e.test（真实 DNS 不存在——能拿到响应即证明走了
    // 上游桩的目标映射，而非直连解析）
    let req = format!("GET http://api.e2e.test:{backend}/ping HTTP/1.1\r\nHost: api.e2e.test\r\n\r\n");
    c.write_all(req.as_bytes()).await.unwrap();
    let resp = read_head_and_body(&mut c, b"e2e-ok").await;
    let resp = String::from_utf8_lossy(&resp);
    assert!(resp.contains("200"), "应返回 200，实际: {resp}");
    assert!(resp.contains("e2e-ok"), "应返回后端 body，实际: {resp}");
    assert_eq!(up.seen.load(Ordering::SeqCst), 1, "目标域请求必须经上游");
}

/// 非目标域明文 http 请求经上游转发（Python 原语义保持）
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nontarget_plain_http_routes_via_upstream() {
    let backend = spawn_static_backend().await;
    let up = spawn_fake_upstream(backend).await;
    let server = start_proxy(
        "nontgt_plain",
        vec!["e2e.test".into()],
        Some(UpstreamProxy::Http(format!("127.0.0.1:{}", up.port))),
    )
    .await;

    let mut c = TcpStream::connect(("127.0.0.1", server.port)).await.unwrap();
    let req = format!("GET http://other.example.com:{backend}/ping HTTP/1.1\r\nHost: other.example.com\r\n\r\n");
    c.write_all(req.as_bytes()).await.unwrap();
    let raw = read_head(&mut c).await;
    let head = String::from_utf8_lossy(&raw);
    assert!(head.contains("200"), "应返回 200，实际: {head}");
    assert_eq!(up.seen.load(Ordering::SeqCst), 1, "非目标域请求必须经上游");
}

/// CONNECT 透明隧道（非目标域 / 被降级为直通的目标域同路径）经上游建立并可透传
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_tunnel_routes_via_upstream() {
    let backend = spawn_static_backend().await;
    let up = spawn_fake_upstream(backend).await;
    let server = start_proxy(
        "tunnel_up",
        vec!["e2e.test".into()],
        Some(UpstreamProxy::Http(format!("127.0.0.1:{}", up.port))),
    )
    .await;

    // 注意用非目标域：*.e2e.test 会命中解密白名单走 MITM 路径，
    // 只有白名单外的 CONNECT 才进入透明隧道（tunnel_raw）
    let mut c = TcpStream::connect(("127.0.0.1", server.port)).await.unwrap();
    c.write_all(format!("CONNECT tunnel.example.com:{backend} HTTP/1.1\r\nHost: tunnel.example.com:{backend}\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let raw = read_head(&mut c).await;
    let head = String::from_utf8_lossy(&raw);
    assert!(head.contains("200"), "CONNECT 应答 200，实际: {head}");
    // 隧道内发一个真实 HTTP 请求，验证双向透传到后端
    // （body 可能与头异段到达——慢 runner 上 read_head 只拿到头，需补读，见 helper 注释）
    c.write_all(format!("GET / HTTP/1.1\r\nHost: tunnel.example.com:{backend}\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let resp = read_head_and_body(&mut c, b"e2e-ok").await;
    let resp = String::from_utf8_lossy(&resp);
    assert!(resp.contains("200") && resp.contains("e2e-ok"), "隧道内应拿到后端响应: {resp}");
    assert_eq!(up.seen.load(Ordering::SeqCst), 1, "CONNECT 隧道必须经上游");
}

/// 上游宕机：明文路径不挂死、快速返回 502（回退直连因 DNS 不可达也失败 → 502）
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_down_plain_returns_502() {
    let backend = spawn_static_backend().await;
    let up = spawn_fake_upstream(backend).await;
    let server = start_proxy(
        "up_down_plain",
        vec!["e2e.test".into()],
        Some(UpstreamProxy::Http(format!("127.0.0.1:{}", up.port))),
    )
    .await;
    up.dead.store(true, Ordering::SeqCst);

    let mut c = TcpStream::connect(("127.0.0.1", server.port)).await.unwrap();
    let req = format!("GET http://api.e2e.test:{backend}/ping HTTP/1.1\r\nHost: api.e2e.test\r\n\r\n");
    c.write_all(req.as_bytes()).await.unwrap();
    let raw = read_head(&mut c).await;
    let head = String::from_utf8_lossy(&raw);
    assert!(
        head.contains("502") || head.contains("504"),
        "上游宕机应快速 502/504，实际: {head}"
    );
}

/// 上游宕机：CONNECT 隧道路径回退直连（测试域 DNS 不可达）→ 干净 502 而非挂死
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_down_connect_tunnel_returns_502() {
    let backend = spawn_static_backend().await;
    let up = spawn_fake_upstream(backend).await;
    let server = start_proxy(
        "up_down_tunnel",
        vec!["e2e.test".into()],
        Some(UpstreamProxy::Http(format!("127.0.0.1:{}", up.port))),
    )
    .await;
    up.dead.store(true, Ordering::SeqCst);

    let mut c = TcpStream::connect(("127.0.0.1", server.port)).await.unwrap();
    // 直连回退目标用本地必拒端口（127.0.0.1:1）：不依赖 DNS 行为（Clash fake-ip
    // 可能对任意域名返回假 IP 并代为建连，导致「直连失败」前提失效）
    c.write_all(b"CONNECT 127.0.0.1:1 HTTP/1.1\r\nHost: 127.0.0.1:1\r\n\r\n")
        .await
        .unwrap();
    let raw = read_head(&mut c).await;
    let head = String::from_utf8_lossy(&raw);
    assert!(head.contains("502"), "上游宕机+直连不可达应 502，实际: {head}");
}

// ---------------- 实网端到端用例（#[ignore]：cargo test -- --ignored） ----------------

/// 完整 MITM 链路模拟豆包客户端（经上游 127.0.0.1:7890，与本机真实场景一致）：
/// CONNECT → 代理叶子证书 TLS（客户端信任代理 CA）→ 解密 → 上游转发 → 响应回传。
/// 运行前提：本机 7890 有可用 HTTP 代理（Clash 等）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "需真实网络与本机 7890 上游代理"]
async fn live_mitm_doubao_via_upstream() {
    live_mitm_case("live_up", Some(UpstreamProxy::Http("127.0.0.1:7890".into()))).await;
}

/// 同上但直连（无上游）。行为依环境而定（诊断用例）：Clash TUN 等接管路由的
/// 模式下「直连」流量会被虚拟网卡截获代转，测试仍可通过；纯系统代理（无 TUN）
/// 环境下直连目标域可能超时失败——该失败即「必须上游优先」的环境诊断证据
/// （proxy.log 22:21 实测 47 请求 0 响应的数据黑洞）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "需真实网络；直连可用性依环境（TUN 接管/系统代理）"]
async fn live_mitm_doubao_direct() {
    live_mitm_case("live_direct", None).await;
}

async fn live_mitm_case(tag: &str, upstream: Option<UpstreamProxy>) {
    use tokio_rustls::rustls::pki_types::ServerName;

    let dir = temp_dir(tag);
    let cfg = ProxyConfig {
        port: free_port(),
        targets: vec![], // 默认白名单含 doubao.com
        auto_capture_jwt: false,
        data_dir: dir.clone(),
        certs_dir: dir.join("certs"),
        log_path: dir.join("proxy.log"),
        req_log_dir: dir.join("reqlogs"),
        upstream,
    };
    let server = ProxyServer::start(cfg, None).await.expect("proxy start");

    // 客户端信任代理 CA（ensure_ca 已写 ca.cer DER）
    let ca_der = std::fs::read(dir.join("certs").join("ca.cer")).expect("ca.cer");
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    roots
        .add(tokio_rustls::rustls::pki_types::CertificateDer::from(ca_der))
        .expect("导入代理 CA");
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let tls_cfg = tokio_rustls::rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(tls_cfg));

    // 1) CONNECT www.doubao.com:443
    let mut tcp = TcpStream::connect(("127.0.0.1", server.port)).await.unwrap();
    tcp.write_all(b"CONNECT www.doubao.com:443 HTTP/1.1\r\nHost: www.doubao.com:443\r\n\r\n")
        .await
        .unwrap();
    let raw = read_head(&mut tcp).await;
    let head = String::from_utf8_lossy(&raw);
    assert!(head.contains("200"), "CONNECT 应答 200，实际: {head}");

    // 2) 与代理叶子证书完成 TLS（模拟豆包客户端校验）
    let name: ServerName<'static> = "www.doubao.com".try_into().expect("server name");
    let tls = tokio::time::timeout(Duration::from_secs(20), connector.connect(name, tcp))
        .await
        .expect("TLS 握手总超时")
        .expect("与代理叶子证书的 TLS 握手失败");
    let (mut r, mut w) = tokio::io::split(tls);

    // 3) 发真实请求并等待响应头回传
    w.write_all(b"GET / HTTP/1.1\r\nHost: www.doubao.com\r\nUser-Agent: e2e-doubao-sim\r\nAccept: */*\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let resp_head = tokio::time::timeout(Duration::from_secs(20), async {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = match r.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        buf
    })
    .await
    .expect("等待上游响应超时（链路黑洞）");
    let head = String::from_utf8_lossy(&resp_head);
    let status_ok = ["HTTP/1.1 200", "HTTP/1.1 301", "HTTP/1.1 302", "HTTP/1.1 307", "HTTP/1.1 308"]
        .iter()
        .any(|s| head.starts_with(s));
    assert!(status_ok, "应拿到真实 HTTP 响应，实际: {head}");
}
