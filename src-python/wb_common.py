#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""WorkBuddy 公共库：数据目录 / 账号池 / 双源凭证 / 统一请求头 / 宽容解析 / token 刷新。

方案依据 docs/workbuddy-product-design.md §5.1~§5.4：
- 凭证双源化（F-10）：工具侧副本 token_store 与桌面 auth 文件「谁新用谁」（expiresAtMs 晚者胜出）
- 统一请求头（§5.3）+ 宽容解析 dig()（§5.4，对齐 Rust fs_utils::dig 语义）
- 红线：chat 请求绝不携带 X-Refresh-Token；本库仅 refresh 端点携带
仅标准库，零第三方依赖。
"""

import datetime
import hashlib
import json
import os
import urllib.error
import urllib.request

# ── 路径 ────────────────────────────────────────────────────────────────────

def data_dir() -> str:
    return os.environ.get("AIWORKDATA_DIR") or os.path.join(
        os.environ.get("APPDATA", ""), "AIWorkAssistant")


def data_subdir() -> str:
    """统一数据子目录（<data_dir>/data/）：与 Rust fs_utils 对齐（§9.4 网关迁移）。
    池/凭证/签到结果/积分缓存等 WorkBuddy 数据文件均落盘于此，路径不一致会导致
    Python 侧读到空池（签到空跑、积分为空）。"""
    p = os.path.join(data_dir(), "data")
    os.makedirs(p, exist_ok=True)
    return p


def auth_file_path() -> str:
    """桌面端 auth 文件（只读；写入仅限客户端关闭窗口期，由 PS 桥/Rust 控制）"""
    local = os.environ.get("LOCALAPPDATA", "")
    return os.path.join(local, "CodeBuddyExtension", "Data", "Public", "auth",
                        "workbuddy-desktop.info")


def wb_data_dir() -> str:
    return os.path.join(os.path.expanduser("~"), ".workbuddy")


def pool_path() -> str:
    return os.path.join(data_subdir(), "workbuddy_accounts.json")


def token_store_path() -> str:
    return os.path.join(data_subdir(), "workbuddy_token_store.json")


def checkin_results_path() -> str:
    return os.path.join(data_subdir(), "workbuddy_checkin_results.json")


def credits_cache_path() -> str:
    return os.path.join(data_subdir(), "workbuddy_credits_cache.json")


def load_json(path, default):
    try:
        with open(path, "r", encoding="utf-8") as f:
            raw = f.read()
        if not raw.strip():
            return default
        return json.loads(raw)
    except Exception:
        return default


def write_json_atomic(path, obj):
    """原子写：tmp + rename（对齐 fs_utils::write_json 语义）"""
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    tmp = "%s.tmp.%d" % (path, os.getpid())
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(obj, f, ensure_ascii=False, indent=2)
    os.replace(tmp, path)


def now_ts() -> str:
    return datetime.datetime.now().strftime("%Y-%m-%d %H:%M:%S")


def today_str() -> str:
    return datetime.date.today().strftime("%Y-%m-%d")


# ── 账号池 ──────────────────────────────────────────────────────────────────

def load_pool() -> dict:
    return load_json(pool_path(), {"accounts": []})


def save_pool(pool: dict) -> None:
    write_json_atomic(pool_path(), pool)


def account_id_of(token: str) -> str:
    """账号 id = wb- + sha256(token) 前 12 位（同 token 稳定同 id，F-04）"""
    return "wb-" + hashlib.sha256(token.encode("utf-8")).hexdigest()[:12]


# ── 双源凭证（F-10）────────────────────────────────────────────────────────

def read_auth_file() -> dict:
    """读桌面 auth 文件（宽容解析嵌套形态）；不存在返回 {}"""
    if not os.path.exists(auth_file_path()):
        return {}
    return load_json(auth_file_path(), {})


def _dig(v, key, depth=0):
    """递归键查找（对齐 Rust dig：信封键逐层下钻 + 数组展开，限深 8）"""
    if depth > 8 or not isinstance(v, (dict, list)):
        return None
    if isinstance(v, list):
        for item in v:
            hit = _dig(item, key, depth + 1)
            if hit is not None:
                return hit
        return None
    if key in v:
        return v[key]
    for wk in ("data", "result", "resp", "response", "info"):
        if wk in v:
            hit = _dig(v[wk], key, depth + 1)
            if hit is not None:
                return hit
    return None


def dig(v, *keys):
    """按顺序查找任一键，返回第一个命中"""
    for k in keys:
        hit = _dig(v, k)
        if hit is not None:
            return hit
    return None


def creds_of(source: dict) -> dict:
    """从 auth 文件 / token store 记录中提取凭证字段（兼容多种嵌套形态，F-04）。
    返回 {access_token, refresh_token, expires_at_ms, refresh_expires_at_ms, uid, domain, nickname, edition}
    """
    if not isinstance(source, dict) or not source:
        return {}

    def _s(v):
        return v if isinstance(v, str) else None

    def _i(v):
        try:
            return int(v)
        except (TypeError, ValueError):
            return None

    auth = source.get("auth") if isinstance(source.get("auth"), dict) else source
    account = source.get("account") if isinstance(source.get("account"), dict) else source
    return {
        "access_token": _s(dig(auth, "accessToken", "access_token", "token")),
        "refresh_token": _s(dig(auth, "refreshToken", "refresh_token")),
        "expires_at_ms": _i(dig(auth, "expiresAtMs", "expires_at_ms", "expiresAt",
                                "expires_in_ms", "accessTokenExpiresAtMs")),
        "refresh_expires_at_ms": _i(dig(auth, "refreshExpiresAtMs", "refresh_expires_at_ms",
                                        "refreshExpiresAt")),
        "uid": _s(dig(account, "uid", "userId", "user_id", "id")),
        "domain": _s(dig(source, "domain")),
        "nickname": _s(dig(account, "nickname", "nickname", "name", "displayName")),
        "edition": _s(dig(account, "editionType", "edition_type", "edition")),
    }


def effective_creds(acct: dict) -> dict:
    """生效凭证 = auth 文件与 token store 中 expiresAtMs 更晚者（F-10 谁新用谁）。
    acct: 账号池记录（含 id/uid）；返回 creds dict（可能为空 = 无可用凭证）"""
    store = load_json(token_store_path(), {})
    recs = store.get("tokens", {}) if isinstance(store, dict) else {}
    store_rec = recs.get(acct.get("id", "")) or {}
    store_creds = creds_of(store_rec)
    # auth 文件仅当其 uid 与账号匹配时参与双源比较（桌面当前登录态）
    file_creds = creds_of(read_auth_file())
    if acct.get("uid") and file_creds.get("uid") and file_creds["uid"] != acct["uid"]:
        file_creds = {}
    a, b = store_creds.get("expires_at_ms"), file_creds.get("expires_at_ms")
    if file_creds.get("access_token") and (not a or (b and b >= a)):
        return file_creds
    return store_creds


def save_token_store(id_: str, creds: dict) -> None:
    """写工具侧凭证副本（version 字段拒绝未知格式）"""
    store = load_json(token_store_path(), {})
    if not isinstance(store, dict):
        store = {}
    if store.get("version") not in (None, 1):
        raise RuntimeError("token_store 版本不识别，拒绝写入")
    store["version"] = 1
    tokens = store.setdefault("tokens", {})
    rec = tokens.get(id_, {})
    rec.update(creds)
    rec["updated_at"] = now_ts()
    tokens[id_] = rec
    write_json_atomic(token_store_path(), store)


# ── 统一请求头（§5.3）───────────────────────────────────────────────────────

def build_auth_headers(creds: dict, web_platform: bool = False) -> dict:
    """统一认证头：Bearer + X-User-Id（缺省 X-No-* 占位）。
    web_platform=True 时附加 X-Client-Platform: web（积分三件套必需）"""
    h = {
        "Authorization": "Bearer " + (creds.get("access_token") or ""),
        "User-Agent": "WorkBuddy",
        "Content-Type": "application/json",
    }
    uid = creds.get("uid")
    if uid:
        h["X-User-Id"] = uid
    else:
        h["X-No-User-Id"] = "1"
    if web_platform:
        h["X-Client-Platform"] = "web"
    return h


def post_json(url, headers, body=None, timeout=30):
    """POST JSON，返回 (http_status, parsed_or_None, raw_text)；HTTPError 也返回状态码"""
    data = json.dumps(body if body is not None else {}).encode("utf-8")
    req = urllib.request.Request(url, data=data, headers=headers, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            raw = resp.read().decode("utf-8", "replace")
            return resp.status, _try_json(raw), raw
    except urllib.error.HTTPError as e:
        raw = ""
        try:
            raw = e.read().decode("utf-8", "replace")
        except Exception:
            pass
        return e.code, _try_json(raw), raw
    except Exception as e:  # 网络异常：0 表示不可达
        return 0, None, str(e)


def get_json(url, headers, timeout=30):
    """GET 请求，返回 (http_status, parsed_or_None, raw_text)；HTTPError 也返回状态码（F-17 成长中心等 GET 端点）"""
    req = urllib.request.Request(url, headers=headers, method="GET")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            raw = resp.read().decode("utf-8", "replace")
            return resp.status, _try_json(raw), raw
    except urllib.error.HTTPError as e:
        raw = ""
        try:
            raw = e.read().decode("utf-8", "replace")
        except Exception:
            pass
        return e.code, _try_json(raw), raw
    except Exception as e:  # 网络异常：0 表示不可达
        return 0, None, str(e)


def _try_json(raw):
    try:
        return json.loads(raw)
    except Exception:
        return None


# ── token 刷新（F-09）──────────────────────────────────────────────────────

REFRESH_URL = "https://www.codebuddy.cn/v2/plugin/auth/token/refresh"


# ── 区域路由（T4.5/F-36，§5.2 域名路由规则）────────────────────────────────
# CN：billing/积分 + 活动接口走 www.codebuddy.cn；Global（domain 含 .workbuddy.ai）：
# 全走 www.workbuddy.ai。plugin 网关（token refresh）固定 codebuddy.cn 不随区域。

BILLING_BASE_CN = "https://www.codebuddy.cn"
BILLING_BASE_GLOBAL = "https://www.workbuddy.ai"


def region_billing_base(domain):
    """按账号 token domain 字段返回 billing/activity 域名"""
    return BILLING_BASE_GLOBAL if ".workbuddy.ai" in (domain or "") else BILLING_BASE_CN


def is_global_region(domain):
    return ".workbuddy.ai" in (domain or "")


def billing_bases(domain):
    """域名双探测（§2.2 接口稳定性）：主域名在前、备用域名在后。
    腾讯可能整体迁移 API 域名（codebuddy.cn ↔ workbuddy.ai），
    主域名网络不可达时调用方应依次尝试备用域名。"""
    main = region_billing_base(domain)
    alt = BILLING_BASE_GLOBAL if main == BILLING_BASE_CN else BILLING_BASE_CN
    return [main, alt]


# ── 本地 quota 端口发现兜底（T5.8/F-21）────────────────────────────────────
# 云端 billing 全链失败时的最后兜底：WorkBuddy/CodeBuddy 桌面端本地服务会
# 在 127.0.0.1 暴露 quota 查询端点。发现顺序：
# ① 扫 ~/.workbuddy/*.port 文件（服务启动时落盘的端口声明）；
# ② 固定候选端口 + 有界端口段探测；
# ③ GET /api/v1/quota，按响应含 remaining/credits/quota/balance 特征确认。
# 红线：单次单发不重试、每端口 0.8s 超时、候选总数有界（最坏 ~15s）。

_QUOTA_PATH = "/api/v1/quota"
_QUOTA_PORT_CANDIDATES = [18789, 11101, 8890, 8899]
_QUOTA_PORT_RANGE = range(18780, 18796)
_QUOTA_KEYS = ("remaining", "credits", "quota", "balance")


def _quota_looks_valid(v, depth=0):
    """响应含 remaining/credits/quota/balance 任一键即认定 quota 端点（浅层宽容）"""
    if depth > 3 or not isinstance(v, dict):
        return False
    for k, val in v.items():
        if isinstance(k, str) and k.lower() in _QUOTA_KEYS:
            return True
        if _quota_looks_valid(val, depth + 1):
            return True
    return False


def _quota_probe_port(port, timeout=0.8):
    """单端口 quota 探测；命中返回响应 dict，未命中/不可达返回 None"""
    import urllib.request
    try:
        req = urllib.request.Request(
            "http://127.0.0.1:%s%s" % (port, _QUOTA_PATH),
            headers={"User-Agent": "WorkBuddy", "Accept": "application/json"},
        )
        with urllib.request.urlopen(req, timeout=timeout) as r:
            raw = r.read(65536).decode("utf-8", "replace")
        v = _try_json(raw)
        if _quota_looks_valid(v):
            return v
    except Exception:
        pass
    return None


def discover_local_quota_services(limit=3):
    """发现本机 quota 服务：*.port 声明端口 → 固定候选 → 有界端口段。
    返回 [(port, quota_dict), ...]（已确认可用的端口，按发现序）"""
    found = []
    seen = set()

    def try_port(p):
        if p in seen or not (0 < p < 65536):
            return
        seen.add(p)
        if len(found) >= limit:
            return
        v = _quota_probe_port(p)
        if v is not None:
            found.append((p, v))

    # ① ~/.workbuddy/*.port
    import glob
    import os
    for f in sorted(glob.glob(os.path.join(os.path.expanduser("~"), ".workbuddy", "*.port")))[:16]:
        try:
            with open(f, "r", encoding="utf-8", errors="replace") as fh:
                txt = fh.read().strip()
            port = int(txt.split()[0])
        except Exception:
            continue
        try_port(port)
        if len(found) >= limit:
            return found
    # ② 固定候选 + ③ 有界端口段
    for p in list(_QUOTA_PORT_CANDIDATES) + list(_QUOTA_PORT_RANGE):
        try_port(p)
        if len(found) >= limit:
            return found
    return found


def local_quota_balance():
    """本地 quota 兜底余额：首个可用端点的 remaining 求和；无可用端点返回 None"""
    for _port, v in discover_local_quota_services(limit=2):
        total = dig(v, "remaining", "RemainingCapacity", "credits", "balance")
        num = _dig_num(total)
        if num is not None:
            return num
    return None


def _dig_num(v):
    """宽容取数：递归摘出首个数值（含数字字符串）"""
    if isinstance(v, bool):
        return None
    if isinstance(v, (int, float)):
        return float(v)
    if isinstance(v, str):
        try:
            return float(v.strip())
        except Exception:
            return None
    if isinstance(v, dict):
        for val in v.values():
            n = _dig_num(val)
            if n is not None:
                return n
    if isinstance(v, list):
        for val in v:
            n = _dig_num(val)
            if n is not None:
                return n
    return None


def refresh_token_once(creds: dict):
    """调 plugin refresh 端点（X-Refresh-Token 仅允许出现在此端点）。
    返回新 creds dict 或 None（失败原因可从返回 None 后由调用方按 401 判定）"""
    if not creds.get("refresh_token"):
        return None
    h = build_auth_headers(creds)
    h["X-Refresh-Token"] = creds["refresh_token"]
    h["X-Auth-Refresh-Source"] = "workbuddy"
    status, body, _ = post_json(REFRESH_URL, h, {})
    if status != 200 or not isinstance(body, dict):
        return None
    acc = dig(body, "accessToken")
    ref = dig(body, "refreshToken")
    exp_in = dig(body, "expiresIn")
    ref_exp_in = dig(body, "refreshExpiresIn")
    if not acc:
        return None
    now_ms = int(datetime.datetime.now().timestamp() * 1000)
    out = dict(creds)
    out["access_token"] = acc
    if ref:
        out["refresh_token"] = ref
    try:
        out["expires_at_ms"] = now_ms + int(exp_in) * 1000 if exp_in else None
    except (TypeError, ValueError):
        out["expires_at_ms"] = None
    try:
        out["refresh_expires_at_ms"] = now_ms + int(ref_exp_in) * 1000 if ref_exp_in else None
    except (TypeError, ValueError):
        out["refresh_expires_at_ms"] = None
    return out


def ensure_fresh(acct: dict, lazy_hours: int = 24) -> tuple:
    """惰性刷新（F-09/F-55）：距过期 < lazy_hours 才刷；一次调用最多一次刷新。
    返回 (creds, refreshed: bool, note: str)"""
    creds = effective_creds(acct)
    if not creds.get("access_token"):
        return creds, False, "no_credential"
    exp = creds.get("expires_at_ms")
    if exp:
        remain_h = (exp - int(datetime.datetime.now().timestamp() * 1000)) / 3600000.0
        if remain_h > lazy_hours:
            return creds, False, "fresh"
        if remain_h < 0 and not creds.get("refresh_token"):
            return creds, False, "expired_needs_relogin"
    new = refresh_token_once(creds)
    if new:
        save_token_store(acct.get("id", ""), new)
        return new, True, "refreshed"
    return creds, False, "refresh_failed"
