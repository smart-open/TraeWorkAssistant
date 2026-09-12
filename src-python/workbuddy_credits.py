#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""WorkBuddy 积分三件套查询（F-20/F-22）+ 旧接口回退 + ≥5min 缓存。

用法：
  python workbuddy_credits.py [--uid <id>] [--fresh]

输出（stdout 末行 JSON，Rust 捕获）：
  {"ok":true,"fetched_at":"...","accounts":[{...}],...}
  单账号：{"user_id","name","balance","packages":[{name,remaining,total,used,
    end_time("YYYY-MM-DD HH:MM:SS" 统一形态，由 expire_ts 归一生成),
    expire_ts(Unix 秒),expire_soon}],"source"}
  全池：{"accounts":[...],"total_balance":N}
缓存：data/workbuddy_credits_cache.json（≥5 分钟；--fresh 强制刷新）。

取数三层降级（§3.6）：云端三件套 → （本地 quota API 兜底属客户端运行时，批次2）→ 无凭证明示。
解析宽容：dig() 6 种嵌套 + 容量字段链 CycleCapacitySizePrecise→CycleTotalCapacity→CapacitySize。
"""

import argparse
import datetime
import json
import sys
import time

import wb_common as wb

CN_BILLING = "https://www.codebuddy.cn"
# 旧接口参数（F-20 回退路径）
OLD_BODY = {"ProductCode": "p_tcaca", "Status": [0, 3],
            "PackageEndTimeRange": {"StartTime": "2000-01-01T00:00:00Z",
                                    "EndTime": "2099-12-31T23:59:59Z"}}
CACHE_TTL = 5 * 60  # ≥5min 缓存（频控红线）


def _num(v):
    if isinstance(v, bool):
        return None
    if isinstance(v, (int, float)):
        return v
    if isinstance(v, str):
        try:
            return float(v)
        except ValueError:
            return None
    return None


def _to_ts(v):
    """到期字段宽容归一 → Unix 秒（解析失败 None）。
    兼容：ISO 字符串（含 Z / 空格分隔 / 日期分隔符变体）、纯日期、
    数字或数字字符串时间戳（毫秒 ≥1e12 自动除 1000）。"""
    if v is None or isinstance(v, bool):
        return None
    # 数字时间戳（毫秒/秒自适应）
    n = _num(v)
    if n is not None:
        if n >= 1e12:  # 毫秒
            return int(n / 1000)
        if n >= 1e9:   # 秒
            return int(n)
        return None    # 过小（如剩余量）不是时间戳
    if not isinstance(v, str) or not v.strip():
        return None
    s = v.strip().replace("Z", "+00:00").replace("/", "-")
    # 纯日期 YYYY-MM-DD → 当天 23:59:59（本地时区），避免凌晨 0 点误报"已过期"
    if len(s) == 10 and s.count("-") == 2:
        s += " 23:59:59"
    try:
        dt = datetime.datetime.fromisoformat(s)
        if dt.tzinfo is None:
            dt = dt.replace(tzinfo=datetime.timezone(datetime.timedelta(hours=8)))
        return int(dt.timestamp())
    except ValueError:
        return None


def _fmt_ts(ts):
    """Unix 秒 → 统一展示形态 "YYYY-MM-DD HH:MM:SS"（UTC+8，与 _to_ts
    无时区输入的假定一致；前端明细按 end_time[:10] 截日期）"""
    if ts is None:
        return None
    dt = datetime.datetime.fromtimestamp(int(ts), datetime.timezone(datetime.timedelta(hours=8)))
    return dt.strftime("%Y-%m-%d %H:%M:%S")


# 兼容旧名（外部可能引用）
_iso_to_ts = _to_ts


def _total_of(pkg):
    """总量：CycleTotalCapacity → CapacitySize → TotalCapacity（§5.4 字段链）"""
    for k in ("CycleTotalCapacity", "CapacitySize", "TotalCapacity", "capacity"):
        v = _num(wb.dig(pkg, k))
        if v is not None:
            return v
    return None


def _remaining_of(pkg, total):
    """剩余：CycleCapacitySizePrecise（本周期剩余精确容量）优先，
    回退 RemainingCapacity/remaining；仅剩 used 时由总量倒推"""
    for k in ("CycleCapacitySizePrecise", "RemainingCapacity", "remaining", "Remaining", "LeftCapacity"):
        v = _num(wb.dig(pkg, k))
        if v is not None:
            return v
    used = _num(wb.dig(pkg, "UsedCapacity", "used", "Used"))
    if used is not None and total is not None:
        return max(0.0, total - used)
    return None


CAPACITY_KEYS = ("CycleCapacitySizePrecise", "RemainingCapacity", "Remaining",
                 "CycleTotalCapacity", "CapacitySize", "TotalCapacity", "UsedCapacity")


def _walk_capable_dicts(v, depth=0):
    """全树（限深 8）收集含任意容量字段的 dict——天然兼容
    data.Accounts / data.data.Accounts / data.Response.Data.Accounts 等 6 种嵌套路径（F-20）"""
    if depth > 8:
        return []
    if isinstance(v, dict):
        hits = []
        if any(k in v for k in CAPACITY_KEYS):
            hits.append(v)
        for child in v.values():
            hits.extend(_walk_capable_dicts(child, depth + 1))
        return hits
    if isinstance(v, list):
        out = []
        for item in v:
            out.extend(_walk_capable_dicts(item, depth + 1))
        return out
    return []


def _packages_from(body):
    """从三件套/旧接口响应提取包列表（容量键全树收集，6 种嵌套路径兼容）"""
    out = []
    for item in _walk_capable_dicts(body):
        total = _total_of(item)
        remaining = _remaining_of(item, total)
        if remaining is None and total is None:
            continue
        name = wb.dig(item, "PackageName", "packageName", "Name", "name",
                      "ProductCode", "description")
        # 到期字段宽容链（§3.6）：DeductionEndTime → PackageEndTime → EndTime
        # → expireTime/expireAt/expire_time/expiredAt/ExpiredTime 等别名，
        # 经 _to_ts 归一为 Unix 秒（秒/毫秒时间戳、ISO/纯日期字符串均可）。
        end_raw = wb.dig(item, "DeductionEndTime", "deductionEndTime",
                         "PackageEndTime", "packageEndTime",
                         "EndTime", "endTime", "expireTime", "expireAt",
                         "ExpireTime", "expire_time", "expiredAt", "ExpiredTime")
        end_ts = _to_ts(end_raw)
        used = _num(wb.dig(item, "UsedCapacity", "used", "Used"))
        # 到期双字段（与到期日历同口径）：
        #   expire_ts —— Unix 秒（BuddyCredits 到期日历直接使用）
        #   end_time  —— 统一形态 "YYYY-MM-DD HH:MM:SS"，由 expire_ts 生成，
        #                保证 expire_ts 有值 ⟺ end_time 有值（BuddyAccounts
        #                积分包明细读此字段）。此前 end_time 仅在原始值为字符串时
        #                回填，上游返回数字（毫秒/秒）时间戳时明细恒显示
        #                「到期时间未知」而日历正常——本行修复该不一致；
        #                时间戳解析失败时两者同时为 None（明细显示「未知」，
        #                日历过滤），与日历侧口径严格对齐。
        out.append({
            "name": str(name) if name else "积分包",
            "remaining": remaining or 0.0,
            "total": total or 0.0,
            "used": used or 0.0,
            "end_time": _fmt_ts(end_ts),
            "expire_ts": end_ts,
        })
    return out


def _balance_from_summary(body):
    """summary 响应 → 余额（remaining 求和；字段链宽容）"""
    total = _num(wb.dig(body, "RemainingCapacity", "remaining", "TotalRemaining",
                        "Balance", "balance"))
    if total is not None:
        return total
    v = wb.dig(body, "CycleCapacitySizePrecise", "CycleTotalCapacity", "CapacitySize")
    return _num(v)


def _billing_urls(domain):
    """区域路由（T4.5/F-36，§5.2）：Global 账号 billing 走 www.workbuddy.ai。
    返回 (summary, paid, free, old_resource) 四元组"""
    base = wb.region_billing_base(domain)
    return (base + "/billing/meter/get-user-resource-summary",
            base + "/billing/meter/get-user-resource-paid-packages",
            base + "/billing/meter/get-user-resource-free-packages",
            base + "/v2/billing/meter/get-user-resource")


def _fetch_round(headers, urls):
    """单轮三件套取数；返回 (pkgs, balance, net_down)——net_down=全部网络不可达"""
    pkgs = []
    balance = None
    saw_auth = False
    net_down = True
    for url in urls:
        status, body, _ = wb.post_json(url, headers, {})
        if status == 0:
            continue  # 网络不可达 → 尝试下一个（供双探测判定）
        net_down = False
        if status == 401:
            saw_auth = True
            continue
        if status == 200 and isinstance(body, dict):
            if url == urls[0]:
                balance = _balance_from_summary(body)
            else:
                pkgs.extend(_packages_from(body))
    return pkgs, balance, saw_auth, net_down


def fetch_credits_once(creds):
    """一次取数：三件套（带 web 头）→ 全 401 刷新一次仅重试失败分支 → 旧接口回退。
    主域名整体网络不可达时切备用域名重试一轮（§2.2 域名双探测，仅一次、不循环）。
    summary 只取余额不产包（其容量字段是池级汇总，入包会制造脏行）。
    返回 (packages, balance, source, new_creds|None)"""
    headers = wb.build_auth_headers(creds, web_platform=True)
    summary_url, paid_url, free_url, old_url = _billing_urls(creds.get("domain", ""))
    pkgs, balance, saw_auth, net_down = _fetch_round(headers, (summary_url, paid_url, free_url))
    if net_down and not pkgs and balance is None:
        # 双探测（§2.2）：主域名网络不可达 → 备用域名重试一轮
        alt_base = wb.billing_bases(creds.get("domain", ""))[-1]
        summary_url, paid_url, free_url, old_url = (
            alt_base + "/billing/meter/get-user-resource-summary",
            alt_base + "/billing/meter/get-user-resource-paid-packages",
            alt_base + "/billing/meter/get-user-resource-free-packages",
            alt_base + "/v2/billing/meter/get-user-resource",
        )
        pkgs, balance, saw_auth, net_down = _fetch_round(headers, (summary_url, paid_url, free_url))
    new_creds = None
    if saw_auth and not pkgs and balance is None:
        new_creds = wb.refresh_token_once(creds)
        if new_creds:
            headers = wb.build_auth_headers(new_creds, web_platform=True)
            pkgs, balance, saw_auth, _ = _fetch_round(headers, (summary_url, paid_url, free_url))
    if pkgs or balance is not None:
        return pkgs, balance, "cloud", new_creds
    # 旧接口回退（F-20）
    status, body, _ = wb.post_json(old_url, headers, OLD_BODY)
    if status == 200 and isinstance(body, dict):
        pkgs = _packages_from(body)
        if pkgs:
            return pkgs, sum(p["remaining"] for p in pkgs), "legacy", new_creds
    # 本地 quota 端口发现兜底（T5.8/F-21）：云端全链失败 → 探测本机桌面服务
    try:
        local = wb.local_quota_balance()
    except Exception:
        local = None
    if local is not None:
        return None, local, "local_quota", new_creds
    return None, None, "fetch_failed", new_creds


def fetch_account(acct):
    aid = acct.get("id", "")
    name = acct.get("nickname") or (acct.get("uid") or "")[:8] or aid
    creds = wb.effective_creds(acct)
    if not creds.get("access_token"):
        return {"user_id": aid, "name": name, "ok": False,
                "message": "无可用凭证", "balance": None, "packages": [], "source": "none"}
    pkgs, balance, source, new_creds = fetch_credits_once(creds)
    if new_creds:
        # 刷新成功 → 回写工具侧副本（F-10 谁新用谁）
        try:
            wb.save_token_store(acct.get("id", ""), new_creds)
        except Exception:
            pass
    if pkgs is None and balance is None:
        return {"user_id": aid, "name": name, "ok": False,
                "message": "积分查询失败（接口/网络）", "balance": None,
                "packages": [], "source": source}
    if balance is None:
        balance = sum(p["remaining"] for p in pkgs)
    # 按到期升序（最先到期排最前，F-56）
    pkgs.sort(key=lambda p: (p["expire_ts"] is None, p["expire_ts"] or 0))
    soon_cut = time.time() + 7 * 86400
    for p in pkgs:
        p["expire_soon"] = bool(p["expire_ts"] and p["expire_ts"] < soon_cut)
    return {"user_id": aid, "name": name, "ok": True, "balance": balance,
            "packages": pkgs, "source": source, "fetched_at": wb.now_ts()}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--uid", default=None)
    ap.add_argument("--fresh", action="store_true")
    args = ap.parse_args()

    cache = wb.load_json(wb.credits_cache_path(), {})
    cache_ok = isinstance(cache, dict) and cache.get("fetched_ts", 0) > 0
    if not args.fresh and cache_ok and (time.time() - cache["fetched_ts"]) < CACHE_TTL:
        accounts = cache.get("accounts", [])
        if args.uid:
            accounts = [a for a in accounts if a.get("user_id") == args.uid]
        sys.stdout.write(json.dumps({"ok": True, "cached": True,
                                     "accounts": accounts}, ensure_ascii=False) + "\n")
        return

    pool = wb.load_pool()
    accounts = pool.get("accounts", [])
    if args.uid:
        accounts = [a for a in accounts if a.get("id") == args.uid]

    results = []
    for acct in accounts:
        try:
            results.append(fetch_account(acct))
        except Exception as e:
            results.append({"user_id": acct.get("id", ""), "name": acct.get("nickname") or "",
                            "ok": False, "message": "异常: %s" % e, "balance": None,
                            "packages": [], "source": "error"})

    out = {"ok": True, "cached": False, "accounts": results,
           "total_balance": sum(r["balance"] or 0 for r in results if r.get("ok"))}
    # 缓存回写（池级）
    wb.write_json_atomic(wb.credits_cache_path(), {
        "fetched_ts": time.time(), "fetched_at": wb.now_ts(), "accounts": results})
    sys.stdout.write(json.dumps(out, ensure_ascii=False) + "\n")


if __name__ == "__main__":
    main()
