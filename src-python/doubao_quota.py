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
  1. 从 <data_dir>/data/doubao_accounts.json 读取该账号的会话凭证（sessionid / sid_guard）。
     凭证由代理抓包自动回写（也可在 账号管理 → 编辑 手动录入/修改）。
  2. POST 默认额度接口（请求体 {"product_line":"membership"}），附带 Cookie 头与常规浏览器 UA/Referer。
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

# 中文 Windows 管道默认 GBK：强制 stdout/stderr UTF-8，供桌面端按 UTF-8 解码
try:
    sys.stdout.reconfigure(encoding="utf-8")
    sys.stderr.reconfigure(encoding="utf-8")
except AttributeError:
    pass
import time
import urllib.error
import urllib.request
from pathlib import Path

DIG_MAX_DEPTH = 10
DEFAULT_QUOTA_URL = "https://www.doubao.com/alice/commerce/sale/subscription/quota/summary/"
HISTORY_MAX = 400  # 滚动历史上限（事件条数），覆盖 30 天巡检频率绰绰有余
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


WINDOW_TYPE_NAMES = {1: "当前时段", 2: "近 7 天"}


def _norm_ms_ts(v):
    """毫秒/秒时间戳 → 本地 datetime；无效返回 None。"""
    try:
        ts = float(v)
    except (TypeError, ValueError):
        return None
    if ts > 1e12:  # 毫秒
        ts /= 1000.0
    if not (1e8 < ts < 4e10):
        return None
    return _dt.datetime.fromtimestamp(ts)


def parse_quota(resp_json: dict) -> dict:
    """优先按代理实测结构精确解析（quota/summary，2026-09 实测），失败回退宽容 dig。"""
    data = resp_json.get("data") if isinstance(resp_json, dict) else None
    level = expire = None
    is_gift = None
    has_subscription = None
    items = []
    subscription = None

    if isinstance(data, dict):
        # ① 订阅信息：套餐名（=会员等级展示）+ 到期时间 + 是否赠送
        sub = data.get("current_subscription")
        if isinstance(sub, dict):
            display = sub.get("display") or {}
            level = (display.get("short_name") or display.get("product_name")
                     or data.get("membership_display_name"))
            expire = fmt_ts(sub.get("end_time"))
            if isinstance(sub.get("is_gift"), bool):
                is_gift = sub["is_gift"]
            has_subscription = sub.get("status") not in (None, 0)
            # 订阅记录（对齐客户端「订阅记录」页：套餐 / 周期 / 起止 / 来源 / 状态）
            start_ts = _norm_ms_ts(sub.get("start_time"))
            end_ts = _norm_ms_ts(sub.get("end_time"))
            period_days = None
            if start_ts and end_ts:
                period_days = max(1, round((end_ts - start_ts).total_seconds() / 86400))
            subscription = {
                "name": level,
                "period_days": period_days,
                "start_at": start_ts.strftime("%Y-%m-%d") if start_ts else None,
                "expire_at": expire,
                "is_gift": is_gift,
                "active": bool(has_subscription),
            }
        elif isinstance(data.get("has_active_subscription"), bool):
            has_subscription = data["has_active_subscription"]
        if not level:
            level = data.get("membership_display_name")

        # ② 额度窗口：window_limit_groups[].window_limits[]（window_type 1=当前时段 2=近7天，
        #    used_percent=已用百分比，end_time=重置时间）
        wls = data.get("window_limit_section")
        if isinstance(wls, dict):
            for group in wls.get("window_limit_groups") or []:
                if not isinstance(group, dict):
                    continue
                gname = group.get("feature_group_name") or ""
                for w in group.get("window_limits") or []:
                    if not isinstance(w, dict):
                        continue
                    wt = w.get("window_type")
                    used = w.get("used_percent")
                    reset_at = _norm_ms_ts(w.get("end_time"))
                    if used is None and not reset_at:
                        continue
                    name = WINDOW_TYPE_NAMES.get(wt) or (f"{gname}·窗口{wt}" if gname else f"窗口{wt}")
                    items.append({
                        "name": name,
                        "used_percent": used,
                        "exhausted": bool(w.get("usage_exhausted")) or (isinstance(used, (int, float)) and used >= 100),
                        "reset_at": reset_at.strftime("%Y-%m-%d %H:%M") if reset_at else None,
                    })

    # ③ 回退：结构不识别时走宽容 dig（老逻辑，保底其他端点形态）
    if level is None and expire is None and not items:
        level = pick(resp_json, LEVEL_KEYS)
        expire = fmt_ts(pick(resp_json, EXPIRE_KEYS))
        for obj in find_quota_items(resp_json)[:16]:
            name = pick(obj, NAME_KEYS)
            total = pick(obj, TOTAL_KEYS)
            if name is None or total is None:
                continue
            items.append({
                "name": str(name),
                "total": total,
                "left": pick(obj, LEFT_KEYS),
                "used": pick(obj, USED_KEYS),
            })

    return {
        "level": level,
        "expire_at": expire,
        "is_gift": is_gift,
        "has_subscription": has_subscription,
        "subscription": subscription,
        "items": items,
    }


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


def summarize_parsed(parsed: dict) -> str:
    """解析结果 → 一句话摘要（与 Rust 端 update_quota_cache 同构；窗口形态优先）。"""
    parts = []
    for it in (parsed.get("items") or [])[:4]:
        name = it.get("name") or "额度"
        if it.get("used_percent") is not None:
            pct = it["used_percent"]
            exhausted = it.get("exhausted") or (isinstance(pct, (int, float)) and pct >= 100)
            state_txt = "已用完" if exhausted else f"已用 {pct:.0f}%"
            reset = it.get("reset_at")
            if reset:
                state_txt += f"（{reset[5:]} 重置）"
            parts.append(f"{name} {state_txt}")
            continue
        if it.get("total") is not None:
            left = it.get("left")
            parts.append(f"{name} {left}/{it['total']}" if left is not None else f"{name} 总量 {it['total']}")
    return " · ".join(parts)


def append_history(data_dir: Path, event: dict) -> None:
    """追加滚动运维历史（data/doubao_health_history.json），超上限裁剪最旧事件。"""
    path = data_dir / "data" / "doubao_health_history.json"
    events = []
    if path.exists():
        try:
            events = json.loads(path.read_text(encoding="utf-8")).get("events", [])
        except Exception:  # noqa: BLE001
            events = []
    events.append(event)
    events = events[-HISTORY_MAX:]
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps({"events": events}, ensure_ascii=False, indent=1), encoding="utf-8")


def load_quota_url(data_dir: Path, override: str | None) -> str:
    """额度端点：--url > settings.doubao_quota_url > 默认值。"""
    if override:
        return override
    try:
        conf = json.loads((data_dir / "conf" / "app_settings.json").read_text(encoding="utf-8"))
        u = (conf.get("doubao_quota_url") or "").strip()
        if u.startswith("http"):
            return u
    except Exception:  # noqa: BLE001
        pass
    return DEFAULT_QUOTA_URL


def query_account(url: str, sid: str, sid_guard: str | None) -> dict:
    """调额度端点并解析；返回 {ok, parsed?, error?}。"""
    cookie = f"sessionid={sid}"
    if sid_guard:
        cookie += f"; sid_guard={sid_guard}"
    body = json.dumps({"product_line": "membership"}).encode("utf-8")
    req = urllib.request.Request(
        url,
        data=body,
        headers={
            "Cookie": cookie,
            "User-Agent": ("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
                           "(KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36"),
            "Referer": "https://www.doubao.com/",
            "Accept": "application/json, text/plain, */*",
            "Content-Type": "application/json",
        },
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=TIMEOUT) as resp:
            data = json.loads(resp.read(512 * 1024).decode("utf-8", errors="replace"))
    except urllib.error.HTTPError as e:
        return {"ok": False, "error": f"HTTP {e.code}"}
    except Exception as e:  # noqa: BLE001
        return {"ok": False, "error": f"请求失败: {e}"}
    if not isinstance(data, dict) or data.get("code") not in (0, None):
        return {"ok": False, "error": f"接口返回异常 code={data.get('code') if isinstance(data, dict) else '?'}"}
    return {"ok": True, "parsed": parse_quota(data)}


def run_batch(data_dir: Path, url: str) -> dict:
    """--all 批量巡检：遍历池内有凭证的账号 → 查额度 → 回写缓存 + 追加历史 → 汇总。"""
    pool_path = data_dir / "data" / "doubao_accounts.json"
    if not pool_path.exists():
        return {"ok": False, "error": f"账号池不存在: {pool_path}"}
    try:
        pool = json.loads(pool_path.read_text(encoding="utf-8"))
    except Exception as e:  # noqa: BLE001
        return {"ok": False, "error": f"账号池解析失败: {e}"}

    ok_n = fail_n = 0
    exhausted = []
    errors = []
    changed = False
    for acc in pool.get("accounts", []):
        uid = str(acc.get("user_id", ""))
        sid, sg = acc.get("session_id"), acc.get("sid_guard")
        if not uid or not sid:
            continue
        r = query_account(url, sid, sg)
        now = now_str()
        if r["ok"]:
            ok_n += 1
            parsed = r["parsed"]
            summary = summarize_parsed(parsed)
            acc["quota_level"] = parsed.get("level")
            acc["quota_expire_at"] = parsed.get("expire_at")
            acc["quota_summary"] = summary or None
            acc["quota_checked_at"] = now
            changed = True
            windows = [
                {"name": it.get("name"), "used_percent": it.get("used_percent"), "reset_at": it.get("reset_at")}
                for it in parsed.get("items", []) if it.get("used_percent") is not None
            ]
            if any(w.get("used_percent") is not None and (w["used_percent"] >= 100) for w in windows):
                exhausted.append({"user_id": uid, "name": acc.get("name") or uid,
                                  "reset_at": next((w.get("reset_at") for w in windows if w.get("reset_at")), None)})
            append_history(data_dir, {
                "ts": now, "kind": "quota", "uid": uid, "ok": True,
                "level": parsed.get("level"), "summary": summary, "windows": windows,
                "source": "task",
            })
        else:
            fail_n += 1
            errors.append({"user_id": uid, "error": r["error"]})
            append_history(data_dir, {"ts": now, "kind": "quota", "uid": uid, "ok": False,
                                      "summary": r["error"], "source": "task"})
    if changed:
        pool_path.write_text(json.dumps(pool, ensure_ascii=False, indent=2), encoding="utf-8")
    return {"ok": True, "mode": "all", "url": url, "total": ok_n + fail_n,
            "success": ok_n, "failed": fail_n, "exhausted": exhausted, "errors": errors,
            "finished_at": now_str()}


def main() -> int:
    ap = argparse.ArgumentParser(description="豆包会员额度查询")
    ap.add_argument("--uid", default=None, help="豆包 user_id（单账号模式；与 --all 二选一）")
    ap.add_argument("--all", action="store_true", help="批量模式：巡检池内全部有凭证账号并回写缓存")
    ap.add_argument("--url", default=None, help="会员额度接口（默认读设置 doubao_quota_url，再退回内置默认）")
    ap.add_argument("--history", action="store_true", help="单账号模式同样追加运维历史（应用内查询默认不追加，由 Rust 侧记录）")
    args = ap.parse_args()

    ddir = data_dir()
    url = load_quota_url(ddir, args.url)

    # ---- 批量模式（定时任务路径）：缓存/历史均在脚本内完成 ----
    if args.all:
        result = run_batch(ddir, url)
        print(json.dumps(result, ensure_ascii=False))
        return 0 if result.get("ok") else 1

    # ---- 单账号模式（应用内路径）：Rust 侧负责缓存回写 ----
    logs = []
    if not args.uid:
        print(json.dumps({"ok": False, "error": "需指定 --uid <user_id> 或 --all", "logs": logs},
                         ensure_ascii=False))
        return 1
    pool_path = ddir / "data" / "doubao_accounts.json"
    sid, sid_guard, err = load_credential(pool_path, args.uid)
    if err:
        logs.append(err)
        print(json.dumps({"ok": False, "error": err, "logs": logs, "finished_at": now_str()},
                         ensure_ascii=False))
        return 1
    if not sid:
        msg = "该账号未录入 sessionid 凭证（开代理自动写入，或账号管理 → 编辑 手动录入）"
        logs.append(msg)
        print(json.dumps({"ok": False, "error": msg, "logs": logs, "finished_at": now_str()},
                         ensure_ascii=False))
        return 1

    r = query_account(url, sid, sid_guard)
    if not r["ok"]:
        logs.append(r["error"])
        print(json.dumps({"ok": False, "error": r["error"], "http_status": None,
                          "logs": logs, "finished_at": now_str()}, ensure_ascii=False))
        return 1
    parsed = r["parsed"]
    if args.history:
        append_history(ddir, {
            "ts": now_str(), "kind": "quota", "uid": args.uid, "ok": True,
            "level": parsed.get("level"), "summary": summarize_parsed(parsed),
            "windows": [
                {"name": it.get("name"), "used_percent": it.get("used_percent"), "reset_at": it.get("reset_at")}
                for it in parsed.get("items", []) if it.get("used_percent") is not None
            ],
            "source": "app",
        })
    summary = {
        "ok": True,
        "http_status": 200,
        "url": url,
        "user_id": args.uid,
        "parsed": parsed,
        "finished_at": now_str(),
        "logs": logs,
    }
    print(json.dumps(summary, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
