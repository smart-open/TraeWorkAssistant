#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
通用测试代理 test_proxy — 参考 device_proxy.py 实现
==================================================
本地 MITM 代理：透明截获被监控域名(默认复用 device_proxy 的 TRAE 相关域)的全量
HTTPS 流量，TLS 解密后完整记录每一笔请求的：

  - 请求方法 / Host / Path / 接口分类(大模型对话 / 签到领取 ...)
  - 全部请求头(详细)
  - 请求体(可在 --save-bodies 时把完整体另存为文件)
  - 响应状态码 / 全部响应头
  - 响应体(自动解压 gzip / deflate / br 后记录)
  - SSE 流式响应的摘要(模型名 / token 用量 / 输出块数)

每笔请求分配单调递增序号 SEQ，按"到达顺序"写入 test_proxy.log，保证后续分析可
还原请求顺序；同一 SEQ 的请求块与响应块成对出现，并发也不会交错。

本脚本仅做"观测记录"，不修改任何请求头、不捕获 JWT、不注入设备标识，与
device_proxy.py 的签到改写逻辑完全解耦，专用于流量抓包分析。

运行依赖：复用 device_proxy.py 的 TLS 拦截 / 解压 / SSE 摘要实现（故需同目录存在
device_proxy.py，且其依赖 cryptography 可用）。

用法：
  python test_proxy.py                 # 监听 127.0.0.1:8899，监控默认 TRAE 域名
  python test_proxy.py --gen-ca       # 仅生成 CA 证书后退出(供安装 CA 用)
  python test_proxy.py --install-ca   # 以管理员身份将 CA 装入 Windows 受信任根证书颁发机构(零配置关键)
  python test_proxy.py --port 8888    # 指定监听端口
  python test_proxy.py --domains "a.com,b.com"   # 覆盖监控域名
  python test_proxy.py --log-all      # 对所有域做 MITM 解密并记录(不再透明放行)
  python test_proxy.py --observe      # 观测模式(默认): 对监控域名 TLS 解密并记录全量信息，非监控域透明放行
  python test_proxy.py --save-bodies  # 额外把完整请求/响应体存到 proxy_bodies/
  python test_proxy.py --max-body 500000          # 日志中单条 body 预览上限(字节)

证书与"零配置"：优先复用桌面端 Trae Work Assistant 已在
%APPDATA%\\TraeWorkAssistant\\data\\certs 生成、且多半已安装信任的自签 CA，因此通常
无需任何额外配置即可对监控域名做 TLS 解密。若本机尚未信任该 CA，以管理员身份执行一次
`python test_proxy.py --install-ca` 即可完成信任(或手动安装同目录 ca.cer)。
"""
import os
import sys
import json
import ssl
import socket
import threading
import datetime
import time
import gzip
import zlib
import http.client
from urllib.parse import urlparse

# 本测试工具默认"自包含"：把 device_proxy 的 artifacts(CA / 其自身 proxy.log) 落到本
# 脚本所在目录。但为了让用户"零配置"即可解密(不必再装一次证书)，优先复用桌面端
# Trae Work Assistant 已生成、且多半已安装信任的 CA(位于 %APPDATA%\TraeWorkAssistant\data\certs)。
BASE = os.path.dirname(os.path.abspath(__file__))
os.environ.setdefault("TRAEDATA_DIR", BASE)

import ctypes
import subprocess

def _is_admin():
    try:
        return ctypes.windll.shell32.IsUserAnAdmin()
    except Exception:
        return False

# 优先复用桌面端已信任的 CA；不存在时退回本脚本 data/certs 自生成
APP_CERT_DIR = os.path.join(os.environ.get("APPDATA", ""), "TraeWorkAssistant", "data", "certs")

def resolve_ca_dir():
    if os.path.isfile(os.path.join(APP_CERT_DIR, "ca.crt")):
        return APP_CERT_DIR
    return os.path.join(BASE, "data", "certs")

import device_proxy as dp  # 复用 TLS 拦截(ensure_ca/leaf_cert)、解压、SSE 摘要、域名匹配
dp.CA_DIR = resolve_ca_dir()   # 关键：让 ensure_ca 复用(或生成到)正确的证书目录

def install_ca_to_root():
    """把代理 CA 安装到 Windows 受信任根证书颁发机构，实现真正的零配置解密。
    需要管理员权限；非管理员时返回 (False, 提示)。"""
    cer = os.path.join(dp.CA_DIR, "ca.cer")
    if not os.path.isfile(cer):
        return False, "CA 证书尚未生成，请先正常运行一次(会自动生成)。"
    if not _is_admin():
        return False, ("需要管理员权限：请右键『以管理员身份运行』后执行 "
                       f'certutil -addstore Root "{cer}"')
    try:
        r = subprocess.run(["certutil", "-addstore", "Root", cer],
                           capture_output=True, text=True, check=True)
        return True, r.stdout.strip() or "已安装到受信任根证书颁发机构。"
    except subprocess.CalledProcessError as e:
        return False, (e.stderr or e.stdout or str(e))
    except Exception as e:
        return False, str(e)

LISTEN_HOST = "127.0.0.1"
LISTEN_PORT = int(os.environ.get("PROXY_PORT", "8899"))

# 监控域名：默认复用 device_proxy 的 TARGET_DOMAINS，可用 --domains 覆盖
MONITOR_DOMAINS = list(dp.TARGET_DOMAINS)
LOG_ALL = False

LOG_FILE = os.path.join(BASE, "test_proxy.log")
BODIES_DIR = os.path.join(BASE, "proxy_bodies")

_seq_lock = threading.Lock()
_seq = 0


def next_seq():
    global _seq
    with _seq_lock:
        _seq += 1
        return _seq


def host_in_monitor(host):
    h = (host or "").lower()
    return any(h == d or h.endswith("." + d) for d in MONITOR_DOMAINS)


# --------------------------------------------------------------------------
# 日志记录器：按 SEQ 顺序写入 test_proxy.log，单条 block 内加锁保证不交错
# --------------------------------------------------------------------------
class TestProxyLogger:
    def __init__(self, path, max_body=200000, save_bodies=False):
        self.path = path
        self.max_body = max_body
        self.save_bodies = save_bodies
        self._lock = threading.Lock()
        self._fd = open(path, "a", encoding="utf-8", buffering=1)
        self.count = 0
        self.domains = {}

    def _preview(self, body):
        if body is None:
            return ""
        if isinstance(body, bytes):
            try:
                txt = body.decode("utf-8")
            except Exception:
                return f"<二进制 {len(body)} bytes，无法解码>"
        else:
            txt = str(body)
        if len(txt) > self.max_body:
            return txt[:self.max_body] + f"\n... [截断，完整 {len(txt)} 字符，见 --save-bodies]"
        return txt

    def _save_body(self, seq, kind, body):
        if not self.save_bodies or not body:
            return
        os.makedirs(BODIES_DIR, exist_ok=True)
        ext = ".bin"
        if isinstance(body, bytes):
            try:
                body.decode("utf-8")
                ext = ".txt"
            except Exception:
                ext = ".bin"
            data = body
        else:
            data = str(body).encode("utf-8")
            ext = ".txt"
        p = os.path.join(BODIES_DIR, f"{seq:04d}_{kind}{ext}")
        try:
            with open(p, "wb") as f:
                f.write(data)
        except Exception:
            pass

    def log_event(self, msg):
        ts = datetime.datetime.now().strftime("%Y-%m-%d %H:%M:%S")
        with self._lock:
            self._fd.write(f"[{ts}] {msg}\n")

    def log_session_start(self, port, domains, log_all, save_bodies, observe=False):
        ts = datetime.datetime.now().strftime("%Y-%m-%d %H:%M:%S")
        record_scope = ('全部域名(MITM)' if log_all else '仅监控域名(解密记录全量)')
        with self._lock:
            self._fd.write("\n" + "=" * 80 + "\n")
            self._fd.write(f" test_proxy 启动  {ts}\n")
            self._fd.write(f"   监听    : {LISTEN_HOST}:{port}\n")
            self._fd.write(f"   监控域  : {', '.join(domains)}\n")
            self._fd.write(f"   模式    : {'观测(监控域名解密记录, 非监控透明放行)' if observe else 'MITM 全量抓包'}\n")
            self._fd.write(f"   记录范围: {record_scope}\n")
            self._fd.write(f"   完整体  : {'另存 proxy_bodies/' if save_bodies else '仅日志预览'}\n")
            self._fd.write("=" * 80 + "\n")

    def log_request(self, seq, t_recv, method, host, path, ep, headers, body):
        with self._lock:
            self.count += 1
            self.domains[host] = self.domains.get(host, 0) + 1
            lines = [
                "\n" + "#" * 80,
                f"[SEQ {seq:04d}] REQUEST  {t_recv.strftime('%Y-%m-%d %H:%M:%S.%f')[:-3]}",
                f"  Method : {method}",
                f"  Host   : {host}",
                f"  Path   : {path}",
                f"  Class  : {ep or '(未知接口)'}",
                f"  --- Request Headers ({len(headers)} 项) ---",
            ]
            for k, v in headers.items():
                lines.append(f"    {k}: {v}")
            blen = len(body) if body else 0
            lines.append(f"  --- Request Body ({blen} bytes) ---")
            lines.append(self._preview(body))
            self._fd.write("\n".join(lines) + "\n")
        self._save_body(seq, "req", body)

    def log_response(self, seq, t_resp, duration_ms, status, reason, resp_headers,
                     body, sse_summary, raw_len):
        with self._lock:
            lines = [
                "\n" + "#" * 80,
                f"[SEQ {seq:04d}] RESPONSE {t_resp.strftime('%Y-%m-%d %H:%M:%S.%f')[:-3]}  "
                f"(耗时 {duration_ms} ms, 原始 {raw_len} bytes)",
                f"  Status : {status} {reason}",
                f"  --- Response Headers ({len(resp_headers)} 项) ---",
            ]
            # resp_headers 可能是 list of tuples 或 dict
            if isinstance(resp_headers, dict):
                items = list(resp_headers.items())
            else:
                items = list(resp_headers)
            for k, v in items:
                lines.append(f"    {k}: {v}")
            blen = len(body) if body else 0
            lines.append(f"  --- Response Body ({blen} bytes) ---")
            lines.append(self._preview(body))
            if sse_summary:
                lines.append("  --- SSE Summary ---")
                for k, v in sse_summary.items():
                    lines.append(f"    {k}: {v}")
            self._fd.write("\n".join(lines) + "\n")
        self._save_body(seq, "resp", body)

    def log_session_end(self):
        with self._lock:
            self._fd.write("\n" + "=" * 80 + "\n")
            self._fd.write(f" test_proxy 停止  共记录请求: {self.count} 条\n")
            if self.domains:
                dist = "  ".join(f"{h}={c}" for h, c in sorted(
                    self.domains.items(), key=lambda x: -x[1]))
                self._fd.write(f" 域名分布: {dist}\n")
            self._fd.write("=" * 80 + "\n")


logger = None


# --------------------------------------------------------------------------
# 转发 + 记录
# --------------------------------------------------------------------------
def forward_normal_logged(host, port, method, path, headers, body, client_sock, seq, t0):
    """非流式请求：完整读取响应后回传客户端并记录。"""
    ctx = ssl.create_default_context()
    conn = http.client.HTTPSConnection(host, port, context=ctx, timeout=30)
    fwd = {k: v for k, v in headers.items() if k.lower() not in dp.HOP_BY_HOP}
    try:
        conn.request(method, path, body=body if method.upper() != "GET" else None, headers=fwd)
        resp = conn.getresponse()
        resp_headers_list = resp.getheaders()
        resp_body = resp.read()
        dp.send_response(client_sock, resp.status, resp.reason, dict(resp_headers_list), resp_body)
        duration = int((time.time() - t0) * 1000)
        decompressed = dp.decompress_body(resp_body, resp_headers_list)
        sse = dp.extract_sse_summary(resp_headers_list, resp_body)
        logger.log_response(seq, datetime.datetime.now(), duration, resp.status,
                           resp.reason, resp_headers_list, decompressed, sse, len(resp_body))
        return True
    except Exception as e:
        duration = int((time.time() - t0) * 1000)
        logger.log_response(seq, datetime.datetime.now(), duration, 0,
                            f"{type(e).__name__}: {e}", [], b"", None, 0)
        try:
            dp.send_response(client_sock, 502, "Bad Gateway", {}, b"Bad Gateway")
        except Exception:
            pass
        return False
    finally:
        conn.close()


def forward_stream_logged(host, port, method, path, headers, body, client_sock, seq, t0):
    """流式(SSE)请求：逐块转发给客户端，同时累积用于记录与 SSE 摘要。"""
    ctx = ssl.create_default_context()
    conn = http.client.HTTPSConnection(host, port, context=ctx, timeout=300)
    fwd = {k: v for k, v in headers.items() if k.lower() not in dp.HOP_BY_HOP}
    try:
        conn.request(method, path, body=body if method.upper() != "GET" else None, headers=fwd)
        resp = conn.getresponse()
        resp_headers_list = resp.getheaders()
        head = [f"HTTP/1.1 {resp.status} {resp.reason}".encode("utf-8")]
        for k, v in resp_headers_list:
            if k.lower() in ("transfer-encoding", "connection", "keep-alive", "content-length"):
                continue
            head.append(f"{k}: {v}".encode("utf-8"))
        head.append(b"Transfer-Encoding: chunked")
        head.append(b"Connection: keep-alive")
        client_sock.sendall(b"\r\n".join(head) + b"\r\n\r\n")

        total = 0
        logged = bytearray()
        max_log = logger.max_body
        while True:
            chunk = resp.read(8192)
            if not chunk:
                break
            client_sock.sendall(f"{len(chunk):x}\r\n".encode("ascii") + chunk + b"\r\n")
            total += len(chunk)
            if len(logged) < max_log:
                logged.extend(chunk)
        client_sock.sendall(b"0\r\n\r\n")

        duration = int((time.time() - t0) * 1000)
        sse = dp.extract_sse_summary(resp_headers_list, bytes(logged))
        logger.log_response(seq, datetime.datetime.now(), duration, resp.status,
                           resp.reason, resp_headers_list, bytes(logged), sse, total)
        return True
    except Exception as e:
        duration = int((time.time() - t0) * 1000)
        logger.log_response(seq, datetime.datetime.now(), duration, 0,
                            f"{type(e).__name__}: {e}", [], b"", None, 0)
        try:
            client_sock.sendall(b"0\r\n\r\n")
        except Exception:
            pass
        return False
    finally:
        conn.close()


def tunnel_https_logged(tls, host, port):
    while True:
        try:
            req = dp.read_http_request(tls)
        except Exception as e:
            logger.log_event(f"[MITM] 读取 TLS 请求错误 {host}: {type(e).__name__}: {e}")
            break
        if req is None:
            break
        method, path, version, headers, body = req
        seq = next_seq()
        t0 = time.time()
        ep = dp.classify_path(path)
        logger.log_request(seq, datetime.datetime.now(), method, host, path, ep, headers, body)

        if dp.is_websocket_upgrade(headers):
            logger.log_event(f"[WS SEQ {seq:04d}] 升级 {host}{path}")
            dp.forward_websocket(host, port, method, path, headers, body, tls)
            logger.log_response(seq, datetime.datetime.now(), int((time.time() - t0) * 1000),
                                "101", "Switching Protocols", [], b"", None, 0)
            break

        is_stream = ("llm_utils_chat" in path) or ("event-stream" in headers.get("accept", ""))
        if is_stream:
            forward_stream_logged(host, port, method, path, headers, body, tls, seq, t0)
        else:
            forward_normal_logged(host, port, method, path, headers, body, tls, seq, t0)


def handle_plain_logged(conn, buf):
    header_blob, _, rest = buf.partition(b"\r\n\r\n")
    lines = header_blob.split(b"\r\n")
    first = lines[0].decode("utf-8", "replace")
    parts = first.split(" ")
    method = parts[0]
    target = parts[1]
    u = urlparse(target)
    host = u.hostname
    port = u.port or (443 if u.scheme == "https" else 80)
    monitored = host_in_monitor(host) or LOG_ALL
    headers = {}
    for line in lines[1:]:
        if b":" in line:
            k, _, v = line.partition(b":")
            headers[k.decode("utf-8", "replace").strip().lower()] = v.decode("utf-8", "replace").strip()
    cl = int(headers.get("content-length", 0) or 0)
    body = rest
    while len(body) < cl:
        chunk = conn.recv(4096)
        if not chunk:
            break
        body += chunk
    body = body[:cl] if cl else body

    if monitored:
        seq = next_seq()
        t0 = time.time()
        ep = dp.classify_path(u.path or "/")
        logger.log_request(seq, datetime.datetime.now(), method, host, u.path or "/", ep, headers, body)
    else:
        seq = t0 = None

    ctx = ssl.create_default_context() if u.scheme == "https" else None
    c = (http.client.HTTPSConnection(host, port, context=ctx, timeout=30)
         if u.scheme == "https" else http.client.HTTPConnection(host, port, timeout=30))
    fwd = {k: v for k, v in headers.items() if k.lower() not in dp.HOP_BY_HOP}
    try:
        c.request(method, u.path or "/", body=body if method.upper() != "GET" else None, headers=fwd)
        resp = c.getresponse()
        resp_headers_list = resp.getheaders()
        resp_body = resp.read()
        dp.send_response(conn, resp.status, resp.reason, dict(resp_headers_list), resp_body)
        if monitored and seq is not None:
            duration = int((time.time() - t0) * 1000)
            decompressed = dp.decompress_body(resp_body, resp_headers_list)
            sse = dp.extract_sse_summary(resp_headers_list, resp_body)
            logger.log_response(seq, datetime.datetime.now(), duration, resp.status,
                               resp.reason, resp_headers_list, decompressed, sse, len(resp_body))
    except Exception as e:
        if monitored and seq is not None:
            duration = int((time.time() - t0) * 1000)
            logger.log_response(seq, datetime.datetime.now(), duration, 0,
                                f"{type(e).__name__}: {e}", [], b"", None, 0)
        try:
            dp.send_response(conn, 502, "Bad Gateway", {}, b"Bad Gateway")
        except Exception:
            pass
    finally:
        c.close()


def handle_client(conn, addr):
    try:
        buf = conn.recv(4096)
        if not buf:
            return
        while b"\r\n\r\n" not in buf:
            more = conn.recv(4096)
            if not more:
                break
            buf += more
        head = buf.split(b"\r\n", 1)[0].decode("utf-8", "replace")
        method = head.split(" ")[0]
        if method == "CONNECT":
            target = head.split(" ")[1]
            if ":" in target:
                host, port = target.rsplit(":", 1)
                port = int(port)
            else:
                host, port = target, 443
            monitored = host_in_monitor(host) or LOG_ALL
            if not monitored:
                # 非监控域名：透明隧道放行，不解密、不记录(避免污染日志)
                dp.tunnel_raw(conn, host, port)
                return
            matched = next((d for d in MONITOR_DOMAINS if host == d or host.endswith("." + d)), None)
            logger.log_event(f"CONNECT {host}:{port}  "
                             + (f"[MONITOR 命中: {matched}]" if matched else "[MONITOR: 全部]"))
            conn.sendall(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            cpath, kpath = dp.leaf_cert(host)
            ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            ctx.load_cert_chain(certfile=cpath, keyfile=kpath)
            tls = ctx.wrap_socket(conn, server_side=True)
            tunnel_https_logged(tls, host, port)
        else:
            handle_plain_logged(conn, buf)
    except Exception as e:
        logger.log_event(f"client err: {type(e).__name__}: {e}")
    finally:
        try:
            conn.close()
        except Exception:
            pass


def main():
    global MONITOR_DOMAINS, LOG_ALL, logger
    import argparse
    parser = argparse.ArgumentParser(description="通用测试代理 test_proxy — 全量抓包记录")
    parser.add_argument("--port", type=int, default=LISTEN_PORT, help="监听端口(默认 8899)")
    parser.add_argument("--domains", type=str, default="", help="覆盖监控域名，逗号分隔")
    parser.add_argument("--observe", action="store_true",
                        help="观测模式(默认即此): 对监控域名 TLS 解密并记录全量信息，非监控域透明放行")
    parser.add_argument("--log-all", action="store_true", help="对所有域做 MITM 解密并记录")
    parser.add_argument("--save-bodies", action="store_true", help="把完整请求/响应体另存 proxy_bodies/")
    parser.add_argument("--max-body", type=int, default=200000, help="日志内单条 body 预览上限(字节)")
    parser.add_argument("--gen-ca", action="store_true", help="仅生成 CA 证书后退出")
    parser.add_argument("--install-ca", action="store_true", help="将 CA 装入 Windows 受信任根证书颁发机构(需管理员)")
    args = parser.parse_args()

    if args.install_ca:
        dp.ensure_ca()
        ok, msg = install_ca_to_root()
        print(msg)
        return 0 if ok else 1

    if args.gen_ca:
        dp.ensure_ca()
        print("CA 证书已生成:", os.path.join(dp.CA_DIR, "ca.cer"))
        print("若尚未信任，请以管理员运行: python test_proxy.py --install-ca")
        return 0

    if args.domains:
        MONITOR_DOMAINS[:] = [d.strip() for d in args.domains.split(",") if d.strip()]
    LOG_ALL = args.log_all
    OBSERVE = True  # 观测模式为默认行为：监控域名解密记录全量，非监控透明放行

    dp.ensure_ca()
    logger = TestProxyLogger(LOG_FILE, max_body=args.max_body, save_bodies=args.save_bodies)
    logger.log_session_start(args.port, MONITOR_DOMAINS, LOG_ALL, args.save_bodies, observe=OBSERVE)

    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind((LISTEN_HOST, args.port))
    srv.listen(128)
    logger.log_event(f"代理已启动: {LISTEN_HOST}:{args.port}  (通用测试抓包代理 / 观测模式)")
    logger.log_event(f"监控域: {', '.join('*.' + d for d in MONITOR_DOMAINS)}")
    if LOG_ALL:
        logger.log_event("记录范围: 全部域名(MITM 解密)")
    else:
        logger.log_event("记录范围: 仅监控域名做 TLS 解密全量记录；其余透明放行不记录")
    logger.log_event(f"CA 目录: {dp.CA_DIR}  (复用桌面端已信任 CA => 零配置)")
    cer = os.path.join(dp.CA_DIR, "ca.cer")
    if not os.path.isfile(cer):
        logger.log_event(f"注意: 未找到 CA({cer})，首次运行将自动生成；若监控域报证书错误，"
                         f"请以管理员运行 --install-ca 信任。")
    logger.log_event(f"日志: {LOG_FILE}")
    logger.log_event("Ctrl+C 停止。")

    try:
        while True:
            try:
                conn, addr = srv.accept()
            except Exception as e:
                logger.log_event(f"[accept] 异常(已忽略): {type(e).__name__}: {e}")
                time.sleep(0.1)
                continue
            t = threading.Thread(target=handle_client, args=(conn, addr), daemon=True)
            t.start()
    except KeyboardInterrupt:
        logger.log_event("代理停止(Ctrl+C)")
        logger.log_session_end()
        print(f"\n已停止。本次共记录 {logger.count} 条请求，日志: {LOG_FILE}")
        return 0


if __name__ == "__main__":
    sys.exit(main() or 0)
