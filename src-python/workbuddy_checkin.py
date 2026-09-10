#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""WorkBuddy 一键签到（F-15/F-16/F-55）+ token 每周兜底续期（F-09）。

用法：
  python workbuddy_checkin.py --json-stream [--uid <id> ...] [--skip-checked] [--skip-expired]
  python workbuddy_checkin.py --renew-only          # schtasks 每周兜底：惰性刷新全部账号凭证

NDJSON 输出（wb-checkin-progress 事件管线）：
  {"type":"start","total":N}
  {"type":"account","index":i,"user_id":..,"name":..,"status":"success|already|fail","message":..}
  {"type":"done","ok":n,"already":n,"failed":n}

红线：全程零 token 输出（凭证不入日志/NDJSON）。
"""

import argparse
import datetime
import json
import sys

import wb_common as wb

BASE = "https://www.codebuddy.cn"
CHECKIN_STATUS_URL = BASE + "/v2/billing/meter/checkin-activity-status"
CHECKIN_STATUS_URL_OLD = BASE + "/v2/billing/meter/checkin-status"
CHECKIN_DO_URL = BASE + "/v2/billing/meter/daily-checkin"


def emit(obj):
    sys.stdout.write(json.dumps(obj, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def checkin_status(headers):
    """查询今日是否已签：新路径回退旧路径；返回 (today_checked_in|None, status_ok: bool)"""
    for url in (CHECKIN_STATUS_URL, CHECKIN_STATUS_URL_OLD):
        status, body, _ = wb.post_json(url, headers, {})
        if status == 401:
            return None, False
        if status == 200 and isinstance(body, dict):
            v = wb.dig(body, "today_checked_in", "todayCheckedIn", "checked_in", "checkedIn")
            if v is None:
                # 部分响应以 0/1 表达
                v = wb.dig(body, "checked", "is_checked")
            if v is not None:
                return bool(v), True
            return None, True  # 200 但字段缺失：视为未知但不失败（执行时容错）
        # 其它状态码尝试旧路径
    return None, False


def checkin_do(headers):
    """执行签到。返回 (kind, message)：success / already / fail"""
    status, body, raw = wb.post_json(CHECKIN_DO_URL, headers, {})
    if status == 401:
        return "auth", "登录态失效（401）"
    code = None
    message = None
    if isinstance(body, dict):
        code = body.get("code", wb.dig(body, "code"))
        message = wb.dig(body, "message", "msg")
    if status in (0,):
        return "fail", "网络不可达: %s" % raw[:120]
    if status in (200, 201) or (isinstance(code, int) and code in (0, 200)):
        # 奖励数额以接口返回为准，不硬编码
        reward = wb.dig(body, "reward", "credits", "points", "amount")
        msg = "签到成功"
        if reward is not None:
            msg += " +%s" % reward
        return "success", msg
    # 已签容错（F-15）：code:10001 / message 含「已签到」/「repeat」
    if (isinstance(code, int) and code == 10001) or \
       (message and any(k in str(message).lower() for k in ("已签到", "repeat", "already"))):
        return "already", "今日已签到"
    return "fail", "%s（code=%s）" % (message or raw[:120] or "HTTP %s" % status, code)


def process_account(acct, skip_checked, skip_expired, lazy_hours):
    """处理单账号签到（含 401 刷新一次重试）。返回 account 事件 dict"""
    aid = acct.get("id", "")
    name = acct.get("nickname") or acct.get("uid", "")[:8] or aid
    base_ev = {"user_id": aid, "name": name}
    if skip_expired and acct.get("needs_relogin"):
        return {**base_ev, "status": "fail", "message": "需重新登录，已跳过"}
    creds, refreshed, note = wb.ensure_fresh(acct, lazy_hours)
    if not creds.get("access_token"):
        return {**base_ev, "status": "fail", "message": "无可用凭证（%s）" % note}
    if refreshed:
        _sync_pool_expiry(aid, creds)
    headers = wb.build_auth_headers(creds)
    checked, ok = checkin_status(headers)
    if ok and checked is True:
        return {**base_ev, "status": "already", "message": "今日已签到"}
    kind, message = checkin_do(headers)
    if kind == "auth":
        # 401：刷新一次仅重试失败分支（禁止二次刷新，F-09）
        new = wb.refresh_token_once(creds)
        if new:
            wb.save_token_store(aid, new)
            _sync_pool_expiry(aid, new)
            kind, message = checkin_do(wb.build_auth_headers(new))
        else:
            kind, message = "fail", "登录态失效且刷新失败，需重新登录"
    ev = {**base_ev, "status": {"success": "success", "already": "already"}.get(kind, "fail"),
          "message": message}
    if kind == "success" and note == "refreshed":
        ev["message"] = message + "（凭证已续期）"
    return ev


def _sync_pool_expiry(aid, creds):
    """刷新成功后回写账号池 token 过期时间（调度/到期日历数据源）"""
    pool = wb.load_pool()
    changed = False
    for a in pool.get("accounts", []):
        if a.get("id") == aid:
            exp = creds.get("expires_at_ms")
            if exp:
                a["access_token_expires_at"] = int(exp // 1000)
            rexp = creds.get("refresh_expires_at_ms")
            if rexp:
                a["refresh_token_expires_at"] = int(rexp // 1000)
            a["needs_relogin"] = False
            a["relogin_reason"] = ""
            changed = True
    if changed:
        wb.save_pool(pool)


def append_results(events):
    """签到结果 90 天滚动存储（趋势/日志数据源，F-55/F-22）"""
    path = wb.checkin_results_path()
    data = wb.load_json(path, {"results": []})
    results = data.get("results", [])
    today = wb.today_str()
    kept = [r for r in results if r.get("date", "") >= (datetime.date.today() -
            datetime.timedelta(days=90)).strftime("%Y-%m-%d")]
    for ev in events:
        kept.append({
            "date": today, "time": wb.now_ts(), "user_id": ev.get("user_id", ""),
            "name": ev.get("name", ""), "status": ev.get("status", ""),
            "message": ev.get("message", ""),
        })
    data["results"] = kept
    wb.write_json_atomic(path, data)


def run_renew_only(lazy_hours, keepalive_days):
    """每周兜底（schtasks）：对全部带 refreshToken 的账号执行惰性刷新。
    keepalive_days<=0 = 每天无条件刷新全部（F-55）；过期前主动刷新（v1.2）。
    输出末行 JSON 摘要供日志留存。"""
    pool = wb.load_pool()
    summary = {"mode": "renew", "finished_at": wb.now_ts(), "accounts": []}
    for acct in pool.get("accounts", []):
        if not acct.get("id"):
            continue
        # refresh_token 过期判断：有 refreshToken 才值得刷
        creds, refreshed, note = wb.ensure_fresh(acct, lazy_hours)
        item = {"user_id": acct.get("id", ""), "refreshed": refreshed, "note": note}
        if refreshed:
            _sync_pool_expiry(acct.get("id", ""), creds)
        if not creds.get("access_token"):
            item["note"] = "no_credential"
        summary["accounts"].append(item)
    # 零 token 输出
    sys.stdout.write(json.dumps(summary, ensure_ascii=False) + "\n")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--json-stream", action="store_true")
    ap.add_argument("--renew-only", action="store_true")
    ap.add_argument("--uid", action="append", default=None)
    ap.add_argument("--skip-checked", action="store_true")
    ap.add_argument("--skip-expired", action="store_true")
    ap.add_argument("--lazy-hours", type=int, default=24)
    ap.add_argument("--keepalive-days", type=int, default=0)
    args = ap.parse_args()

    if args.renew_only:
        run_renew_only(args.lazy_hours, args.keepalive_days)
        return

    pool = wb.load_pool()
    accounts = pool.get("accounts", [])
    if args.uid:
        sel = set(args.uid)
        accounts = [a for a in accounts if a.get("id") in sel]
    emit({"type": "start", "total": len(accounts)})

    events = []
    for i, acct in enumerate(accounts, 1):
        try:
            ev = process_account(acct, args.skip_checked, args.skip_expired, args.lazy_hours)
        except Exception as e:  # 单账号异常不中断整轮
            ev = {"user_id": acct.get("id", ""), "name": acct.get("nickname") or "",
                  "status": "fail", "message": "异常: %s" % e}
        ev["index"] = i
        emit(ev)
        events.append(ev)

    ok = sum(1 for e in events if e["status"] == "success")
    already = sum(1 for e in events if e["status"] == "already")
    failed = len(events) - ok - already
    emit({"type": "done", "ok": ok, "already": already, "failed": failed})
    append_results(events)


if __name__ == "__main__":
    main()
