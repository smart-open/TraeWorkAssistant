#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""workbuddy_credits 纯函数单测（无需网络）：积分包到期解析与归一。
覆盖 §3.6 到期字段别名链（DeductionEndTime/PackageEndTime/EndTime/expireTime…）、
秒/毫秒时间戳与 ISO/纯日期字符串归一，以及 expire_ts ↔ end_time 双字段一致性
（修复：账号管理积分包明细「到期时间未知」而积分看板到期日历有值的不一致）。
可用 `python src-python/tests/test_wb_credits.py` 直接运行，或 `pytest src-python/tests/`。
"""
import datetime
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import workbuddy_credits as wc

TZ8 = datetime.timezone(datetime.timedelta(hours=8))

# 固定锚点时间（绝对时刻，与本地时区无关）：2026-05-19 12:00:00 +08:00
TS_ANCHOR = int(datetime.datetime(2026, 5, 19, 12, 0, 0, tzinfo=TZ8).timestamp())

# 三件套 paid-packages 样例：DeductionEndTime 为毫秒时间戳（此前该形态下明细
# end_time 为 None → 前端显示「到期时间未知」，日历侧 expire_ts 正常）
PAID_BODY = {
    "code": 0,
    "data": {"data": {"Packages": [
        {"PackageName": "专业版权益包", "CycleCapacitySizePrecise": 1200.5,
         "CycleTotalCapacity": 2000.0, "UsedCapacity": 799.5,
         "DeductionEndTime": TS_ANCHOR * 1000},
    ]}},
}

# 三件套 free-packages 样例：ISO 字符串（camelCase 字段）
FREE_BODY = {
    "code": 0,
    "data": {"Packages": [
        {"packageName": "免费体验包", "RemainingCapacity": 50.0,
         "CapacitySize": 100.0, "expireTime": "2026-05-19T23:59:59Z"},
    ]},
}

# 旧接口回退（F-20）样例：data.Accounts 嵌套 + PackageEndTime 无时区字符串
LEGACY_BODY = {
    "code": 0,
    "data": {"Accounts": [
        {"PackageName": "旧接口包", "CapacitySize": 500.0,
         "RemainingCapacity": 300.0, "PackageEndTime": "2026-06-30 00:00:00"},
    ]},
}

_DT_RE = re.compile(r"^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}$")


def _assert_end_time_shape(pkg):
    """日历侧口径断言：expire_ts 有值 ⟺ end_time 有值，且 end_time 为统一
    形态（前端明细按 end_time[:10] 截日期展示）。"""
    if pkg["expire_ts"] is not None:
        assert pkg["end_time"], "expire_ts 有值时 end_time 必须有值（明细不再「未知」）"
        assert _DT_RE.match(pkg["end_time"]), f"end_time 非统一形态: {pkg['end_time']!r}"
        date_part = pkg["end_time"][:10]
        expect_date = datetime.datetime.fromtimestamp(
            pkg["expire_ts"], TZ8).strftime("%Y-%m-%d")
        assert date_part == expect_date
        # 前端口径：slice(0,10).replace(/-/g,'/') 必须得到合法日期
        datetime.datetime.strptime(date_part, "%Y-%m-%d")
    else:
        assert pkg["end_time"] is None, "expire_ts 为 None 时 end_time 必须同时为 None"


def test_to_ts_variants():
    assert wc._to_ts(TS_ANCHOR * 1000) == TS_ANCHOR          # 毫秒数字
    assert wc._to_ts(str(TS_ANCHOR * 1000)) == TS_ANCHOR     # 毫秒数字字符串
    assert wc._to_ts(TS_ANCHOR) == TS_ANCHOR                 # 秒
    assert wc._to_ts("2026-05-19T04:00:00Z") == TS_ANCHOR    # ISO Z（UTC）
    assert wc._to_ts("2026-05-19 12:00:00") == TS_ANCHOR     # 无时区按 +8
    assert wc._to_ts("2026/05/19 12:00:00") == TS_ANCHOR     # 斜杠分隔
    assert wc._to_ts("2026-05-19") == int(datetime.datetime(
        2026, 5, 19, 23, 59, 59, tzinfo=TZ8).timestamp())    # 纯日期→当天末
    assert wc._to_ts("not-a-date") is None
    assert wc._to_ts(0) is None
    assert wc._to_ts(None) is None
    assert wc._to_ts(True) is None


def test_fmt_ts_fixed_anchor():
    assert wc._fmt_ts(TS_ANCHOR) == "2026-05-19 12:00:00"
    assert wc._fmt_ts(None) is None


def test_paid_packages_millis_fixture():
    pkgs = wc._packages_from(PAID_BODY)
    assert len(pkgs) == 1
    p = pkgs[0]
    assert p["name"] == "专业版权益包"
    assert p["expire_ts"] == TS_ANCHOR
    assert p["remaining"] == 1200.5 and p["total"] == 2000.0 and p["used"] == 799.5
    _assert_end_time_shape(p)


def test_free_packages_iso_fixture():
    pkgs = wc._packages_from(FREE_BODY)
    assert len(pkgs) == 1
    p = pkgs[0]
    assert p["expire_ts"] == int(datetime.datetime(
        2026, 5, 19, 23, 59, 59, tzinfo=datetime.timezone.utc).timestamp())
    # Z(UTC) → +8 展示换算为次日 07:59:59
    assert p["end_time"] == "2026-05-20 07:59:59"
    _assert_end_time_shape(p)


def test_legacy_response_fixture():
    pkgs = wc._packages_from(LEGACY_BODY)
    assert len(pkgs) == 1
    p = pkgs[0]
    assert p["expire_ts"] == int(datetime.datetime(
        2026, 6, 30, 0, 0, 0, tzinfo=TZ8).timestamp())
    assert p["end_time"] == "2026-06-30 00:00:00"
    _assert_end_time_shape(p)


def test_expiry_alias_chain():
    """§3.6 别名链逐键取到（每键最小包 dict，单键命中）"""
    for key in ("DeductionEndTime", "deductionEndTime", "PackageEndTime",
                "packageEndTime", "EndTime", "endTime", "expireTime",
                "expireAt", "ExpireTime", "expire_time", "expiredAt",
                "ExpiredTime"):
        body = {"data": {"Packages": [
            {"Name": "k", "CapacitySize": 10.0, "RemainingCapacity": 5.0,
             key: TS_ANCHOR},
        ]}}
        pkgs = wc._packages_from(body)
        assert len(pkgs) == 1, f"别名 {key} 未被解析"
        assert pkgs[0]["expire_ts"] == TS_ANCHOR, f"别名 {key} 归一失败"


def test_end_time_consistency_all_fixtures():
    """三份样例全部包：明细（end_time）与日历（expire_ts）口径严格一致"""
    for body in (PAID_BODY, FREE_BODY, LEGACY_BODY):
        for p in wc._packages_from(body):
            _assert_end_time_shape(p)


def test_numeric_string_no_longer_leaks_to_end_time():
    """此前 end_time 回填原始字符串：数字字符串时间戳会泄漏成 '1779…' 垃圾值；
    现在统一归一为 datetime 形态"""
    body = {"data": {"Packages": [
        {"Name": "n", "CapacitySize": 10.0, "RemainingCapacity": 5.0,
         "DeductionEndTime": str(TS_ANCHOR * 1000)},
    ]}}
    p = wc._packages_from(body)[0]
    assert p["expire_ts"] == TS_ANCHOR
    assert p["end_time"] == "2026-05-19 12:00:00"


def test_unparseable_expiry_is_unknown_everywhere():
    """到期值无法解析 → expire_ts 与 end_time 同时为 None：
    明细显示「未知」，日历过滤——两端口径一致"""
    body = {"data": {"Packages": [
        {"Name": "bad", "CapacitySize": 10.0, "RemainingCapacity": 5.0,
         "DeductionEndTime": "forever"},
    ]}}
    p = wc._packages_from(body)[0]
    assert p["expire_ts"] is None
    assert p["end_time"] is None


def test_no_expiry_field_package_still_listed():
    """无到期字段的包仍入列表（容量字段命中），到期两端同显未知/过滤"""
    body = {"data": {"Packages": [
        {"Name": "noend", "CapacitySize": 10.0, "RemainingCapacity": 5.0},
    ]}}
    p = wc._packages_from(body)[0]
    assert p["expire_ts"] is None and p["end_time"] is None


if __name__ == "__main__":
    test_to_ts_variants()
    test_fmt_ts_fixed_anchor()
    test_paid_packages_millis_fixture()
    test_free_packages_iso_fixture()
    test_legacy_response_fixture()
    test_expiry_alias_chain()
    test_end_time_consistency_all_fixtures()
    test_numeric_string_no_longer_leaks_to_end_time()
    test_unparseable_expiry_is_unknown_everywhere()
    test_no_expiry_field_package_still_listed()
    print("ALL TESTS PASSED")
