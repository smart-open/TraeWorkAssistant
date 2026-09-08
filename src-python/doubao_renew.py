#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
豆包会话续期脚本（P3，方案依据 doubao-trae-switch-plan.md §2.3）

实测结论（2026-09-08，本机 Doubao Chromium 147）：
  桌面客户端的 cookie 值（sessionid/sid_guard 等）在 Chromium os_crypt 之下还有一层客户端级
  加密——v10/DPAPI + AES-256-GCM 解出的明文仍为二进制密文（GCM tag 验证通过，非 ASCII），
  无法离线得到明文 sessionid。因此：
  - 续期主路径 = PS 桥 KeepAlive（启动豆包 25s 让客户端自己联网滑动续期，由 Rust/PS 侧实现）；
  - 本脚本的探活巡检仅对用户**手动录入**的 sessionid 生效（doubao_account_set_credential）；
  - cookie 解密能力保留为**诊断**用途（校验 User Data / 快照 Local State+Cookies 是否完整可解）。

职责：
  1. --sync-only（诊断模式）：解密当前 User Data 与各快照槽的目标 cookie，报告可解性与明文
     特征（不写入账号池——密文不能当 sessionid 用）。
  2. 默认模式：对池内手动录入 sessionid 的账号探活保活端点（settings.doubao_renew_url，
     默认 https://www.doubao.com/）；200=有效（Set-Cookie 新值回写），302→passport / 401=
     标记过期；sid_guard 到期时间一并解析。
  3. 结果写 <data_dir>/data/doubao_renew_result.json，stdout 输出一行摘要 JSON。

依赖：标准库 + cryptography（device_proxy.py 同款，已在 requirements.txt）。
DPAPI 走 ctypes（不依赖 pywin32）。仅操作本人合法持有的账号，不破解客户端加密。
"""

import argparse
import base64
import ctypes
import ctypes.wintypes
import datetime
import json
import os
import shutil
import sqlite3
import sys
import tempfile
import urllib.error
import urllib.request
from pathlib import Path
from typing import Optional

DEFAULT_RENEW_URL = "https://www.doubao.com/"
TARGET_COOKIES = ("sessionid", "sessionid_ss", "sid_tt", "uid_tt", "sid_guard")
TIME_FMT = "%Y-%m-%d %H:%M:%S"


def now_str() -> str:
    return datetime.datetime.now().strftime(TIME_FMT)


def app_data_dir() -> Path:
    env = os.environ.get("AIWORKDATA_DIR")
    if env:
        return Path(env)
    return Path(os.environ["APPDATA"]) / "AIWorkAssistant"


# ── DPAPI / AES-GCM 解密 ────────────────────────────────────────────────────

class _DATA_BLOB(ctypes.Structure):
    _fields_ = [
        ("cbData", ctypes.wintypes.DWORD),
        ("pbData", ctypes.POINTER(ctypes.c_char)),
    ]


def dpapi_unprotect(data: bytes) -> bytes:
    """CryptUnprotectData：当前 Windows 用户上下文可解（豆包 v10 cookie 同机同用户）。"""
    buf = ctypes.create_string_buffer(data, len(data))
    blob_in = _DATA_BLOB(len(data), ctypes.cast(buf, ctypes.POINTER(ctypes.c_char)))
    blob_out = _DATA_BLOB()
    ok = ctypes.windll.crypt32.CryptUnprotectData(
        ctypes.byref(blob_in), None, None, None, None, 0, ctypes.byref(blob_out)
    )
    if not ok:
        raise OSError("CryptUnprotectData 失败（非本机/本用户加密或数据损坏）")
    try:
        return ctypes.string_at(blob_out.pbData, blob_out.cbData)
    finally:
        ctypes.windll.kernel32.LocalFree(blob_out.pbData)


def load_aes_key(user_data: Path) -> bytes:
    """Local State → os_crypt.encrypted_key（base64，DPAPI 包裹）→ AES-256 密钥。"""
    ls_path = user_data / "Local State"
    ls = json.loads(ls_path.read_text(encoding="utf-8"))
    enc_key = ls["os_crypt"]["encrypted_key"]
    raw = base64.b64decode(enc_key)
    if raw[:5] != b"DPAPI":
        raise ValueError("encrypted_key 前缀非 DPAPI，布局可能已变化")
    return dpapi_unprotect(raw[5:])


def decrypt_cookie_blob(blob: bytes, key: bytes) -> Optional[str]:
    """v10 布局：'v10' + nonce(12) + ciphertext+tag(16)。v20/app-bound 直接放弃（不攻击）。"""
    if not blob:
        return None
    if blob[:3] != b"v10":
        return None
    try:
        from cryptography.hazmat.primitives.ciphers.aead import AESGCM

        plain = AESGCM(key).decrypt(blob[3:15], blob[15:], None)
        return plain.decode("utf-8", errors="replace")
    except Exception:
        return None


def read_doubao_cookies(user_data: Path) -> dict:
    """读取并解密一个 User Data 根下的 doubao.com 域目标 cookie。返回 {name: value}。"""
    db = user_data / "Default" / "Network" / "Cookies"
    if not db.exists():
        return {}
    key = load_aes_key(user_data)
    out: dict = {}
    # 复制后再读，避免豆包运行时 SQLite 文件锁
    with tempfile.TemporaryDirectory(prefix="doubao_ck_") as td:
        tmp_db = Path(td) / "Cookies"
        shutil.copy2(db, tmp_db)
        # -wal/-shm 一并复制，尽量避免读到未 checkpoint 的空库
        for suffix in ("-wal", "-shm"):
            side = db.with_name("Cookies" + suffix)
            if side.exists():
                shutil.copy2(side, Path(td) / ("Cookies" + suffix))
        conn = sqlite3.connect(str(tmp_db))
        try:
            rows = conn.execute(
                "SELECT name, encrypted_value FROM cookies WHERE host_key LIKE '%doubao.com'"
            ).fetchall()
        finally:
            conn.close()
        for name, enc in rows:
            if name not in TARGET_COOKIES:
                continue
            val = decrypt_cookie_blob(bytes(enc), key)
            if val:
                out[name] = val
    return out


def parse_sid_guard(value: Optional[str]) -> Optional[str]:
    """sid_guard 格式：'<sid>|<create_ts 秒>|<duration 秒>|...' → 到期时间字符串。"""
    if not value:
        return None
    parts = value.split("|")
    if len(parts) < 3:
        return None
    try:
        create_ts = int(parts[1])
        duration = int(parts[2])
    except ValueError:
        return None
    if create_ts <= 0 or duration <= 0:
        return None
    expire = datetime.datetime.fromtimestamp(create_ts + duration)
    return expire.strftime(TIME_FMT)


# ── 续期巡检（网络）─────────────────────────────────────────────────────────

class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None  # 30x 不自动跟随，交由调用方判定是否跳 passport


def renew_probe(sessionid: str, url: str, timeout: int = 15) -> dict:
    """携带 cookie GET 保活端点。返回 {status: ok|expired|error, new_cookies: {}, detail}。"""
    req = urllib.request.Request(
        url,
        headers={
            "Cookie": f"sessionid={sessionid}; sessionid_ss={sessionid}",
            "User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AIWorkAssistant/1.0",
        },
        method="GET",
    )
    opener = urllib.request.build_opener(_NoRedirect)
    new_cookies: dict = {}
    try:
        resp = opener.open(req, timeout=timeout)
        # 滑动续期：响应可能 Set-Cookie 下发新 sessionid/sid_guard
        for raw in resp.headers.get_all("Set-Cookie") or []:
            for pair in raw.split(";"):
                if "=" in pair:
                    k, _, v = pair.strip().partition("=")
                    if k in TARGET_COOKIES and v:
                        new_cookies[k] = v
        # 某些网关对未登录也返回 200 + 登录页，用 Location/页面特征兜底不判定，视为有效
        return {"status": "ok", "new_cookies": new_cookies, "detail": f"HTTP {resp.status}"}
    except urllib.error.HTTPError as e:
        loc = (e.headers.get("Location") or "") if e.headers else ""
        if e.code in (301, 302, 303, 307, 308) and "passport" in loc.lower():
            return {"status": "expired", "new_cookies": {}, "detail": f"302 → {loc[:120]}"}
        if e.code == 401:
            return {"status": "expired", "new_cookies": {}, "detail": "HTTP 401"}
        return {"status": "error", "new_cookies": {}, "detail": f"HTTP {e.code} {loc[:80]}"}
    except Exception as e:  # noqa: BLE001
        return {"status": "error", "new_cookies": {}, "detail": str(e)[:160]}


# ── 账号池读写 ───────────────────────────────────────────────────────────────

def load_pool(path: Path) -> dict:
    if not path.exists():
        return {"accounts": []}
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
        if isinstance(data, dict) and isinstance(data.get("accounts"), list):
            return data
    except Exception:
        pass
    return {"accounts": []}


def save_pool(path: Path, pool: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(f".tmp.{os.getpid()}")
    tmp.write_text(json.dumps(pool, ensure_ascii=False, indent=2), encoding="utf-8")
    os.replace(tmp, path)


# ── 主流程 ───────────────────────────────────────────────────────────────────

def local_doubao_user_data() -> Path:
    return Path(os.environ["LOCALAPPDATA"]) / "Doubao" / "User Data"


def sync_cookie_state(data_dir: Path, log) -> dict:
    """诊断模式：解密当前 User Data + 各快照槽，报告可解性与明文特征（不写池）。
    实测：桌面客户端 cookie 值为客户端级二次加密的密文（解出非 ASCII），不能当 sessionid 用。"""
    profiles_root = data_dir / "data" / "profiles_doubao"
    sources = []
    try:
        import cryptography  # noqa: F401
    except ImportError:
        log("cryptography 库缺失，解密诊断不可用（生产环境应由打包 runtime 提供）")
        return {"synced": 0, "sources": [], "cryptography": False}

    def diagnose(label: str, ud: Path):
        try:
            cookies = read_doubao_cookies(ud)
        except Exception as e:  # noqa: BLE001
            log(f"{label} 解密失败：{e}")
            sources.append({"source": label, "decryptable": False, "detail": str(e)[:120]})
            return
        if not cookies:
            log(f"{label} 未解出目标 cookie（豆包可能未登录或布局变化）")
            sources.append({"source": label, "decryptable": False, "detail": "no cookies"})
            return
        ascii_n = sum(1 for v in cookies.values() if v.isascii())
        sources.append({
            "source": label,
            "decryptable": True,
            "cookies": sorted(cookies.keys()),
            "ascii_values": ascii_n,
            "note": "ascii_values>0 的值可作为明文凭证" if ascii_n else "密文（客户端级二次加密），不可作为凭证",
        })
        log(f"{label} 解出 {len(cookies)} 项（ASCII 明文 {ascii_n} 项）")

    ud = local_doubao_user_data()
    if ud.exists():
        diagnose("live", ud)
    if profiles_root.exists():
        for slot in sorted(p for p in profiles_root.iterdir() if p.is_dir() and p.name != "last"):
            diagnose(f"snapshot:{slot.name}", slot)

    return {"synced": 0, "sources": sources, "cryptography": True}


def run_renewal(data_dir: Path, renew_url: str, log) -> dict:
    """对池内手动录入 sessionid 的账号做续期探活（session_source=manual）。
    客户端二次加密密文无法作为凭证，探活不覆盖解密来源。"""
    pool_path = data_dir / "data" / "doubao_accounts.json"
    pool = load_pool(pool_path)
    accounts = pool["accounts"]
    results = []
    ok_n = expired_n = error_n = skipped_n = 0
    for acc in accounts:
        uid = acc.get("user_id", "")
        sid = acc.get("session_id")
        if not sid or acc.get("session_source") != "manual":
            skipped_n += 1
            if sid and acc.get("session_source") != "manual":
                results.append({"user_id": uid, "status": "skipped", "detail": "sessionid 非手动录入来源，不参与探活"})
            continue
        probe = renew_probe(sid, renew_url)
        entry = {"user_id": uid, "status": probe["status"], "detail": probe["detail"]}
        if probe["status"] == "ok":
            ok_n += 1
            acc["expired"] = False
            acc["last_renew_at"] = now_str()
            if probe["new_cookies"].get("sessionid"):
                acc["session_id"] = probe["new_cookies"]["sessionid"]
                entry["renewed"] = True
            if probe["new_cookies"].get("sid_guard"):
                acc["sid_guard"] = probe["new_cookies"]["sid_guard"]
                acc["session_expire_at"] = parse_sid_guard(probe["new_cookies"]["sid_guard"])
        elif probe["status"] == "expired":
            expired_n += 1
            acc["expired"] = True
            acc["last_renew_at"] = now_str()
        else:
            error_n += 1
            entry["detail"] = probe["detail"]  # 网络错误不改 expired 状态
        results.append(entry)
    if results:
        save_pool(pool_path, pool)
    return {"ok": ok_n, "expired": expired_n, "error": error_n, "skipped": skipped_n, "accounts": results}


def main() -> int:
    parser = argparse.ArgumentParser(description="豆包会话续期巡检")
    parser.add_argument("--sync-only", action="store_true", help="仅解密同步 cookie 状态，不做网络续期")
    parser.add_argument("--url", default=None, help="保活端点（默认读设置 doubao_renew_url，再退回 doubao.com 首页）")
    args = parser.parse_args()

    data_dir = app_data_dir()
    logs = []

    def log(msg: str):
        logs.append(msg)
        print(f"[doubao-renew] {msg}", file=sys.stderr)

    # 保活端点：命令行 > settings.doubao_renew_url > 默认首页
    url = args.url
    if not url:
        try:
            settings = json.loads((data_dir / "conf" / "app_settings.json").read_text(encoding="utf-8"))
            url = settings.get("doubao_renew_url") or DEFAULT_RENEW_URL
        except Exception:
            url = DEFAULT_RENEW_URL

    sync_info = sync_cookie_state(data_dir, log)

    if args.sync_only:
        summary = {"mode": "diagnose", "finished_at": now_str(), "sync": sync_info, "logs": logs}
    else:
        renew_info = run_renewal(data_dir, url, log)
        summary = {
            "mode": "full",
            "finished_at": now_str(),
            "renew_url": url,
            "sync": sync_info,
            "renew": {k: renew_info[k] for k in ("ok", "expired", "error", "skipped")},
            "accounts": renew_info["accounts"],
            "logs": logs,
        }

    result_path = data_dir / "data" / "doubao_renew_result.json"
    result_path.parent.mkdir(parents=True, exist_ok=True)
    result_path.write_text(json.dumps(summary, ensure_ascii=False, indent=2), encoding="utf-8")

    # stdout 仅输出一行摘要 JSON 供桌面端解析（进度日志走 stderr）
    print(json.dumps(summary, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
