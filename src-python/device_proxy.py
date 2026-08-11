#!/usr/bin/env python3
# -*- coding: utf-8 -*-
r"""
Trae Work 签到设备ID代理 (方案A) — Trae Work 助手 内置版
========================================================
本地 MITM 代理：透明截获 api.trae.cn 的签到请求，
按账号(从 Authorization JWT 的 data.id 取)注入各自独立的伪 x-device-id /
x-market-user-id / vscode-sessionid，从而绕过"每设备每天"签到配额。

仅对签到接口 /trae/api/v2/ug/checkin_credits/claim 改写请求头，其余流量原样转发。
首次运行会生成自签根 CA (certs/ca.crt + ca.cer)，需管理员安装到
Windows 受信任根证书颁发机构，TRAE 才会信任代理证书。

环境变量：
    PROXY_PORT        监听端口（默认 8899）
    AUTO_CAPTURE_JWT  是否自动捕获写回 accounts.json（默认 1）
    TRAEDATA_DIR      数据目录（默认脚本所在目录）；应用通过此变量指向 %APPDATA%\TraeWorkAssistant

用法：
    python device_proxy.py            # 监听 127.0.0.1:8899
    python device_proxy.py --gen-ca   # 仅生成 CA 证书后退出（供安装流程调用）
"""
import os
import sys
import json
import ssl
import socket
import threading
import base64
import random
import uuid
import hashlib
import datetime
import http.client

# ---------------- 配置 ----------------
LISTEN_HOST = "127.0.0.1"
LISTEN_PORT = int(os.environ.get("PROXY_PORT", "8899"))
BASE = os.path.dirname(os.path.abspath(__file__))
# 数据目录：优先 TRAEDATA_DIR（由桌面端注入），否则回退到脚本目录（保持独立可用性）
DATA_DIR = os.environ.get("TRAEDATA_DIR", BASE)
MAP_FILE = os.path.join(DATA_DIR, "device_map.json")
LOG_FILE = os.path.join(DATA_DIR, "proxy.log")
CA_DIR = os.path.join(DATA_DIR, "certs")
SIGNIN_PATH = "/trae/api/v2/ug/checkin_credits/claim"
STATUS_PATH = "/trae/api/v2/ug/checkin_credits/status"
ACCOUNTS_FILE = os.path.join(DATA_DIR, "checkin_accounts.json")
# 默认开启自动捕获 JWT 写回 checkin_accounts.json；设 AUTO_CAPTURE_JWT=0 关闭
AUTO_CAPTURE_JWT = os.environ.get("AUTO_CAPTURE_JWT", "1") not in ("0", "false", "False", "")

# ---------------- 日志 ----------------
_log_lock = threading.Lock()
os.makedirs(os.path.dirname(LOG_FILE), exist_ok=True)
_logf = open(LOG_FILE, "a", encoding="utf-8", buffering=1)

def log(*a):
    ts = datetime.datetime.now().strftime("%Y-%m-%d %H:%M:%S")
    line = f"[{ts}] " + " ".join(str(x) for x in a)
    with _log_lock:
        print(line, flush=True)
        try:
            _logf.write(line + "\n")
        except Exception:
            pass

# ---------------- 每账号设备映射 ----------------
_map_lock = threading.Lock()
_device_map = {}

def load_map():
    global _device_map
    if os.path.exists(MAP_FILE):
        try:
            with open(MAP_FILE, "r", encoding="utf-8") as f:
                _device_map = json.load(f)
        except Exception as e:
            log("读取映射失败:", e)
            _device_map = {}
    log(f"已加载设备映射: {len(_device_map)} 个账号")

def save_map():
    try:
        os.makedirs(os.path.dirname(MAP_FILE), exist_ok=True)
        with open(MAP_FILE, "w", encoding="utf-8") as f:
            json.dump(_device_map, f, ensure_ascii=False, indent=2)
    except Exception as e:
        log("保存映射失败:", e)

def _normalize_seed(seed):
    """将任意 seed 归一为稳定 int（与 auto_checkin.py 完全一致）。"""
    if isinstance(seed, int):
        return seed & 0x7FFFFFFFFFFFFFFF
    if isinstance(seed, str):
        if seed.isdigit():
            return int(seed) & 0x7FFFFFFFFFFFFFFF
        return int(hashlib.sha256(seed.encode("utf-8")).hexdigest(), 16) & 0x7FFFFFFFFFFFFFFF
    return seed


def _stable_rng(seed):
    """基于 seed 的稳定随机数生成器（与 auto_checkin.py 保持一致）。"""
    return random.Random(_normalize_seed(seed))


def rand_digits(n, seed=None):
    if seed is None:
        return "".join(random.choice("0123456789") for _ in range(n))
    return "".join(_stable_rng(seed).choice("0123456789") for _ in range(n))


def rand_hex(n, seed=None):
    if seed is None:
        return "".join(random.choice("0123456789abcdef") for _ in range(n))
    return "".join(_stable_rng(seed).choice("0123456789abcdef") for _ in range(n))


def get_device_for(user_id):
    with _map_lock:
        if user_id not in _device_map:
            _device_map[user_id] = {
                "device_id": rand_digits(15, seed=user_id),
                "market_user_id": str(uuid.UUID(int=_stable_rng(user_id).getrandbits(128))),
                "session_id": rand_hex(64, seed=user_id),
                "created": datetime.datetime.now().isoformat(timespec="seconds"),
            }
            save_map()
            log(f"  >> 新账号注册设备: user={user_id} -> x-device-id={_device_map[user_id]['device_id']}")
        return _device_map[user_id]

# ---------------- JWT user id 提取(不校验签名) ----------------
def extract_user_id(auth_header):
    if not auth_header:
        return None
    parts = auth_header.split(None, 1)
    token = parts[1] if len(parts) == 2 else parts[0]
    segs = token.split(".")
    if len(segs) < 2:
        return None
    try:
        pad = segs[1] + "=" * (-len(segs[1]) % 4)
        payload = json.loads(base64.urlsafe_b64decode(pad))
        data = payload.get("data", {})
        if isinstance(data, dict) and data.get("id"):
            return data.get("id")
        if payload.get("auth_id"):
            return payload.get("auth_id")
        if payload.get("sub"):
            return payload.get("sub")
        return None
    except Exception:
        return None


def get_jwt_exp(jwt_full):
    """从 JWT 解 exp 字段返回 (datetime, None)；解析失败返回 (None, None)。"""
    if not jwt_full:
        return None, None
    token = jwt_full
    if token.startswith("Cloud-IDE-JWT "):
        token = token.split(None, 1)[1]
    segs = token.split(".")
    if len(segs) < 2:
        return None, None
    try:
        pad = segs[1] + "=" * (-len(segs[1]) % 4)
        payload = json.loads(base64.urlsafe_b64decode(pad))
        exp = payload.get("exp")
        if not isinstance(exp, (int, float)):
            return None, None
        return datetime.datetime.fromtimestamp(exp), exp
    except Exception:
        return None, None


# ---------------- accounts.json 捕获写回 ----------------
_accounts_lock = threading.RLock()  # 用 RLock 以便 update_account_jwt 持锁后再调 save_accounts
_accounts_cache = None
_accounts_mtime = None


def load_accounts(force_reload=False):
    """加载 checkin_accounts.json；带 mtime 检测，文件被外部改动时自动 reload。"""
    global _accounts_cache, _accounts_mtime
    with _accounts_lock:
        if not os.path.exists(ACCOUNTS_FILE):
            _accounts_cache = {"accounts": []}
            _accounts_mtime = 0
            return _accounts_cache
        try:
            mtime = os.path.getmtime(ACCOUNTS_FILE)
        except OSError:
            mtime = 0
        if not force_reload and _accounts_cache is not None and _accounts_mtime == mtime:
            return _accounts_cache
        try:
            with open(ACCOUNTS_FILE, "r", encoding="utf-8") as f:
                _accounts_cache = json.load(f)
            _accounts_mtime = mtime
            log(f"  [accounts] 重新加载 {len(_accounts_cache.get('accounts', []))} 个账号")
        except Exception as e:
            log(f"  [accounts] 读取失败: {e}")
            _accounts_cache = {"accounts": []}
            _accounts_mtime = mtime
        return _accounts_cache


def save_accounts():
    """把内存中的 accounts 写回磁盘（持锁原子替换）。"""
    global _accounts_cache, _accounts_mtime
    with _accounts_lock:
        if _accounts_cache is None:
            return
        os.makedirs(os.path.dirname(ACCOUNTS_FILE), exist_ok=True)
        tmp = ACCOUNTS_FILE + ".tmp"
        try:
            with open(tmp, "w", encoding="utf-8") as f:
                json.dump(_accounts_cache, f, ensure_ascii=False, indent=2)
            os.replace(tmp, ACCOUNTS_FILE)
            _accounts_mtime = os.path.getmtime(ACCOUNTS_FILE)
        except Exception as e:
            log(f"  [accounts] 写入失败: {e}")


def update_account_jwt(user_id, jwt_full):
    """
    按 user_id 查找账号，更新 jwt 字段。规则：
      - 新 JWT 的 exp 必须 ≥ 旧 JWT 的 exp，否则跳过（防止覆盖更新的 token）
      - 找不到对应账号 → 追加为 auto_<user_id前8位> 新账号
    返回 'updated' / 'appended' / 'skipped' / 'unchanged'
    """
    global _accounts_cache
    cfg = load_accounts()
    new_exp_dt, new_exp_ts = get_jwt_exp(jwt_full)
    new_exp_str = new_exp_dt.strftime("%Y-%m-%d %H:%M") if new_exp_dt else "?"
    with _accounts_lock:
        accounts = cfg.get("accounts", [])
        target = None
        for a in accounts:
            if str(a.get("UserID", "")) == str(user_id):
                target = a
                break
        if target:
            old_exp_dt, _ = get_jwt_exp(target.get("jwt", ""))
            if target.get("jwt") == jwt_full:
                return "unchanged"
            # 防降级：新 token 过期时间 ≤ 旧的 → 跳过
            if old_exp_dt and new_exp_dt and new_exp_dt <= old_exp_dt:
                old_exp_str = old_exp_dt.strftime("%Y-%m-%d %H:%M")
                log(f"  [JWT 跳过(更旧)] user={user_id} 账号={target.get('name', '?')} "
                    f"旧 exp={old_exp_str} 新 exp={new_exp_str}")
                return "skipped"
            target["jwt"] = jwt_full
            target["updated_at"] = datetime.datetime.now().isoformat(timespec="seconds")
            log(f"  [JWT 自动更新] user={user_id} 账号={target.get('name', '?')} exp={new_exp_str}")
            save_accounts()
            return "updated"
        # 新账号
        new_acc = {
            "name": f"auto_{str(user_id)[:8]}",
            "UserID": user_id,
            "jwt": jwt_full,
            "added_at": datetime.datetime.now().isoformat(timespec="seconds"),
        }
        accounts.append(new_acc)
        log(f"  [JWT 自动追加新账号] user={user_id} -> name={new_acc['name']} exp={new_exp_str}")
        save_accounts()
        return "appended"

# ---------------- CA / 叶子证书 ----------------
_ca_cert = _ca_key = None
_leaf_cache = {}

def ensure_ca():
    global _ca_cert, _ca_key
    cert_pem = os.path.join(CA_DIR, "ca.crt")
    key_pem = os.path.join(CA_DIR, "ca.key")
    cer_der = os.path.join(CA_DIR, "ca.cer")
    if os.path.exists(cert_pem) and os.path.exists(key_pem):
        from cryptography.hazmat.primitives.serialization import load_pem_private_key
        from cryptography import x509
        with open(cert_pem, "rb") as f:
            _ca_cert = x509.load_pem_x509_certificate(f.read())
        with open(key_pem, "rb") as f:
            _ca_key = load_pem_private_key(f.read(), password=None)
        log("已加载现有 CA:", cert_pem)
        return
    from cryptography import x509
    from cryptography.x509.oid import NameOID
    from cryptography.hazmat.primitives import hashes, serialization
    from cryptography.hazmat.primitives.asymmetric import rsa
    os.makedirs(CA_DIR, exist_ok=True)
    key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    subj = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "TraeDeviceProxyCA")])
    cert = (
        x509.CertificateBuilder()
        .subject_name(subj)
        .issuer_name(subj)
        .public_key(key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(days=1))
        .not_valid_after(datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(days=3650))
        .add_extension(x509.BasicConstraints(ca=True, path_length=None), critical=True)
        .add_extension(
            x509.KeyUsage(
                digital_signature=True, content_commitment=False, key_encipherment=False,
                data_encipherment=False, key_agreement=False, key_cert_sign=True,
                crl_sign=True, encipher_only=False, decipher_only=False,
            ),
            critical=True,
        )
        .sign(key, hashes.SHA256())
    )
    with open(cert_pem, "wb") as f:
        f.write(cert.public_bytes(serialization.Encoding.PEM))
    with open(key_pem, "wb") as f:
        f.write(key.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.TraditionalOpenSSL, serialization.NoEncryption()))
    with open(cer_der, "wb") as f:
        f.write(cert.public_bytes(serialization.Encoding.DER))
    _ca_cert, _ca_key = cert, key
    log("已生成自签 CA ->", cert_pem, "/", cer_der)

def leaf_cert(host):
    if host in _leaf_cache:
        return _leaf_cache[host]
    from cryptography import x509
    from cryptography.x509.oid import NameOID
    from cryptography.hazmat.primitives import hashes, serialization
    from cryptography.hazmat.primitives.asymmetric import rsa
    key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    cert = (
        x509.CertificateBuilder()
        .subject_name(x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, host)]))
        .issuer_name(_ca_cert.subject)
        .public_key(key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(days=1))
        .not_valid_after(datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(days=3650))
        .add_extension(x509.SubjectAlternativeName([x509.DNSName(host)]), critical=False)
        .add_extension(x509.BasicConstraints(ca=False, path_length=None), critical=True)
        .sign(_ca_key, hashes.SHA256())
    )
    cpath = os.path.join(CA_DIR, f"leaf_{host}.crt")
    kpath = os.path.join(CA_DIR, f"leaf_{host}.key")
    with open(cpath, "wb") as f:
        f.write(cert.public_bytes(serialization.Encoding.PEM))
    with open(kpath, "wb") as f:
        f.write(key.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.TraditionalOpenSSL, serialization.NoEncryption()))
    _leaf_cache[host] = (cpath, kpath)
    return cpath, kpath

# ---------------- HTTP 请求/响应读写 ----------------
def recv_until(sock, terminator, buf=b""):
    while terminator not in buf:
        chunk = sock.recv(4096)
        if not chunk:
            return None
        buf += chunk
    return buf

def read_http_request(sock):
    buf = recv_until(sock, b"\r\n\r\n")
    if buf is None:
        return None
    header_blob, _, rest = buf.partition(b"\r\n\r\n")
    lines = header_blob.split(b"\r\n")
    first = lines[0].decode("latin1")
    parts = first.split(" ")
    method = parts[0]
    path = parts[1] if len(parts) > 1 else "/"
    version = parts[2] if len(parts) > 2 else "HTTP/1.1"
    headers = {}
    for line in lines[1:]:
        if b":" in line:
            k, _, v = line.partition(b":")
            headers[k.decode("latin1").strip().lower()] = v.decode("latin1").strip()
    cl = int(headers.get("content-length", 0) or 0)
    body = rest
    while len(body) < cl:
        chunk = sock.recv(4096)
        if not chunk:
            break
        body += chunk
    return method, path, version, headers, body[:cl] if cl else body

def send_response(sock, status, reason, headers, body):
    if isinstance(body, str):
        body = body.encode("utf-8")
    out = {}
    for k, v in headers.items():
        if k.lower() in ("transfer-encoding", "connection", "keep-alive"):
            continue
        out[k] = v
    out["Content-Length"] = str(len(body))
    out["Connection"] = "keep-alive"
    head = [f"HTTP/1.1 {status} {reason}".encode("latin1")]
    for k, v in out.items():
        head.append(f"{k}: {v}".encode("latin1"))
    sock.sendall(b"\r\n".join(head) + b"\r\n\r\n" + body)

HOP_BY_HOP = {"proxy-connection", "connection", "keep-alive", "proxy-authorization", "host", "content-length"}

def forward_upstream(host, port, method, path, headers, body, client_sock):
    ctx = ssl.create_default_context()
    conn = http.client.HTTPSConnection(host, port, context=ctx, timeout=30)
    fwd = {k: v for k, v in headers.items() if k.lower() not in HOP_BY_HOP}
    try:
        body_arg = body if method.upper() != "GET" else None
        conn.request(method, path, body=body_arg, headers=fwd)
        resp = conn.getresponse()
        resp_body = resp.read()
        send_response(client_sock, resp.status, resp.reason, dict(resp.getheaders()), resp_body)
    except Exception as e:
        log("  upstream error:", e)
        send_response(client_sock, 502, "Bad Gateway", {}, b"Bad Gateway")
    finally:
        conn.close()

# ---------------- 隧道(HTTPS over CONNECT) ----------------
def tunnel_https(tls, host, port):
    while True:
        try:
            req = read_http_request(tls)
        except Exception as e:
            log("  read tls err:", e)
            break
        if req is None:
            break
        method, path, version, headers, body = req
        log(f"  {method} {path}")
        # 自动捕获 JWT：凡是带 authorization: Cloud-IDE-JWT 的 api.trae.cn 请求都捕获
        # （覆盖 sign-in / status / 任意接口），保证 13 天换号时只需在 TRAE 里点一次签到
        if AUTO_CAPTURE_JWT and host == "api.trae.cn":
            auth = headers.get("authorization") or headers.get("x-cloudide-token")
            if auth and auth.strip():
                uid_c = extract_user_id(auth)
                if uid_c and auth.strip().startswith("Cloud-IDE-JWT"):
                    update_account_jwt(uid_c, auth.strip())
        # 设备头改写（仅签到接口）
        if SIGNIN_PATH in path:
            auth = headers.get("authorization")
            uid = extract_user_id(auth)
            if uid:
                dev = get_device_for(uid)
                headers["x-device-id"] = dev["device_id"]
                headers["x-market-user-id"] = dev["market_user_id"]
                headers["vscode-sessionid"] = dev["session_id"]
                log(f"  [签到改写] user={uid} -> x-device-id={dev['device_id']} x-market-user-id={dev['market_user_id'][:8]}... vscode-sessionid={dev['session_id'][:8]}...")
            else:
                log("  [签到] 未解析到 user id，未改写")
        forward_upstream(host, port, method, path, headers, body, tls)

# ---------------- 明文 HTTP 代理 ----------------
def handle_plain(conn, buf):
    header_blob, _, rest = buf.partition(b"\r\n\r\n")
    lines = header_blob.split(b"\r\n")
    first = lines[0].decode("latin1")
    parts = first.split(" ")
    method = parts[0]
    target = parts[1]
    from urllib.parse import urlparse
    u = urlparse(target)
    host = u.hostname
    port = u.port or (443 if u.scheme == "https" else 80)
    headers = {}
    for line in lines[1:]:
        if b":" in line:
            k, _, v = line.partition(b":")
            headers[k.decode("latin1").strip().lower()] = v.decode("latin1").strip()
    cl = int(headers.get("content-length", 0) or 0)
    body = rest
    while len(body) < cl:
        chunk = conn.recv(4096)
        if not chunk:
            break
        body += chunk
    body = body[:cl] if cl else body
    ctx = ssl.create_default_context() if u.scheme == "https" else None
    c = http.client.HTTPSConnection(host, port, context=ctx, timeout=30) if u.scheme == "https" else http.client.HTTPConnection(host, port, timeout=30)
    fwd = {k: v for k, v in headers.items() if k.lower() not in HOP_BY_HOP}
    try:
        c.request(method, u.path or "/", body=body if method.upper() != "GET" else None, headers=fwd)
        resp = c.getresponse()
        send_response(conn, resp.status, resp.reason, dict(resp.getheaders()), resp.read())
    except Exception as e:
        log("  plain upstream err:", e)
        send_response(conn, 502, "Bad Gateway", {}, b"Bad Gateway")
    finally:
        c.close()

# ---------------- 客户端连接分发 ----------------
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
        head = buf.split(b"\r\n", 1)[0].decode("latin1")
        method = head.split(" ")[0]
        if method == "CONNECT":
            target = head.split(" ")[1]
            host = target.rsplit(":", 1)[0]
            port = int(target.rsplit(":", 1)[1]) if ":" in target else 443
            conn.sendall(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            cpath, kpath = leaf_cert(host)
            ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            ctx.load_cert_chain(certfile=cpath, keyfile=kpath)
            tls = ctx.wrap_socket(conn, server_side=True)
            log(f"CONNECT {host}:{port}")
            tunnel_https(tls, host, port)
        else:
            handle_plain(conn, buf)
    except Exception as e:
        log("client err:", e)
    finally:
        try:
            conn.close()
        except Exception:
            pass

# ---------------- 主入口 ----------------
def main():
    # --gen-ca：仅生成 CA 证书后退出（供桌面端证书安装流程调用）
    if "--gen-ca" in sys.argv:
        ensure_ca()
        log("CA 证书已生成，退出。")
        return 0

    ensure_ca()
    load_map()
    if AUTO_CAPTURE_JWT:
        load_accounts()  # 预热 accounts 缓存
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind((LISTEN_HOST, LISTEN_PORT))
    srv.listen(128)
    log(f"代理已启动: {LISTEN_HOST}:{LISTEN_PORT}  (仅改写签到接口 {SIGNIN_PATH})")
    log(f"映射文件: {MAP_FILE}   日志: {LOG_FILE}   accounts: {ACCOUNTS_FILE}")
    log(f"自动捕获 JWT 写回 accounts.json: {'开' if AUTO_CAPTURE_JWT else '关 (AUTO_CAPTURE_JWT=0)'}")
    if AUTO_CAPTURE_JWT:
        log("  → TRAE 中点签到时，新 JWT 会自动覆盖到 checkin_accounts.json（按 user_id 匹配，带 exp 防降级）")
    log("请把 CA 证书 certs/ca.cer 安装到 Windows 受信任根证书颁发机构(管理员)。")
    try:
        while True:
            conn, addr = srv.accept()
            t = threading.Thread(target=handle_client, args=(conn, addr), daemon=True)
            t.start()
    except KeyboardInterrupt:
        log("代理停止")

if __name__ == "__main__":
    sys.exit(main() or 0)
