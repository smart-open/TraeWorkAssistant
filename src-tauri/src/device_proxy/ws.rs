//! WebSocket 升级转发 + 双向帧记录（device_proxy.py `forward_websocket` /
//! `_WSMessageParser` 迁移，P4-6）。
//!
//! 流程（对齐 Python 版）：
//! 1. 直连上游并 TLS 握手（Python 版 WS 通道不走 VPN 上游）
//! 2. 转发升级请求（WS 专用跳过头集合：保留 Connection/Upgrade/Host，缺失兜底补齐 ——
//!    全量套用 HOP_BY_HOP 会把 "Connection: Upgrade" 删掉导致上游 400、客户端卡死）
//! 3. 透传上游 101 响应，升级成功后双向隧道 + 逐帧解析（去掩码/续帧重组/
//!    permessage-deflate 尽力解压）落日志
//!
//! 帧解析器自带 10MB 缓冲上限：畸形超长帧断开隧道，防止内存打爆。

use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::server::TlsStream;
use tokio_rustls::client::TlsStream as ClientTlsStream;

use crate::device_proxy::handler::RawRequest;
use crate::device_proxy::logger::ProxyLog;

/// WS 解析缓冲上限（对齐 Python _WS_BUF_MAX）
const WS_BUF_MAX: usize = 10 * 1024 * 1024;
/// 单次读取块大小（对齐 Python recv(65536)）
const READ_CHUNK: usize = 65536;
/// 上游 WS 升级握手超时（含 TLS 握手后的 101 响应等待）
const WS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// 上游握手响应头缓冲上限
const WS_RESP_MAX: usize = 16 * 1024;

/// WS 升级请求专用跳过头（对齐 Python _WS_SKIP）
const WS_SKIP: &[&str] = &["proxy-connection", "proxy-authorization", "content-length", "keep-alive"];

fn op_name(opcode: u8) -> String {
    match opcode {
        0x1 => "text".to_string(),
        0x2 => "binary".to_string(),
        0x8 => "close".to_string(),
        0x9 => "ping".to_string(),
        0xA => "pong".to_string(),
        _ => format!("0x{opcode:x}"),
    }
}

// ---------------- WS 帧解析器（对齐 Python _WSMessageParser） ----------------

/// 单帧解析结果（区分「数据不足」与「帧已消费但消息未完整」）
enum FrameParse {
    /// 缓冲数据不足，等待后续字节
    Incomplete,
    /// 帧已消费并累积进分帧缓存，消息尚未完整
    Fragment,
    /// 一条完整消息就绪（单帧 / 重组完成 / 控制帧）
    Message(u8, Vec<u8>),
}

/// 流式解析 WS 帧，按消息（含续帧重组）回调。客户端→服务端帧带掩码（自动去掩码），
/// 服务端→客户端不带。binary/text 消息尽力 permessage-deflate 解压（context takeover）。
struct WsMessageParser<F> {
    buf: Vec<u8>,
    frag_opcode: Option<u8>,
    frag_data: Vec<u8>,
    /// raw deflate 解码器（context takeover：跨消息复用状态）
    deco: Option<flate2::Decompress>,
    on_message: F,
}

impl<F> WsMessageParser<F>
where
    F: FnMut(u8, &[u8], Option<Vec<u8>>),
{
    fn new(on_message: F) -> Self {
        Self { buf: Vec::new(), frag_opcode: None, frag_data: Vec::new(), deco: None, on_message }
    }

    fn feed(&mut self, data: &[u8]) -> Result<(), String> {
        self.buf.extend_from_slice(data);
        if self.buf.len() > WS_BUF_MAX {
            return Err(format!("WS 解析缓冲超限 ({} > {WS_BUF_MAX})，断开连接", self.buf.len()));
        }
        while let Some((opcode, raw)) = self.try_parse()? {
            // 仅 text/binary 参与 permessage-deflate（审查修复：控制帧恒不压缩，
            // 把 ping/pong 载荷喂进解码器会污染 context takeover 状态，
            // 导致后续消息解压错乱）
            let decoded = if opcode == 0x1 || opcode == 0x2 {
                self.maybe_decompress(&raw)
            } else {
                None
            };
            (self.on_message)(opcode, &raw, decoded);
        }
        Ok(())
    }

    /// 尝试从缓冲取出一条完整消息；数据不足返回 Ok(None)。
    /// 分帧（帧已消费但消息未完整）不返回，继续吞后续帧直至消息完整或数据耗尽
    ///（对齐 Python：_buf 逐帧消化，消息回调只在完整消息时触发）。
    fn try_parse(&mut self) -> Result<Option<(u8, Vec<u8>)>, String> {
        loop {
            match self.try_parse_frame()? {
                FrameParse::Message(op, data) => return Ok(Some((op, data))),
                FrameParse::Fragment => continue,
                FrameParse::Incomplete => return Ok(None),
            }
        }
    }

    /// 尝试从缓冲解析一条完整消息；数据不足返回 None
    fn try_parse_frame(&mut self) -> Result<FrameParse, String> {
        if self.buf.len() < 2 {
            return Ok(FrameParse::Incomplete);
        }
        let b0 = self.buf[0];
        let b1 = self.buf[1];
        let fin = (b0 & 0x80) != 0;
        let opcode = b0 & 0x0f;
        let masked = (b1 & 0x80) != 0;
        let mut length = (b1 & 0x7f) as u64;
        let mut idx = 2usize;
        if length == 126 {
            if self.buf.len() < idx + 2 {
                return Ok(FrameParse::Incomplete);
            }
            length = u16::from_be_bytes([self.buf[idx], self.buf[idx + 1]]) as u64;
            idx += 2;
        } else if length == 127 {
            if self.buf.len() < idx + 8 {
                return Ok(FrameParse::Incomplete);
            }
            let mut b = [0u8; 8];
            b.copy_from_slice(&self.buf[idx..idx + 8]);
            length = u64::from_be_bytes(b);
            idx += 8;
        }
        // 声明长度超限：不等数据到齐直接拒绝（畸形帧防内存打爆，对齐 Python 校验）
        if length > WS_BUF_MAX as u64 {
            return Err(format!("WS 帧声明长度超限 ({length} > {WS_BUF_MAX})，断开连接"));
        }
        let mut mask = [0u8; 4];
        if masked {
            if self.buf.len() < idx + 4 {
                return Ok(FrameParse::Incomplete);
            }
            mask.copy_from_slice(&self.buf[idx..idx + 4]);
            idx += 4;
        }
        let length = length as usize;
        if self.buf.len() < idx + length {
            return Ok(FrameParse::Incomplete);
        }
        let mut payload = self.buf[idx..idx + length].to_vec();
        idx += length;
        self.buf.drain(..idx);
        if masked {
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= mask[i % 4];
            }
        }
        if opcode == 0x0 {
            // 续帧：拼入未完成消息（审查修复：累积总量同样受 WS_BUF_MAX 约束——
            // 此前仅单帧受限，恶意分片可借多帧无限累积打爆内存）
            if self.frag_data.len() + payload.len() > WS_BUF_MAX {
                return Err(format!(
                    "WS 分帧消息累积超限 ({} > {WS_BUF_MAX})，断开连接",
                    self.frag_data.len() + payload.len()
                ));
            }
            self.frag_data.extend_from_slice(&payload);
            if fin {
                let op = self.frag_opcode.take().unwrap_or(0);
                let data = std::mem::take(&mut self.frag_data);
                Ok(FrameParse::Message(op, data))
            } else {
                Ok(FrameParse::Fragment)
            }
        } else if opcode == 0x1 || opcode == 0x2 {
            // text / binary：FIN 完整消息，否则进入续帧累积
            if fin {
                Ok(FrameParse::Message(opcode, payload))
            } else {
                self.frag_opcode = Some(opcode);
                self.frag_data = payload;
                Ok(FrameParse::Fragment)
            }
        } else {
            // 控制帧 close/ping/pong 或未知
            Ok(FrameParse::Message(opcode, payload))
        }
    }

    /// 尽力 raw-deflate 解压（对齐 Python _maybe_decompress：失败返回 None，
    /// 追加 permessage-deflate 的空块尾 00 00 ff ff 冲出残留）
    fn maybe_decompress(&mut self, raw: &[u8]) -> Option<Vec<u8>> {
        if raw.is_empty() {
            return None;
        }
        let dec = self.deco.get_or_insert_with(|| flate2::Decompress::new(false));
        let mut out = Vec::with_capacity(raw.len() * 4);
        if dec.decompress_vec(raw, &mut out, flate2::FlushDecompress::None).is_err() {
            return None;
        }
        // 尾冲出失败容忍（对齐 Python try/except）
        let _ = dec.decompress_vec(&[0x00, 0x00, 0xff, 0xff], &mut out, flate2::FlushDecompress::Sync);
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }
}

// ---------------- 帧记录 ----------------

/// 构造帧记录回调：写操作日志（hexdump + 可打印字符串提取，对齐 Python _default_ws_frame_logger）。
/// 载荷可视化前先过 body 掩码（审查修复「帧脱敏」：WS 帧内同样会携带
/// token/authorization 等凭证键值，原样落操作日志违反脱敏红线）
fn frame_logger(
    direction: &'static str,
    host: String,
    path: String,
    log: ProxyLog,
) -> impl FnMut(u8, &[u8], Option<Vec<u8>>) {
    move |opcode, raw, decoded| {
        let opname = op_name(opcode);
        log.log(&format!(
            "  [WS FRAME] {direction} {host}{path} op={opname}({opcode}) raw={}B dec={}B",
            raw.len(),
            decoded.as_ref().map(|d| d.len()).unwrap_or(0)
        ));
        let data = decoded.as_deref().unwrap_or(raw);
        let masked = crate::device_proxy::logger::mask_body(&String::from_utf8_lossy(data));
        let masked_bytes = masked.as_bytes();
        log.log(&crate::device_proxy::logger::ws_hexdump(masked_bytes, 256));
        for s in crate::device_proxy::logger::ws_extract_strings(masked_bytes, 4, 20) {
            log.log(&format!("    | {s}"));
        }
    }
}

// ---------------- 双向隧道 ----------------

/// 单方向管道：src -> dst，逐块喂给帧解析器，EOF/错误时关闭 dst 写端（对齐 Python _pipe）
async fn ws_pipe<S, D, F>(
    mut src: S,
    mut dst: D,
    direction: &'static str,
    mut parser: WsMessageParser<F>,
    ws_tag: &str,
    log: &ProxyLog,
) -> u64
where
    S: AsyncRead + Unpin,
    D: AsyncWrite + Unpin,
    F: FnMut(u8, &[u8], Option<Vec<u8>>),
{
    let mut total = 0u64;
    let mut chunk = vec![0u8; READ_CHUNK];
    loop {
        match src.read(&mut chunk).await {
            Ok(0) => {
                log.log(&format!("  {ws_tag} {direction} 连接关闭 (已转发 {total} bytes)"));
                break;
            }
            Ok(n) => {
                total += n as u64;
                if let Err(e) = parser.feed(&chunk[..n]) {
                    log.log(&format!("  {ws_tag} {direction} 隧道异常: {e}"));
                    break;
                }
                if dst.write_all(&chunk[..n]).await.is_err() {
                    log.log(&format!("  {ws_tag} {direction} 写对端失败"));
                    break;
                }
            }
            Err(e) => {
                log.log(&format!("  {ws_tag} {direction} 隧道异常: {e}"));
                break;
            }
        }
    }
    let _ = dst.shutdown().await;
    total
}

/// 处理 WebSocket 升级：转发握手 → 透传 101 → 双向隧道 + 帧记录。
/// 借用客户端连接（&mut 借助 tokio 对 &mut T 的 AsyncRead/AsyncWrite 实现 split），
/// 返回后连接交还调用方由其决定关闭（serve_mitm 退出 keep-alive 循环）。
pub async fn forward_websocket<S: AsyncRead + AsyncWrite + Unpin>(
    client_io: &mut TlsStream<S>,
    ctx: &crate::device_proxy::handler::ProxyCtx,
    host: &str,
    port: u16,
    req: &RawRequest,
) {
    let ws_tag = format!("[WebSocket] {host}:{port}{}", req.path);
    // 路由对齐 MITM 转发（见 upstream.rs 模块注释）：上游优先、失败回退直连；
    // 白名单域（p3-2e，默认 qoder 各域）跳过上游直连目标
    let via_up = ctx.upstream.is_some();
    ctx.log.log(&format!(
        "  {ws_tag} 正在连接上游 {host}:{port}（{}）...",
        if via_up { "经上游代理" } else { "直连" }
    ));
    let mut upstream: ClientTlsStream<TcpStream> =
        match crate::device_proxy::upstream::connect_tls_upstream_first(
            host,
            port,
            ctx.upstream.as_ref(),
            &ctx.direct_domains,
            &ctx.log,
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                ctx.log.log(&format!("  {ws_tag} 连接上游失败: {e}"));
                return;
            }
        };
    ctx.log.log(&format!("  {ws_tag} 上游 TLS 连接已建立"));

    // 构建并发送升级请求（WS 专用跳过头集合 + 缺失兜底补齐）
    let mut req_lines = format!("{} {} HTTP/1.1\r\n", req.method, req.path);
    let mut seen_upgrade = false;
    let mut seen_connection = false;
    for (k, v) in &req.headers {
        let kl = k.to_ascii_lowercase();
        if WS_SKIP.iter().any(|s| *s == kl) {
            continue;
        }
        if kl == "upgrade" {
            seen_upgrade = true;
        }
        if kl == "connection" {
            seen_connection = true;
        }
        req_lines.push_str(&format!("{k}: {v}\r\n"));
    }
    if !seen_upgrade {
        req_lines.push_str("Upgrade: websocket\r\n");
    }
    if !seen_connection {
        req_lines.push_str("Connection: Upgrade\r\n");
    }
    ctx.log.log(&format!(
        "  {ws_tag} 关键头: Upgrade={} Connection={} Sec-WebSocket-Key={}...",
        req.hget("upgrade").unwrap_or("?"),
        req.hget("connection").unwrap_or("?"),
        req.hget("sec-websocket-key").unwrap_or("").chars().take(16).collect::<String>(),
    ));
    let mut req_data = req_lines.into_bytes();
    req_data.extend_from_slice(b"\r\n");
    req_data.extend_from_slice(&req.body);
    ctx.log
        .log(&format!("  {ws_tag} 发送升级请求: {} {} ({} bytes)", req.method, req.path, req_data.len()));

    if let Err(e) = upstream.write_all(&req_data).await {
        ctx.log.log(&format!("  {ws_tag} 发送升级请求失败: {e}"));
        return;
    }

    // 读取上游响应（应为 101 Switching Protocols）。
    // 握手阶段整体限时 + 缓冲上限（审查修复：原无超时无上限，上游僵死时
    // 任务永久挂起占住连接与并发槽，恶意响应头可无限撑大内存）
    ctx.log.log(&format!("  {ws_tag} 等待上游响应..."));
    let mut resp_buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    let handshake = timeout(WS_HANDSHAKE_TIMEOUT, async {
        loop {
            if resp_buf.windows(4).any(|w| w == b"\r\n\r\n") {
                return Ok(());
            }
            match upstream.read(&mut chunk).await {
                // EOF：交由上层判空/判 101
                Ok(0) => return Ok(()),
                Ok(n) => {
                    resp_buf.extend_from_slice(&chunk[..n]);
                    if resp_buf.len() > WS_RESP_MAX {
                        return Err(format!("上游响应头超过 {WS_RESP_MAX} 字节"));
                    }
                }
                Err(e) => return Err(e.to_string()),
            }
        }
    })
    .await;
    match handshake {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            ctx.log.log(&format!("  {ws_tag} 读取上游响应失败: {e}"));
            return;
        }
        Err(_) => {
            ctx.log.log(&format!(
                "  {ws_tag} 等待上游响应超时 ({}s)，放弃",
                WS_HANDSHAKE_TIMEOUT.as_secs()
            ));
            return;
        }
    }
    if resp_buf.is_empty() {
        ctx.log.log(&format!("  {ws_tag} 上游响应为空，放弃"));
        return;
    }

    // 透传响应给客户端
    let first_line = String::from_utf8_lossy(resp_buf.split(|&b| b == b'\n').next().unwrap_or(b""))
        .trim_end()
        .to_string();
    ctx.log
        .log(&format!("  {ws_tag} 收到上游响应: {first_line} ({} bytes)", resp_buf.len()));
    if let Err(e) = client_io.write_all(&resp_buf).await {
        ctx.log.log(&format!("  {ws_tag} 响应转发客户端失败: {e}"));
        return;
    }
    let _ = client_io.flush().await;
    ctx.log.log(&format!("  {ws_tag} 响应已转发给客户端"));

    if !first_line.contains(" 101 ") {
        ctx.log.log(&format!("  {ws_tag} 升级失败: {first_line}"));
        return;
    }
    ctx.log.log(&format!("  {ws_tag} 升级成功 (101 Switching Protocols), 开始双向隧道"));
    ctx.req_logger.log_websocket(host, &req.path, &req.headers);

    // 双向隧道：每方向独立帧解析器（join! 同作用域运行，无 'static 约束）
    let (c_r, c_w) = tokio::io::split(client_io);
    let (u_r, u_w) = tokio::io::split(upstream);
    let p1 = WsMessageParser::new(frame_logger("client_to_up", host.to_string(), req.path.clone(), ctx.log.clone()));
    let p2 = WsMessageParser::new(frame_logger("up_to_client", host.to_string(), req.path.clone(), ctx.log.clone()));
    let (up_bytes, down_bytes) = tokio::join!(
        ws_pipe(c_r, u_w, "client_to_up", p1, &ws_tag, &ctx.log),
        ws_pipe(u_r, c_w, "up_to_client", p2, &ws_tag, &ctx.log),
    );
    ctx.log.log(&format!(
        "  {ws_tag} 双向隧道结束: client->up={up_bytes} bytes, up->client={down_bytes} bytes"
    ));
    ctx.log.log(&format!("  {ws_tag} 上游连接已关闭"));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 累积消息的测试容器
    fn parse_all(data: &[u8]) -> Vec<(u8, Vec<u8>)> {
        let mut msgs = Vec::new();
        let mut p = WsMessageParser::new(|op, raw, _dec| msgs.push((op, raw.to_vec())));
        p.feed(data).expect("feed ok");
        msgs
    }

    /// 构造一帧（服务端→客户端：不带掩码）
    fn frame(fin: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![(if fin { 0x80 } else { 0 }) | opcode];
        if payload.len() < 126 {
            out.push(payload.len() as u8);
        } else {
            out.push(126);
            out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        out.extend_from_slice(payload);
        out
    }

    /// 构造一帧（客户端→服务端：带掩码，对 payload 掩码前数据）
    fn masked_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mask = [0xAA, 0xBB, 0xCC, 0xDD];
        let mut out = vec![0x80 | opcode, 0x80 | payload.len() as u8];
        out.extend_from_slice(&mask);
        out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        out
    }

    #[test]
    fn parses_single_text_frame() {
        let msgs = parse_all(&frame(true, 0x1, b"hello"));
        assert_eq!(msgs, vec![(0x1, b"hello".to_vec())]);
    }

    #[test]
    fn unmasks_client_frames() {
        let msgs = parse_all(&masked_frame(0x2, &[1, 2, 3, 4, 5]));
        assert_eq!(msgs, vec![(0x2, vec![1, 2, 3, 4, 5])]);
    }

    #[test]
    fn reassembles_fragmented_messages() {
        let mut data = frame(false, 0x1, b"foo");
        data.extend(frame(false, 0x0, b"bar"));
        data.extend(frame(true, 0x0, b"baz"));
        let msgs = parse_all(&data);
        assert_eq!(msgs, vec![(0x1, b"foobarbaz".to_vec())]);
    }

    #[test]
    fn control_frames_pass_through_between_fragments() {
        let mut data = frame(false, 0x1, b"ab");
        data.extend(frame(true, 0x9, b"ping")); // ping 穿插在分帧之间
        data.extend(frame(true, 0x0, b"cd"));
        let msgs = parse_all(&data);
        assert_eq!(
            msgs,
            vec![(0x9, b"ping".to_vec()), (0x1, b"abcd".to_vec())]
        );
    }

    #[test]
    fn partial_frames_are_buffered() {
        let full = frame(true, 0x1, b"hello");
        let (a, b) = full.split_at(3);
        let msgs = std::cell::RefCell::new(Vec::new());
        let mut p =
            WsMessageParser::new(|op, raw, _dec| msgs.borrow_mut().push((op, raw.to_vec())));
        p.feed(a).expect("feed1");
        assert!(msgs.borrow().is_empty());
        p.feed(b).expect("feed2");
        assert_eq!(*msgs.borrow(), vec![(0x1, b"hello".to_vec())]);
    }

    #[test]
    fn oversized_buffer_rejected() {
        let mut p = WsMessageParser::new(|_, _, _| {});
        // 声明 127 位超长帧头但不发数据
        let mut head = vec![0x82, 0x7f];
        head.extend_from_slice(&(WS_BUF_MAX as u64 + 1).to_be_bytes());
        assert!(p.feed(&head).is_err());
    }

    /// 构造带 64 位长度头的帧（payload > 65535 时必须用 127 编码，测试辅助 frame() 不支持）
    fn big_frame(fin: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![(if fin { 0x80 } else { 0 }) | opcode, 127];
        out.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    /// 审查修复回归（分片累积上限）：各分片单帧均合法（< WS_BUF_MAX），
    /// 但逐片累积超限时必须拒绝（分片炸弹）
    #[test]
    fn fragmented_accumulation_capped() {
        let mut p = WsMessageParser::new(|_, _, _| {});
        // 首片：帧总字节数（含 10 字节帧头）恰不超单次 feed 缓冲上限
        assert!(p.feed(&big_frame(false, 0x1, &vec![0u8; WS_BUF_MAX - 11])).is_ok());
        assert!(p.feed(&big_frame(false, 0x0, b"x")).is_ok());
        assert!(
            p.feed(&big_frame(true, 0x0, &[0u8; 16])).is_err(),
            "分片累积超 WS_BUF_MAX 应拒绝"
        );
    }

    /// 审查修复回归（帧脱敏）：WS 帧载荷中的凭证键值落操作日志前必须掩码
    #[test]
    fn frame_logger_masks_credentials() {
        let dir = std::env::temp_dir().join(format!("aiwork_ws_mask_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let log = ProxyLog::new(
            dir.join("proxy.log"),
            None,
            std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0)),
        );
        let mut f = frame_logger("client_to_up", "h.com".into(), "/ws".into(), log);
        f(0x1, br#"{"token":"secret-jwt-value"}"#, None);
        // 日志经专用落盘线程异步写盘（非阻塞投递）：轮询等待掩码行落盘后再断言。
        // 预算须留足：全量并发跑测试时 drainer 线程调度可被推迟到秒级（实测 1s
        // 预算在 400+ 并发用例下稳定超时）；且必须等到本行的掩码出现而非任意
        // 非空内容，否则空文件提前退出会跳过脱敏断言形成假阴性
        let path = dir.join("proxy.log");
        let content = {
            let mut s = String::new();
            for _ in 0..500 {
                if let Ok(read) = std::fs::read_to_string(&path) {
                    s = read;
                    if s.contains("***") {
                        break;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            s
        };
        assert!(!content.contains("secret-jwt-value"), "WS 帧凭证必须脱敏:\n{content}");
        assert!(content.contains("***"), "掩码行应在轮询预算内落盘:\n{content}");
    }

    #[test]
    fn op_names() {
        assert_eq!(op_name(0x1), "text");
        assert_eq!(op_name(0x8), "close");
        assert_eq!(op_name(0xF), "0xf");
    }
}
