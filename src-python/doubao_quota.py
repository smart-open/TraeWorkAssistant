#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
豆包会员额度查询脚本（P4 框架）

端点现状：豆包会员/订阅额度接口为 www.doubao.com 域下已登录 XHR，社区无公开文档，
须由用户经 MITM 抓包（项目自带 device_proxy.py）固化后填入「环境配置 → 会员额度接口」。
本脚本在该端点就绪后即插即用：

用法：
  python doubao_quota.py --uid <user_id> --url <https://...>

流程：
  1. 从 <data_dir>/data/doubao_accounts.json 读取该账号手动录入的凭证（sessionid / sid_guard）。
     凭证在豆包账号管理 → 编辑 → 会话凭证 录入（桌面客户端 cookie 为客户端级加密无法自动提取）。
  2. GET 端点，附带 Cookie 头与常规浏览器 UA/Referer。
  3. 宽容解析（dig 递归下钻，不预设响应结构）：
     - 会员等级：level / vip_level / member_level / grade / vip_type / member_type
     - 到期时间：expire_time / expire_at / due_time / valid_end_time / end_time（秒/毫秒时间戳或日期串）
     - 额度条目：数组对象同时含「名称键」与「总量键」即视为额度项（used/left 可选）
  4. stdout 最后一行输出摘要 JSON（Rust 端按此约定解析），进度与错误走 stderr。

仅查询展示，不做代刷；凭证等同密码，仅本地使用。
"""

import argparse
import datetime as _dt
import json
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

DIG_MAX_DEPTH = 10
UID_KEYS = ("user_id", "uid")
NAME_KEYS = ("name", "title", "item_name", "metric_name", "label")
TOTAL_KEYS = ("total", "limit", "quota", "max", "total_count", "total_num")
USED_KEYS = ("used", "use", "consume", "used_count")
LEFT_KEYS = ("remaining", "left", "remain", "available", "left_count", "rest")
LEVEL_KEYS = ("level", "vip_level", "member_level", "grade", "vip_type", "member_type", "plan")
EXPIRE_KEYS = (
    "expire_time", "expire_at", "expired_time", "due_time", "due_date",
    "valid_end_time", "end_time", "end_date", "subscription_expire_time",
)
TIMEOUT = 15


def now_str() -> str:
    return _dt.datetime.now().strftime("%Y-%m-%d %H:%M:%S")


def data_dir() -> Path:
    import os
    env = os.environ.get("AIWORKDATA_DIR")
    if env:
        return Path(env)
    return Path.home() / "AppData" / "Roaming" / "AIWorkAssistant"


def dig(v, keys, depth=0):
    """沿对象/数组递归查找首个命中键（宽容解析核心）。"""
    if depth > DIG_MAX_DEPTH:
        return None
    if isinstance(v, dict):
        for k in keys:
            if k in v:
                return v[k]
        for child in v.values():
            hit = dig(child, keys, depth + 1)
            if hit is not None:
                return hit
    elif isinstance(v, list):
        for item in v:
            hit = dig(item, keys, depth + 1)
            if hit is not None:
                return hit
    return None


def find_quota_items(v, depth=0, out=None):
    """收集数组中「名称键 + 总量键」同存的额度对象（常见 bundling/entitlement 结构）。"""
    if out is None:
        out = []
    if depth > DIG_MAX_DEPTH:
        return out
    if isinstance(v, dict):
        if any(k in v for k in NAME_KEYS) and any(k in v for k in TOTAL_KEYS):
            out.append(v)
        else:
            for child in v.values():
                find_quota_items(child, depth + 1, out)
    elif isinstance(v, list):
        for item in v:
            find_quota_items(item, depth + 1, out)
    return out


def fmt_ts(v):
    """时间戳/日期串归一为 'YYYY-MM-DD HH:MM'；无法解析返回 None。"""
    if v is None:
        return None
    if isinstance(v, (int, float)):
        ts = float(v)
        if ts > 1e12:  # 毫秒
            ts /= 1000.0
        if 1e8 < ts < 4e10:
            return _dt.datetime.fromtimestamp(ts).strftime("%Y-%m-%d %H:%M")
        return str(v)
    s = str(v).strip()
    for pat in ("%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M", "%Y-%m-%d", "%Y/%m/%d"):
        try:
            return _dt.datetime.strptime(s[:19], pat).strftime("%Y-%m-%d %H:%M")
        except ValueError:
            continue
    return s[:32] if s else None


def pick(v, keys):
    """dig 命中后规整为可展示标量。"""
    hit = dig(v, keys)
    if hit is None:
        return None
    if isinstance(hit, bool):
        return None
    if isinstance(hit, (int, float)):
        return hit
    s = str(hit).strip()
    return s[:64] if s else None


def parse_quota(resp_json: dict) -> dict:
    level = pick(resp_json, LEVEL_KEYS)
    expire = fmt_ts(pick(resp_json, EXPIRE_KEYS))
    items = []
    for obj in find_quota_items(resp_json)[:16]:
        name = pick(obj, NAME_KEYS)
        total = pick(obj, TOTAL_KEYS)
        if name is None or total is None:
            continue
        left = pick(obj, LEFT_KEYS)
        used = pick(obj, USED_KEYS)
        items.append({"name": str(name), "total": total, "left": left, "used": used})
    return {"level": level, "expire_at": expire, "items": items}


def raw_keys(v, prefix="", depth=0, out=None, limit=40):
    """响应顶层结构摘要（键路径列表），供端点固化时人工比对。"""
    if out is None:
        out = []
    if depth > 3 or len(out) >= limit:
        return out
    if isinstance(v, dict):
        for k, child in v.items():
            out.append(f"{prefix}{k}")
            raw_keys(child, f"{prefix}{k}.", depth + 1, out, limit)
    elif isinstance(v, list) and v:
        out.append(f"{prefix}[]({len(v)})")
        raw_keys(v[0], f"{prefix}[].", depth + 1, out, limit)
    return out


def load_credential(pool_path: Path, uid: str):
    if not pool_path.exists():
        return None, None, f"账号池不存在: {pool_path}"
    try:
        pool = json.loads(pool_path.read_text(encoding="utf-8"))
    except Exception as e:  # noqa: BLE001
        return None, None, f"账号池解析失败: {e}"
    for acc in pool.get("accounts", []):
        if str(acc.get("user_id", "")) == uid:
            return acc.get("session_id"), acc.get("sid_guard"), None
    return None, None, f"账号 {uid} 不在账号池中"


def main() -> int:
    ap = argparse.ArgumentParser(description="豆包会员额度查询")
    ap.add_argument("--uid", required=True, help="豆包 user_id")
    ap.add_argument("--url", required=True, help="会员额度接口（抓包固化）")
    args = ap.parse_args()

    logs = []
    pool_path = data_dir() / "data" / "doubao_accounts.json"
    sid, sid_guard, err = load_credential(pool_path, args.uid)
    if err:
        logs.append(err)
        print(json.dumps({"ok": False, "error": err, "logs": logs, "finished_at": now_str()},
                         ensure_ascii=False))
        return 1
    if not sid:
        msg = "该账号未录入 sessionid 凭证（账号管理 → 编辑 → 会话凭证）"
        logs.append(msg)
        print(json.dumps({"ok": False, "error": msg, "logs": logs, "finished_at": now_str()},
                         ensure_ascii=False))
        return 1

    cookie = f"sessionid={sid}"
    if sid_guard:
        cookie += f"; sid_guard={sid_guard}"
    req = urllib.request.Request(
        args.url,
        headers={
            "Cookie": cookie,
            "User-Agent": ("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
                           "(KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36"),
            "Referer": "https://www.doubao.com/",
            "Accept": "application/json, text/plain, */*",
        },
        method="GET",
    )
    try:
        with urllib.request.urlopen(req, timeout=TIMEOUT) as resp:
            status = resp.status
            body = resp.read(512 * 1024).decode("utf-8", errors="replace")
            set_cookie = resp.headers.get("Set-Cookie") or ""
    except urllib.error.HTTPError as e:
        logs.append(f"HTTP {e.code}")
        print(json.dumps({"ok": False, "error": f"HTTP {e.code}", "logs": logs, "finished_at": now_str()},
                         ensure_ascii=False))
        return 1
    except Exception as e:  # noqa: BLE001
        logs.append(f"请求失败: {e}")
        print(json.dumps({"ok": False, "error": f"请求失败: {e}", "logs": logs, "finished_at": now_str()},
                         ensure_ascii=False))
        return 1

    try:
        resp_json = json.loads(body)
    except ValueError:
        msg = "响应非 JSON（端点可能不对或返回登录页 HTML）"
        logs.append(msg)
        print(json.dumps({"ok": False, "error": msg, "http_status": status,
                          "raw_preview": body[:400], "logs": logs, "finished_at": now_str()},
                         ensure_ascii=False))
        return 1

    parsed = parse_quota(resp_json) if isinstance(resp_json, dict) else {"level": None, "expire_at": None, "items": []}
    summary = {
        "ok": True,
        "http_status": status,
        "url": args.url,
        "user_id": args.uid,
        "parsed": parsed,
        "raw_keys": raw_keys(resp_json) if isinstance(resp_json, dict) else [],
        "raw_preview": body[:2000],
        "set_cookie_refreshed": bool(set_cookie),
        "finished_at": now_str(),
        "logs": logs,
    }
    print(json.dumps(summary, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
