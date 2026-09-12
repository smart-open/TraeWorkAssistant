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
GLOBAL_BILLING = "https://www.workbuddy.ai"

# 当前账号区域基址（T4.5/F-36）：process_account/process_account_growth 入口按
# token domain 切换；单线程串行处理账号，模块级状态安全。
_CURRENT_BASE = [BASE]


def _set_region(domain):
    _CURRENT_BASE[0] = wb.region_billing_base(domain)


def _u(path):
    return _CURRENT_BASE[0] + path


def _urls_for(base):
    """指定基座下的全部端点（签到 + 成长中心，T4.5/F-36 §5.2）"""
    b = base
    g = b + "/v2/activity/growth"
    return {
        "checkin_status": b + "/v2/billing/meter/checkin-activity-status",
        "checkin_status_old": b + "/v2/billing/meter/checkin-status",
        "checkin_do": b + "/v2/billing/meter/daily-checkin",
        "travel_status": g + "/buddy/travel/status",
        "travel_claim": g + "/buddy/travel/claim",
        "travel_config": g + "/buddy/travel/config",
        "travel_depart": g + "/buddy/travel/depart",
        "lottery_chances": g + "/lottery/chances",
        "lottery_draw": g + "/lottery/draw",
        "tasks": g + "/tasks",
        "tasks_accept": g + "/tasks/accept",
        "energy": g + "/energy",
        "streak": g + "/streak",
    }


def _urls():
    """当前账号区域端点表"""
    return _urls_for(_CURRENT_BASE[0])


def _alt_urls():
    """备用域名端点表（§2.2 域名双探测）：当前主域名网络不可达时切换重试一次"""
    bases = wb.billing_bases(None)
    alt = bases[1] if bases[0] == _CURRENT_BASE[0] else bases[0]
    return _urls_for(alt)

# 盲盒抽取循环上限（防接口异常时死循环；正常 balance 会归零）
LOTTERY_MAX_DRAWS = 20


def emit(obj):
    sys.stdout.write(json.dumps(obj, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def checkin_status(headers, urls):
    """查询今日是否已签：新路径回退旧路径；返回 (today_checked_in|None, status_ok: bool)"""
    for url in (urls["checkin_status"], urls["checkin_status_old"]):
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


def checkin_do(headers, urls):
    """执行签到。返回 (kind, message, reward)：success / already / fail"""
    status, body, raw = wb.post_json(urls["checkin_do"], headers, {})
    if status == 0:
        # 域名双探测（§2.2）：主域名网络不可达 → 备用域名重试一次
        alt = _alt_urls()["checkin_do"]
        if alt != urls["checkin_do"]:
            status, body, raw = wb.post_json(alt, headers, {})
    if status == 401:
        return "auth", "登录态失效（401）", None
    code = None
    message = None
    if isinstance(body, dict):
        code = body.get("code", wb.dig(body, "code"))
        message = wb.dig(body, "message", "msg")
    if status in (0,):
        return "fail", "网络不可达: %s" % raw[:120], None
    if status in (200, 201) or (isinstance(code, int) and code in (0, 200)):
        # 奖励数额以接口返回为准，不硬编码（F-17）；键候选覆盖常见命名，
        # 兜底走签到前后余额差值（process_account）。数额走独立 reward 字段展示。
        reward = wb.dig(body, "reward", "credits", "points", "amount",
                        "integral", "score", "bonus", "reward_amount",
                        "add_integral", "addCredits", "earned")
        return "success", "签到成功", _num_or_none(reward)
    # 已签容错（F-15）：code:10001 / message 含「已签到」/「repeat」
    if (isinstance(code, int) and code == 10001) or \
       (message and any(k in str(message).lower() for k in ("已签到", "repeat", "already"))):
        return "already", "今日已签到", None
    return "fail", "%s（code=%s）" % (message or raw[:120] or "HTTP %s" % status, code), None


def _num_or_none(v):
    """奖励/余额数值归一：数字或纯数字字符串 → float，其余 None"""
    if isinstance(v, bool):
        return None
    if isinstance(v, (int, float)):
        return float(v)
    if isinstance(v, str):
        try:
            return float(v.strip())
        except ValueError:
            return None
    return None


def _scalar_of(v):
    """嵌套对象按常用数值键归一（energy/streak 兼容 {current:..} 等对象返回，
    修复前端「连签 [object Object] 天」展示）"""
    if isinstance(v, dict):
        for k in ("current", "days", "count", "value", "num", "total", "streak", "energy"):
            if v.get(k) is not None:
                return _scalar_of(v[k])
        return None
    return _num_or_none(v)


def fetch_balance(headers):
    """查询当前通用积分余额（get-user-resource-summary，与积分页同口径）。
    网络失败/解析失败返回 None——仅用于签到获得积分差值兜底，不阻塞签到。"""
    summary_url = _u("/billing/meter/get-user-resource-summary")
    status, body, _ = wb.post_json(summary_url, headers, {})
    if status == 0:
        bases = wb.billing_bases(None)
        alt_base = bases[1] if bases and bases[0] == _CURRENT_BASE[0] else (bases[0] if bases else "")
        if alt_base and alt_base != _CURRENT_BASE[0]:
            status, body, _ = wb.post_json(alt_base + "/billing/meter/get-user-resource-summary", headers, {})
    if status != 200 or not isinstance(body, dict):
        return None
    total = wb.dig(body, "RemainingCapacity", "remaining", "TotalRemaining",
                   "Balance", "balance")
    return _num_or_none(total)


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
    _set_region(creds.get("domain", ""))
    urls = _urls()
    headers = wb.build_auth_headers(creds)
    checked, ok = checkin_status(headers, urls)
    if ok and checked is True:
        return {**base_ev, "status": "already", "message": "今日已签到"}
    # 签到前余额（获得积分差值兜底数据源；查询失败不阻塞签到）
    pre_balance = fetch_balance(headers)
    kind, message, reward = checkin_do(headers, urls)
    if kind == "auth":
        # 401：刷新一次仅重试失败分支（禁止二次刷新，F-09）
        new = wb.refresh_token_once(creds)
        if new:
            wb.save_token_store(aid, new)
            _sync_pool_expiry(aid, new)
            headers = wb.build_auth_headers(new)
            kind, message, reward = checkin_do(headers, urls)
        else:
            kind, message, reward = "fail", "登录态失效且刷新失败，需重新登录", None
    # 获得积分兜底（F-17）：接口未返回奖励数额时用签到前后余额差值；
    # 仅差值>0 才采信（防并发扣减/查询时点差造成负值误报）
    if kind == "success" and reward is None and pre_balance is not None:
        post = fetch_balance(headers)
        if post is not None and post > pre_balance:
            reward = round(post - pre_balance, 2)
    ev = {**base_ev, "status": {"success": "success", "already": "already"}.get(kind, "fail"),
          "message": message}
    if reward is not None:
        ev["reward"] = reward
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
        rec = {
            "date": today, "time": wb.now_ts(), "user_id": ev.get("user_id", ""),
            "name": ev.get("name", ""), "status": ev.get("status", ""),
            "message": ev.get("message", ""),
        }
        if ev.get("reward") is not None:
            rec["reward"] = ev.get("reward")
        kept.append(rec)
    data["results"] = kept
    wb.write_json_atomic(path, data)


# ── 成长中心自动化（T2.5/F-17）────────────────────────────────────────────

def _reward_text(body, raw):
    """奖励数额一律以接口返回为准，不硬编码（F-17 红线）"""
    r = wb.dig(body, "reward", "credits", "points", "amount", "value")
    return "+%s" % r if r is not None else (raw[:60] or "ok")


def growth_travel(headers, urls):
    """Buddy 旅行：status → arrived 则 claim → config → depart（各步独立容错）"""
    status, body, raw = wb.get_json(urls["travel_status"], headers)
    if status == 401:
        return "auth", "登录态失效（401）"
    if status != 200 or not isinstance(body, dict):
        return "fail", ("travel/status 不可用（HTTP %s）" % status) if status else ("travel/status 不可用: %s" % raw[:60])
    arrived = wb.dig(body, "arrived", "is_arrived", "has_arrived")
    record_id = wb.dig(body, "record_id", "recordId")
    if not arrived:
        # 未到达：报告在途状态即可
        dest = wb.dig(body, "destination", "name", "target")
        return "skip", "旅行在途%s" % ("（%s）" % dest if dest else "")
    # 领奖
    st1, b1, r1 = wb.post_json(urls["travel_claim"], headers,
                               {"record_id": record_id} if record_id is not None else {})
    if st1 == 401:
        return "auth", "登录态失效（401）"
    claim_txt = _reward_text(b1, r1) if st1 in (200, 201) else "claim 失败（HTTP %s）" % st1
    # 查目的地配置并出发（config 失败不阻塞 depart）
    st2, b2, _ = wb.get_json(urls["travel_config"], headers)
    dest = wb.dig(b2, "destination", "name", "target") if isinstance(b2, dict) else None
    st3, b3, r3 = wb.post_json(urls["travel_depart"], headers,
                               {"destination": dest} if dest is not None else {})
    if st3 in (200, 201):
        return "ok", "领奖%s，已出发%s" % (claim_txt, "（%s）" % dest if dest else "")
    return "ok", "领奖%s；depart 失败（HTTP %s）" % (claim_txt, st3)


def growth_lottery(headers, urls):
    """盲盒：chances(balance>0) → draw 循环（可开关）"""
    status, body, raw = wb.get_json(urls["lottery_chances"], headers)
    if status == 401:
        return "auth", "登录态失效（401）"
    if status != 200 or not isinstance(body, dict):
        return "fail", ("lottery/chances 不可用（HTTP %s）" % status) if status else ("lottery/chances 不可用: %s" % raw[:60])
    balance = wb.dig(body, "balance", "chances", "count", "remain")
    try:
        balance = int(balance)
    except (TypeError, ValueError):
        return "skip", "无可用次数"
    if balance <= 0:
        return "skip", "无可用次数"
    draws, rewards = 0, []
    while balance > 0 and draws < LOTTERY_MAX_DRAWS:
        st, b, r = wb.post_json(urls["lottery_draw"], headers, {})
        if st == 401:
            return "auth", "登录态失效（401，已抽 %d 次）" % draws
        if st not in (200, 201):
            break
        draws += 1
        rewards.append(wb.dig(b, "reward", "credits", "points", "amount"))
        balance -= 1
    if draws == 0:
        return "fail", "draw 不可用"
    return "ok", "抽取 %d 次（奖励 %s）" % (draws, "/".join(str(x) for x in rewards if x is not None) or "见响应")


def growth_tasks(headers, urls):
    """任务领奖：tasks → 过滤 has_reward && 未领取 → accept {task_code}（可开关）"""
    status, body, raw = wb.get_json(urls["tasks"], headers)
    if status == 401:
        return "auth", "登录态失效（401）"
    if status != 200 or not isinstance(body, dict):
        return "fail", ("tasks 不可用（HTTP %s）" % status) if status else ("tasks 不可用: %s" % raw[:60])
    tasks = wb.dig(body, "tasks", "list", "records")
    if not isinstance(tasks, list):
        tasks = []
    claimed, skipped = 0, 0
    for t in tasks:
        if not isinstance(t, dict):
            continue
        if not t.get("has_reward") and wb.dig(t, "hasReward") is None:
            continue
        accept_status = str(t.get("accept_status", wb.dig(t, "acceptStatus", "status") or ""))
        if accept_status in ("1", "2", "claimed", "accepted", "已领取", "true", "True"):
            skipped += 1
            continue
        code = t.get("task_code", t.get("taskCode", t.get("code")))
        st, b, r = wb.post_json(urls["tasks_accept"], headers, {"task_code": code} if code is not None else {})
        if st == 401:
            return "auth", "登录态失效（401，已领 %d 项）" % claimed
        if st in (200, 201):
            claimed += 1
    if claimed == 0:
        return "skip", "无可领奖励（已领 %d 项）" % skipped
    return "ok", "领取 %d 项任务奖励" % claimed


def growth_info(headers, urls):
    """能量与连签天数（页面附注展示；对象响应归一为标量，防 [object Object]）"""
    info = {}
    st, b, _ = wb.get_json(urls["energy"], headers)
    if st == 200 and isinstance(b, dict):
        v = _scalar_of(wb.dig(b, "energy", "value", "balance", "num"))
        if v is not None:
            info["energy"] = v
    st, b, _ = wb.get_json(urls["streak"], headers)
    if st == 200 and isinstance(b, dict):
        v = _scalar_of(wb.dig(b, "streak", "days", "continuous_days", "count"))
        if v is not None:
            info["streak"] = v
    return info


def process_account_growth(acct, flags):
    """单账号成长中心链式执行（旅行→盲盒→任务，各步独立容错；401 刷新一次重试）"""
    aid = acct.get("id", "")
    name = acct.get("nickname") or acct.get("uid", "")[:8] or aid
    base_ev = {"type": "growth", "user_id": aid, "name": name}
    creds, refreshed, note = wb.ensure_fresh(acct, 24)
    if not creds.get("access_token"):
        return {**base_ev, "status": "fail", "message": "无可用凭证（%s）" % note}
    _set_region(creds.get("domain", ""))
    urls = _urls()
    headers = wb.build_auth_headers(creds)

    def with_retry(fn):
        kind, msg = fn(headers, urls)
        if kind == "auth":
            new = wb.refresh_token_once(creds)
            if new:
                wb.save_token_store(aid, new)
                _sync_pool_expiry(aid, new)
                return fn(wb.build_auth_headers(new), urls)
            return "fail", "登录态失效且刷新失败"
        return kind, msg

    result = {}
    if flags.get("travel"):
        kind, msg = with_retry(growth_travel)
        result["travel"] = "fail" if kind == "fail" else msg
    if flags.get("lottery"):
        kind, msg = with_retry(growth_lottery)
        result["lottery"] = "fail" if kind == "fail" else msg
    if flags.get("tasks"):
        kind, msg = with_retry(growth_tasks)
        result["tasks"] = "fail" if kind == "fail" else msg
    result.update(growth_info(headers, urls))
    fails = sum(1 for v in (result.get("travel"), result.get("lottery"), result.get("tasks"))
                if v == "fail")
    return {**base_ev,
            "status": "fail" if fails == len([k for k in ("travel", "lottery", "tasks") if k in result]) and fails > 0 else "ok",
            **result}


def run_growth(uids, flags):
    """成长中心整轮：NDJSON 输出（wb-checkin-progress 管线复用）"""
    pool = wb.load_pool()
    accounts = pool.get("accounts", [])
    if uids:
        sel = set(uids)
        accounts = [a for a in accounts if a.get("id") in sel]
    emit({"type": "start", "total": len(accounts), "mode": "growth"})
    for i, acct in enumerate(accounts, 1):
        try:
            ev = process_account_growth(acct, flags)
        except Exception as e:  # 单账号异常不中断整轮
            ev = {"type": "growth", "user_id": acct.get("id", ""),
                  "name": acct.get("nickname") or "", "status": "fail", "message": "异常: %s" % e}
        ev["index"] = i
        emit(ev)
    emit({"type": "done", "mode": "growth"})


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
    ap.add_argument("--growth", action="store_true", help="成长中心整轮（T2.5/F-17）")
    ap.add_argument("--growth-travel", action="store_true")
    ap.add_argument("--growth-lottery", action="store_true")
    ap.add_argument("--growth-tasks", action="store_true")
    ap.add_argument("--uid", action="append", default=None)
    ap.add_argument("--skip-checked", action="store_true")
    ap.add_argument("--skip-expired", action="store_true")
    ap.add_argument("--lazy-hours", type=int, default=24)
    ap.add_argument("--keepalive-days", type=int, default=0)
    args = ap.parse_args()

    if args.renew_only:
        run_renew_only(args.lazy_hours, args.keepalive_days)
        return

    # 成长中心模式：独立于签到流程，事件走同一 NDJSON 管线
    if args.growth:
        run_growth(args.uid, {
            "travel": args.growth_travel,
            "lottery": args.growth_lottery,
            "tasks": args.growth_tasks,
        })
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
