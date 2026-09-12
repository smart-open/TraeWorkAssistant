#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
豆包会话续期脚本（P3，方案依据 doubao-trae-switch-plan.md §2.3）

实测结论（2026-09-08，本机 Doubao Chromium 147）：
  桌面客户端的 cookie 值（sessionid/sid_guard 等）在 Chromium os_crypt 之下还有一层客户端级
  加密——v10/DPAPI + AES-256-GCM 解出的明文仍为二进制密文（GCM tag 验证通过，非 ASCII），
  无法离线得到明文 sessionid。因此：
  - 续期主路径 = PS 桥 KeepAlive（启动豆包 25s 让客户端自己联网滑动续期，由 Rust/PS 侧实现）；
  - 本脚本的探活巡检对池内**明文 sessionid 凭证**生效（来源不限：手动录入
    doubao_account_set_credential 或代理抓包自动回写，两者同为明文、同等可用）；
  - cookie 解密能力保留为**诊断**用途（校验 User Data / 快照 Local State+Cookies 是否完整可解）。

职责：
  1. --sync-only（诊断模式）：解密当前 User Data 与各快照槽的目标 cookie，报告可解性与明文
     特征（不写入账号池——密文不能当 sessionid 用）。
  2. 默认模式：对池内有明文 sessionid 的账号先用已登录 JSON 端点（DEFAULT_PROBE_URL）
     权威判定会话有效性，判定有效后再请求保活端点（settings.doubao_renew_url，
     默认 https://www.doubao.com/info/v2/，仅作保活与 Set-Cookie 抓取——其 200 恒真
     不可用于有效性判定）回写新值；判定失效标记过期；sid_guard 到期时间一并解析。
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

# 中文 Windows 管道默认 GBK：强制 stdout/stderr UTF-8，供桌面端按 UTF-8 解码
try:
    sys.stdout.reconfigure(encoding="utf-8")
    sys.stderr.reconfigure(encoding="utf-8")
except AttributeError:
    pass
import tempfile
import urllib.error
import urllib.request
from pathlib import Path
from typing import Optional

# 轻量探活端点（必须登录）：200=有效 / 302→passport=过期；代理日志实测确认
DEFAULT_RENEW_URL = "https://www.doubao.com/info/v2/"
TARGET_COOKIES = ("sessionid", "sessionid_ss", "sid_tt", "uid_tt", "sid_guard")
TIME_FMT = "%Y-%m-%d %H:%M:%S"


def now_str() -> str:
    return datetime.datetime.now().strftime(TIME_FMT)


def app_data_dir() -> Path:
    """数据目录，与 device_proxy.py 同链：AIWORKDATA_DIR → 旧 TRAEDATA_DIR → 脚本所在目录
    （读取 env 时兼容新旧两个变量名，保证独立运行与桌面端注入两种场景一致）。"""
    env = os.environ.get("AIWORKDATA_DIR") or os.environ.get("TRAEDATA_DIR")
    if env:
        return Path(env)
    return Path(__file__).resolve().parent


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


def _chromium_profiles(user_data: Path) -> list:
    """列出 User Data 根（或快照槽）下的 Profile 目录（Default + Profile N）。"""
    if not user_data.is_dir():
        return []
    try:
        return [p for p in sorted(user_data.iterdir())
                if p.is_dir() and (p.name == "Default" or p.name.startswith("Profile "))]
    except OSError:
        return []


def _read_active_profile_name(user_data: Path) -> Optional[str]:
    """读 Local State → profile.last_used（客户端当前活跃 Profile 目录名）。"""
    ls = user_data / "Local State"
    if not ls.is_file():
        return None
    try:
        data = json.loads(ls.read_text(encoding="utf-8"))
        name = str(data.get("profile", {}).get("last_used") or "").strip()
        return name or None
    except Exception:
        return None


def _read_profile_target_cookies(prof: Path, key: bytes) -> dict:
    """读取并解密单个 Profile 的 doubao.com 域目标 cookie（复制后读，规避运行时文件锁）。
    返回 {name: value}。"""
    db = prof / "Network" / "Cookies"
    if not db.exists():
        db = prof / "Cookies"  # 旧布局兜底
        if not db.exists():
            return {}
    out: dict = {}
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


def read_doubao_cookies(user_data: Path) -> dict:
    """读取并解密一个 User Data 根下的 doubao.com 域目标 cookie。返回 {name: value}。

    多 Profile（豆包自带账号隔离，登录会话可能位于任意 Profile）：活跃 Profile
    （Local State → profile.last_used）优先取值，其余 Profile 按目录序兜底补缺
    （不覆盖高优先级 Profile 已取到的键）。"""
    profiles = _chromium_profiles(user_data)
    if not profiles:
        return {}
    active = _read_active_profile_name(user_data)
    if active:
        profiles.sort(key=lambda p: p.name != active)  # 活跃排最前（稳定排序）
    key = load_aes_key(user_data)
    out: dict = {}
    for prof in profiles:
        for name, val in _read_profile_target_cookies(prof, key).items():
            out.setdefault(name, val)
    return out


def read_multi_sids(user_data: Path) -> dict:
    """读 Local State → saman.local_storage_app_for_web.enterprise 内嵌的 x-tt-multi-sids
    （uid → 明文 sessionid 映射）。客户端把当前全部账号的会话 sid 明文缓存在这里——
    Cookies 解出的是客户端级二次加密密文（不可用），此处是唯一可离线验证的会话来源。"""
    ls = user_data / "Local State"
    if not ls.is_file():
        return {}
    try:
        data = json.loads(ls.read_text(encoding="utf-8"))
        ent = str(data.get("saman", {}).get("local_storage_app_for_web", {}).get("enterprise") or "")
    except Exception:
        return {}
    i = ent.find("x-tt-multi-sids")
    if i < 0:
        return {}
    raw = ent[i + len('x-tt-multi-sids":"'):]
    raw = raw.split('"')[0]
    import urllib.parse

    out: dict = {}
    for pair in urllib.parse.unquote(raw).split("|"):
        k, sep, v = pair.partition(":")
        if sep and k.strip().isdigit() and v.strip():
            out[k.strip()] = v.strip()
    return out


def probe_slot_session(user_data: Path, uid: str, url: str) -> dict:
    """切换/一键打开前预检：目标槽位存储的会话在服务端是否仍有效。
    背景（2026-09-09 实测日志）：在豆包客户端内「退出登录」会调 passport/web/logout 吊销
    该账号的服务端会话——快照文件虽完好，里面的会话已被判死；恢复后客户端启动一联网即
    SESSION_EXPIRED 强制登出（表现为「切换成功但未登录」，且仅被登出过的账号中招）。
    返回 {status: ok|expired|unknown, detail, source}。"""
    sid = read_multi_sids(user_data).get(uid, "") if uid else ""
    source = "local_state_multi_sids"
    if not sid:
        # 兜底：cookie 解密（仅 ASCII 明文可用；密文为客户端级二次加密，不可验证）
        try:
            cookies = read_doubao_cookies(user_data)
        except Exception:
            cookies = {}
        cand = cookies.get("sessionid") or cookies.get("sessionid_ss") or ""
        if cand and cand.isascii():
            sid = cand
            source = "cookie_decrypt"
    if not sid:
        return {
            "status": "unknown",
            "detail": "槽位无可验证会话凭证（multi-sids 无该 uid 且无明文 cookie）",
            "source": source,
        }
    r = api_probe(sid, url)
    return {"status": r["status"], "detail": r["detail"], "source": source}


def parse_sid_guard(value: Optional[str]) -> Optional[str]:
    """sid_guard 格式：'<sid>|<create_ts 秒>|<duration 秒>|...' → 到期时间字符串。
    池内存储保持 Set-Cookie 下发的 URL 编码原样（| 为 %7C，对话 API 需逐字节一致），
    解析前先解码。"""
    if not value:
        return None
    if "%" in value:
        import urllib.parse

        value = urllib.parse.unquote(value)
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
    # 显式绕过系统代理：探活是直连 API 校验，走 MITM 代理毫无收益且会撞上
    # 代理动态证书的兼容性问题（urllib/OpenSSL 3.x 拒绝缺 AKI 扩展的叶子证书）
    opener = urllib.request.build_opener(_NoRedirect, urllib.request.ProxyHandler({}))
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


# ── 会话有效性探测（切换前预检）─────────────────────────────────────────────

# POST 已登录 JSON 端点：code=0 有效 / code=710012001 登录态失效（doubao_quota.py 同款语义）。
# 注意 GET info/v2/ 不可用作判定——网关对任意 sid（含垃圾值）一律 200 + SPA HTML 首页
# （实测 2026-09-09），按 200=有效判定会全部假阳性。
DEFAULT_PROBE_URL = "https://www.doubao.com/alice/commerce/sale/subscription/quota/summary/"
SESSION_EXPIRED_CODE = 710012001


def api_probe(sessionid: str, url: str, timeout: int = 15) -> dict:
    """携带会话 POST 已登录端点，返回 {status: ok|expired|error|unknown, detail}。"""
    body = json.dumps({"product_line": "membership"}).encode("utf-8")
    req = urllib.request.Request(
        url,
        data=body,
        headers={
            "Cookie": f"sessionid={sessionid}; sessionid_ss={sessionid}",
            "User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AIWorkAssistant/1.0",
            "Referer": "https://www.doubao.com/",
            "Accept": "application/json, text/plain, */*",
            "Content-Type": "application/json",
        },
        method="POST",
    )
    # 显式绕过系统代理（同 renew_probe）：直连 API 校验，避免 MITM 代理动态证书兼容性问题
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    try:
        with opener.open(req, timeout=timeout) as resp:
            data = json.loads(resp.read(512 * 1024).decode("utf-8", errors="replace"))
    except urllib.error.HTTPError as e:
        loc = (e.headers.get("Location") or "") if e.headers else ""
        if e.code in (301, 302, 303, 307, 308) and "passport" in loc.lower():
            return {"status": "expired", "detail": f"302 → {loc[:120]}"}
        if e.code == 401:
            return {"status": "expired", "detail": "HTTP 401"}
        return {"status": "error", "detail": f"HTTP {e.code} {loc[:80]}"}
    except Exception as e:  # noqa: BLE001
        return {"status": "error", "detail": str(e)[:160]}
    if not isinstance(data, dict):
        return {"status": "error", "detail": "响应非 JSON 对象"}
    code = data.get("code")
    if code in (0, None):
        return {"status": "ok", "detail": "code=0"}
    if code == SESSION_EXPIRED_CODE:
        return {"status": "expired", "detail": "code=710012001（登录态失效，会话已被服务端吊销或过期）"}
    return {"status": "unknown", "detail": f"code={code}（业务拒绝，无法判定会话状态）"}


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
    """对池内有明文 sessionid 的账号做两段式探活续期。
    来源不限：手动录入（manual）与代理抓包自动回写（proxy）同为明文凭证，同等参与探活；
    池内不存在密文来源（cookie 诊断只报告不写池）。

    两段式（修复「死会话被洗白」）：
      ① 先用已登录 JSON 端点 api_probe(DEFAULT_PROBE_URL) 判定会话有效性——
         info/v2/（默认 renew_url）对任意 sid（含垃圾值）一律 200+SPA HTML，
         200 恒真不可用于有效性判定（实测 2026-09-09，见 DEFAULT_PROBE_URL 处注释），
         直接按 200=有效会把已吊销会话洗白回 expired=False；
      ② 判定 ok 后才调 renew_probe(renew_url) 做保活请求并抓取 Set-Cookie 回写
         （renew_url 仅作保活用途）；判定 expired 置 expired=True；error 不改状态。"""
    pool_path = data_dir / "data" / "doubao_accounts.json"
    pool = load_pool(pool_path)
    accounts = pool["accounts"]
    results = []
    ok_n = expired_n = error_n = skipped_n = 0
    for acc in accounts:
        uid = acc.get("user_id", "")
        sid = acc.get("session_id")
        if not sid:
            skipped_n += 1
            continue
        # ① 权威判定：已登录 JSON 端点（code=0 有效 / 710012001 失效）
        first = api_probe(sid, DEFAULT_PROBE_URL)
        if first["status"] == "ok":
            # ② 会话有效 → 保活请求抓续期 Cookie（renew_url/info/v2/ 仅作保活用途）
            second = renew_probe(sid, renew_url)
            if second["status"] == "ok":
                probe = second
            elif second["status"] == "expired":
                # 保活端点 302→passport/401 是明确失效信号，推翻 ok 判定按过期处理
                probe = second
            else:
                # 保活请求网络失败：会话刚被权威端点判定有效，不因保活失败改状态，
                # 仅跳过 Cookie 回写（status=ok、无 new_cookies）
                probe = {"status": "ok", "new_cookies": {},
                         "detail": f"probe ok; renew: {second['detail']}"}
        elif first["status"] == "expired":
            probe = {"status": "expired", "new_cookies": {}, "detail": first["detail"]}
        else:
            # error / unknown：无法判定会话状态，不改状态（不洗白也不误杀）
            probe = {"status": "error", "new_cookies": {}, "detail": first["detail"]}
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
    parser.add_argument("--probe-user-data", metavar="DIR", dest="probe_user_data",
                        help="预检指定 User Data/快照槽的会话服务端有效性（配合 --probe-uid，切换前拦截死会话）")
    parser.add_argument("--probe-uid", default="", help="预检目标的账号 user_id（取 x-tt-multi-sids 中该 uid 的 sid）")
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

    # 切换前预检模式：只探测目标槽位会话有效性（不写任何文件、不走巡检流程）。
    # 探测端点用已登录 JSON 接口（DEFAULT_PROBE_URL），不用 doubao_renew_url——
    # info/v2/ 对任意 sid 都返回 200+HTML，无法判定（实测 2026-09-09）。
    # 退出码：0=有效 / 3=服务端明确失效（调用方应中止切换）/ 1=无法验证（调用方 fail-open）。
    if args.probe_user_data:
        probe_url = args.url or DEFAULT_PROBE_URL
        ud = Path(args.probe_user_data)
        if not ud.is_dir():
            print(json.dumps({"status": "unknown", "detail": f"槽位目录不存在: {args.probe_user_data}",
                              "source": ""}, ensure_ascii=False))
            return 1
        res = probe_slot_session(ud, args.probe_uid.strip(), probe_url)
        print(json.dumps(res, ensure_ascii=False))
        return 0 if res["status"] == "ok" else (3 if res["status"] == "expired" else 1)

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
