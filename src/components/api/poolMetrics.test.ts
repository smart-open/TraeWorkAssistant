import { describe, expect, it } from 'vitest';

import { computePoolMetrics } from './poolMetrics';
import type { AccountView, ApiPoolFile, CustomModel, WorkBuddyAccountView } from '../../types';

/** 最小 Trae 账号（仅单测所需字段，池键 = user_id） */
const trae = (user_id: string, general_credits: number | null) =>
  ({ user_id, general_credits }) as AccountView;

/** 最小 Buddy 账号（仅单测所需字段，池键 = id） */
const wb = (id: string, has_credential: boolean, credits_balance: number | null) =>
  ({ id, has_credential, credits_balance }) as WorkBuddyAccountView;

/** 最小自定义模型 */
const model = (enabled: boolean) => ({ enabled }) as CustomModel;

describe('computePoolMetrics（资源总览三池摘要口径）', () => {
  it('Trae 池：池内账号数 = enabled_uids；可用积分只累计池内账号；积分总余额累计全部账号', () => {
    const accounts = [trae('u1', 100), trae('u2', 50), trae('u3', 10)];
    const pool = { enabled_uids: ['u1', 'u2'] } as ApiPoolFile;
    const m = computePoolMetrics(pool, accounts, [], []);
    expect(m.traePoolCount).toBe(2);
    expect(m.traePoolCredits).toBe(150);
    expect(m.traeAccountTotal).toBe(3);
    expect(m.traeTotalCredits).toBe(160);
  });

  it('Trae 池：general_credits 为 null 按 0 计入；pool 缺失时池内为空但总余额仍统计全部账号', () => {
    const accounts = [trae('u1', null), trae('u2', 30)];
    const m = computePoolMetrics(null, accounts, [], []);
    expect(m.traePoolCount).toBe(0);
    expect(m.traePoolCredits).toBe(0);
    expect(m.traeTotalCredits).toBe(30);
  });

  it('Buddy 池：白名单非空按名单取成员（fail-open 冻结），可用积分只累计名单内账号', () => {
    const wbAccounts = [
      wb('wb-a', true, 10),
      wb('wb-b', true, 20),
      wb('wb-c', true, 40), // 含凭证但不在名单 → 不计入池内
      wb('wb-d', false, 999), // 无凭证 → 永不入池
    ];
    const pool = { enabled_uids: [], wb_enabled: true, wb_enabled_uids: ['wb-a', 'wb-c'] } as ApiPoolFile;
    const m = computePoolMetrics(pool, [], wbAccounts, []);
    expect(m.buddyEnabled).toBe(true);
    expect(m.buddyPoolCount).toBe(2);
    expect(m.buddyPoolCredits).toBe(50); // wb-a + wb-c
    expect(m.buddyAccountTotal).toBe(4);
    expect(m.buddyTotalCredits).toBe(70); // 全部含凭证账号 wb-a/b/c
  });

  it('Buddy 池：白名单为空数组/字段缺失 = fail-open，池内 = 全部含凭证账号（不含无凭证）', () => {
    const wbAccounts = [wb('wb-a', true, 10), wb('wb-b', true, 20), wb('wb-c', false, 30)];
    const empty = computePoolMetrics({ enabled_uids: [], wb_enabled_uids: [] }, [], wbAccounts, []);
    expect(empty.buddyPoolCount).toBe(2);
    expect(empty.buddyPoolCredits).toBe(30);
    const missing = computePoolMetrics({ enabled_uids: [] }, [], wbAccounts, []);
    expect(missing.buddyPoolCount).toBe(2);
    expect(missing.buddyEnabled).toBe(false); // wb_enabled 缺失 = 未启用
  });

  it('Buddy 池：全部余额未知 → credits 为 null（上层展示「未知」）；部分未知按 0 计入', () => {
    const unknown = [wb('wb-a', true, null), wb('wb-b', true, null)];
    const m1 = computePoolMetrics({ enabled_uids: [] }, [], unknown, []);
    expect(m1.buddyPoolCredits).toBeNull();
    expect(m1.buddyTotalCredits).toBeNull();
    const mixed = [wb('wb-a', true, null), wb('wb-b', true, 20)];
    const m2 = computePoolMetrics({ enabled_uids: [] }, [], mixed, []);
    expect(m2.buddyPoolCredits).toBe(20);
  });

  it('自定义模型池：启用数与总数；空数据（pool=null + 空数组）全零且余额为 null', () => {
    const m = computePoolMetrics({ enabled_uids: [] }, [], [], [
      model(true),
      model(false),
      model(true),
    ]);
    expect(m.customEnabledCount).toBe(2);
    expect(m.customModelTotal).toBe(3);
    expect(computePoolMetrics(null, [], [], [])).toEqual({
      traePoolCount: 0,
      traePoolCredits: 0,
      traeAccountTotal: 0,
      traeTotalCredits: 0,
      buddyEnabled: false,
      buddyPoolCount: 0,
      buddyPoolCredits: null,
      buddyTotalCredits: null,
      buddyAccountTotal: 0,
      customEnabledCount: 0,
      customModelTotal: 0,
    });
  });
});
