#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""auto_checkin 纯函数单测（无需网络）。
可用 `python src-python/tests/test_auto_checkin.py` 直接运行，或 `pytest src-python/tests/`。
"""
import base64
import json
import time
import os
import sys
import tempfile
import shutil

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import auto_checkin
from auto_checkin import (
    extract_user_id,
    get_jwt_exp,
    rand_digits,
    classify_error,
    save_cooldown,
    parse_claim_reward,
    resolve_claim_credits,
)


def _make_jwt(user_id, exp_offset_hours=72):
    header = base64.urlsafe_b64encode(json.dumps({"alg": "RS256", "typ": "JWT"}).encode()).rstrip(b"=")
    payload = base64.urlsafe_b64encode(
        json.dumps({"data": {"id": user_id}, "exp": int(time.time()) + exp_offset_hours * 3600}).encode()
    ).rstrip(b"=")
    return f"Cloud-IDE-JWT {header.decode()}.{payload.decode()}.sig"


def test_extract_user_id():
    uid = "1234567890123456"
    assert extract_user_id(_make_jwt(uid)) == uid
    # 无前缀也能解析
    assert extract_user_id(_make_jwt(uid).split(" ", 1)[1]) == uid
    # 非法 token 返回 None
    assert extract_user_id("not-a-jwt") is None


def test_get_jwt_exp():
    uid = "1234567890123456"
    exp_dt, remain = get_jwt_exp(_make_jwt(uid, 48))
    assert exp_dt is not None
    assert remain is not None
    assert 47 < remain < 49


def test_rand_digits_deterministic():
    a = rand_digits(15, seed="1234567890123456")
    b = rand_digits(15, seed="1234567890123456")
    assert a == b
    assert len(a) == 15
    assert a.isdigit()


def test_classify_error_mapping():
    """classify_error(http_status, message, code) -> (error_type, cooldown_seconds)，与实现对齐"""
    # HTTP 200 + 业务码 1005（套餐限额）→ PlanLimit 冷却 12h
    assert classify_error(200, "plan limit", 1005) == ("PlanLimit", 43200)
    # 429 限频 → SoftRate 冷却 60s
    assert classify_error(429, "too many", None) == ("SoftRate", 60)
    # 401 会话吊销 → SessionDead 永久（-1）
    assert classify_error(401, "unauthorized", None) == ("SessionDead", -1)
    # 404 → NotFound 冷却 60s
    assert classify_error(404, "not found", None) == ("NotFound", 60)
    # 5xx → Server 冷却 600s
    assert classify_error(500, "oops", None) == ("Server", 600)
    assert classify_error(599, "oops", None) == ("Server", 600)
    # 4xx（非 401/404/429）→ Client 冷却 600s
    assert classify_error(400, "bad", None) == ("Client", 600)
    assert classify_error(403, "forbidden", None) == ("Client", 600)


def test_classify_error_business_and_unknown():
    # 200 + 非零业务码（非 1005）→ BusinessError 冷却 300s
    assert classify_error(200, "biz", 1234) == ("BusinessError", 300)
    # 非 JSON 响应修复后 code=None：HTTP 200 不再被误判为业务码
    assert classify_error(200, "HTTP 200: 非 JSON 响应", None) == ("Unknown", 0)


def _read_cooldown(user_id, data_dir):
    path = os.path.join(data_dir, "account_cooldowns.json")
    with open(path, "r", encoding="utf-8") as f:
        data = json.load(f)
    return data["cooldowns"][user_id]


def test_consecutive_server_errors_cooldown():
    """连续 3 次 Server/Client 错误才进入 600s 冷却；前两次仅记 error_count（until=0）。
    save_cooldown 依赖模块级 DATA_SUBDIR 定位落盘文件，此处注入临时目录隔离。"""
    data_dir = tempfile.mkdtemp(prefix="aiw_test_cooldown_")
    orig = auto_checkin.DATA_SUBDIR
    auto_checkin.DATA_SUBDIR = data_dir
    try:
        start = time.time()
        for i in range(2):
            save_cooldown("u1", "Server", 600, f"err{i}")
            entry = _read_cooldown("u1", data_dir)
            assert entry["until"] == 0
            assert entry["error_count"] == i + 1
        # 第 3 次：进入冷却，until ≈ now + 600，error_count 归零
        save_cooldown("u1", "Server", 600, "err2")
        entry = _read_cooldown("u1", data_dir)
        assert entry["error_count"] == 0
        assert start + 590 <= entry["until"] <= start + 615
    finally:
        auto_checkin.DATA_SUBDIR = orig
        shutil.rmtree(data_dir, ignore_errors=True)


def test_client_errors_share_strike_counter_type_gate():
    """Client 错误同样走 3 次计数；但错误类型切换到非 Server/Client（如 SoftRate）时立即冷却。"""
    data_dir = tempfile.mkdtemp(prefix="aiw_test_cooldown2_")
    orig = auto_checkin.DATA_SUBDIR
    auto_checkin.DATA_SUBDIR = data_dir
    try:
        save_cooldown("u2", "Client", 600, "c1")
        assert _read_cooldown("u2", data_dir)["until"] == 0
        # 非 Server/Client 类型：不受 3 次计数保护，立即按 cooldown_seconds 冷却
        save_cooldown("u2", "SoftRate", 60, "rate")
        entry = _read_cooldown("u2", data_dir)
        assert entry["type"] == "SoftRate"
        assert entry["until"] > time.time()
    finally:
        auto_checkin.DATA_SUBDIR = orig
        shutil.rmtree(data_dir, ignore_errors=True)


def test_parse_claim_reward_fields():
    """parse_claim_reward：claim 响应奖励字段提取（data 层优先于顶层，非法值返回 None）。"""
    # data 层常见奖励字段
    assert parse_claim_reward({"code": 0, "data": {"reward": 20}}) == 20
    assert parse_claim_reward({"code": 0, "data": {"delta": 7}}) == 7
    assert parse_claim_reward({"code": 0, "data": {"credits": 30}}) == 30
    # 顶层兜底
    assert parse_claim_reward({"code": 0, "reward": 5}) == 5
    # data 层优先于顶层
    assert parse_claim_reward({"code": 0, "data": {"delta": 7}, "credits": 99}) == 7
    # 奖励类字段名优先于 credits（同名共存时 reward 先命中）
    assert parse_claim_reward({"code": 0, "data": {"reward": 8, "credits": 500}}) == 8
    # 整值 float / 数字串可接受
    assert parse_claim_reward({"code": 0, "data": {"reward": 2.0}}) == 2
    assert parse_claim_reward({"code": 0, "data": {"reward": "15"}}) == 15
    # 非法值：bool / 负数 / 非数字串 / 缺失 / 非 dict
    assert parse_claim_reward({"code": 0, "data": {"reward": True}}) is None
    assert parse_claim_reward({"code": 0, "data": {"reward": -5}}) is None
    assert parse_claim_reward({"code": 0, "data": {"reward": "abc"}}) is None
    assert parse_claim_reward({"code": 0, "data": {}}) is None
    assert parse_claim_reward(None) is None
    assert parse_claim_reward({"code": 0, "message": "ok"}) is None


def _no_recheck():
    raise AssertionError("不应触发 status 复查")


def test_resolve_claim_credits_layer1_claim_reward():
    """层1：claim 带奖励字段 → delta 取奖励值，credits 沿用签到前余额，不做复查。"""
    assert resolve_claim_credits({"code": 0, "data": {"reward": 20}}, 500, recheck=_no_recheck) == (
        500, 20, "claim_reward")
    # credits_before 缺失时奖励仍生效，余额记 None
    assert resolve_claim_credits({"code": 0, "data": {"reward": 20}}, None, recheck=_no_recheck) == (
        None, 20, "claim_reward")


def test_resolve_claim_credits_layer2_balance_diff():
    """层2：claim 无奖励字段 → 复查一次 status，delta = 复查余额 - 签到前余额。"""
    calls = []

    def recheck():
        calls.append(1)
        return True, False, 520, 0, "ok"

    assert resolve_claim_credits({"code": 0, "message": "ok"}, 500, recheck=recheck) == (
        520, 20, "balance_diff")
    assert len(calls) == 1
    # delta 为 0 也视为有效（非负），credits 取复查余额
    assert resolve_claim_credits({}, 500, recheck=lambda: (True, None, 500, 0, "ok")) == (
        500, 0, "balance_diff")


def test_resolve_claim_credits_layer3_fallback():
    """层3：复查失败 / 负差值 / 解析失败 → 回退旧行为（delta = credits_before）。"""
    # 复查网络失败
    assert resolve_claim_credits({}, 500, recheck=lambda: (False, None, None, -1, "net err")) == (
        500, 500, "legacy")
    # 复查抛异常（可容忍，不向外传播）
    def _boom():
        raise RuntimeError("boom")
    assert resolve_claim_credits({}, 500, recheck=_boom) == (500, 500, "legacy")
    # 复查余额非整数
    assert resolve_claim_credits({}, 500, recheck=lambda: (True, None, None, 0, "ok")) == (
        500, 500, "legacy")
    # 差值为负 → 语义假设不成立，回退旧行为
    assert resolve_claim_credits({}, 500, recheck=lambda: (True, None, 480, 0, "ok")) == (
        500, 500, "legacy")
    # credits_before 缺失 → 不发起复查，旧行为无余额无 delta
    assert resolve_claim_credits({}, None, recheck=_no_recheck) == (None, 0, "legacy")


if __name__ == "__main__":
    test_extract_user_id()
    test_get_jwt_exp()
    test_rand_digits_deterministic()
    test_classify_error_mapping()
    test_classify_error_business_and_unknown()
    test_consecutive_server_errors_cooldown()
    test_client_errors_share_strike_counter_type_gate()
    test_parse_claim_reward_fields()
    test_resolve_claim_credits_layer1_claim_reward()
    test_resolve_claim_credits_layer2_balance_diff()
    test_resolve_claim_credits_layer3_fallback()
    print("ALL TESTS PASSED")
