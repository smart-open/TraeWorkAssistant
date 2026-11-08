import { describe, expect, it } from 'vitest';

import type { ApiPoolFile } from '../../types';
import { DISPATCH_PRESETS, matchPreset } from './dispatchPresets';

describe('matchPreset（调度策略收口：预设匹配）', () => {
  it('数据未就绪（pool/policy 为 null）返回 undefined', () => {
    expect(matchPreset(null, { strategy: 'smart' })).toBeUndefined();
    expect(
      matchPreset(
        { strategy: '', wb_strategy: '', qoder_strategy: '', buddy_ai_strategy: '' },
        null,
      ),
    ).toBeUndefined();
    expect(matchPreset(null, null)).toBeUndefined();
  });

  it('默认配置（全空策略 + 智能调度）不命中预设：Buddy 池空串 = fair_rotation 专属默认，不在预设集内（自定义组合）', () => {
    // api_pool.json 默认全空串：trae/qoder/buddyAi 跟随 expire_first，但 Buddy 池
    // 空串 = fair_rotation（WB 专属默认，与后端 resolve_wb 同口径）→ 无预设命中
    const hit = matchPreset(
      { strategy: '', wb_strategy: '', qoder_strategy: '', buddy_ai_strategy: '' },
      { strategy: 'smart' },
    );
    expect(hit).toBeUndefined();
  });

  it('Buddy 池空串 = fair_rotation（与后端 resolve_wb 同口径），其余池空串跟随 Trae 池', () => {
    // Buddy 空串（fair_rotation）与 Qoder/BuddyAI 空串（跟随 expire_first）→ 自定义
    expect(
      matchPreset(
        { strategy: 'expire_first', wb_strategy: '', qoder_strategy: '', buddy_ai_strategy: '' },
        { strategy: 'smart' },
      ),
    ).toBeUndefined();
    // Buddy 显式 weighted + Qoder/BuddyAI 空串（跟随 weighted）→ 命中「智能均衡」
    const hit = matchPreset(
      { strategy: 'weighted', wb_strategy: 'weighted', qoder_strategy: '', buddy_ai_strategy: '' },
      { strategy: 'smart' },
    );
    expect(hit?.key).toBe('balanced');
    expect(hit?.recommended).toBe(true);
  });

  it('显式 expire_first 各池 + 智能调度命中「积分保鲜」', () => {
    const hit = matchPreset(
      {
        strategy: 'expire_first',
        wb_strategy: 'expire_first',
        qoder_strategy: 'expire_first',
        buddy_ai_strategy: 'expire_first',
      },
      { strategy: 'smart' },
    );
    expect(hit?.key).toBe('fresh');
  });

  it('Qoder/BuddyAI 池空串跟随 Trae 池生效值（跟随 weighted 命中「智能均衡」；Buddy 池不在此列）', () => {
    const hit = matchPreset(
      { strategy: 'weighted', wb_strategy: 'weighted', qoder_strategy: '', buddy_ai_strategy: '' },
      { strategy: 'smart' },
    );
    expect(hit?.key).toBe('balanced');
    expect(hit?.recommended).toBe(true);
  });

  it('固定优先级组合命中「固定优先」', () => {
    const hit = matchPreset(
      {
        strategy: 'expire_first',
        wb_strategy: 'expire_first',
        qoder_strategy: 'expire_first',
        buddy_ai_strategy: 'expire_first',
      },
      { strategy: 'priority' },
    );
    expect(hit?.key).toBe('fixed');
  });

  it('非预设组合（如 Trae/Buddy 策略不同）返回 undefined（自定义组合）', () => {
    expect(
      matchPreset(
        {
          strategy: 'weighted',
          wb_strategy: 'p2c',
          qoder_strategy: 'weighted',
          buddy_ai_strategy: 'weighted',
        },
        { strategy: 'smart' },
      ),
    ).toBeUndefined();
    expect(
      matchPreset(
        {
          strategy: 'random',
          wb_strategy: 'random',
          qoder_strategy: 'random',
          buddy_ai_strategy: 'random',
        },
        { strategy: 'priority' },
      ),
    ).toBeUndefined();
  });

  it('池间策略一致但池内策略不匹配时不命中', () => {
    expect(
      matchPreset(
        {
          strategy: 'random',
          wb_strategy: 'random',
          qoder_strategy: 'random',
          buddy_ai_strategy: 'random',
        },
        { strategy: 'smart' },
      ),
    ).toBeUndefined();
  });

  it('Qoder 池内策略纳入匹配（qoder_strategy 可配）：偏离预设即自定义组合', () => {
    // 各池中仅 Qoder 独立配了 p2c（其余 weighted）→ 不再命中「智能均衡」
    expect(
      matchPreset(
        {
          strategy: 'weighted',
          wb_strategy: 'weighted',
          qoder_strategy: 'p2c',
          buddy_ai_strategy: 'weighted',
        },
        { strategy: 'smart' },
      ),
    ).toBeUndefined();
    // 各池全部 p2c → 命中「低延迟优先」
    expect(
      matchPreset(
        { strategy: 'p2c', wb_strategy: 'p2c', qoder_strategy: 'p2c', buddy_ai_strategy: 'p2c' },
        { strategy: 'smart' },
      )?.key,
    ).toBe('fast');
    // qoder_strategy 显式独立配 weighted，其余空（Buddy=fair_rotation、BuddyAI 跟随 weighted）→ 自定义组合
    expect(
      matchPreset(
        { strategy: 'weighted', wb_strategy: '', qoder_strategy: 'weighted', buddy_ai_strategy: '' },
        { strategy: 'smart' },
      ),
    ).toBeUndefined();
    // Buddy 显式配 weighted、qoder 显式配 weighted，BuddyAI 空（跟随 weighted）→ 命中「智能均衡」
    expect(
      matchPreset(
        { strategy: 'weighted', wb_strategy: 'weighted', qoder_strategy: 'weighted', buddy_ai_strategy: '' },
        { strategy: 'smart' },
      )?.key,
    ).toBe('balanced');
  });

  it('BuddyAI 国际池内策略纳入匹配（buddy_ai_strategy 可配）：偏离预设即自定义组合', () => {
    // 各池中仅 BuddyAI 国际池独立配了 credit_first（其余 weighted）→ 不再命中「智能均衡」
    expect(
      matchPreset(
        {
          strategy: 'weighted',
          wb_strategy: 'weighted',
          qoder_strategy: 'weighted',
          buddy_ai_strategy: 'credit_first',
        },
        { strategy: 'smart' },
      ),
    ).toBeUndefined();
    // buddy_ai_strategy 显式独立配 weighted，Buddy/Qoder 显式 weighted → 命中「智能均衡」
    expect(
      matchPreset(
        { strategy: 'weighted', wb_strategy: 'weighted', qoder_strategy: 'weighted', buddy_ai_strategy: 'weighted' },
        { strategy: 'smart' },
      )?.key,
    ).toBe('balanced');
    // BuddyAI 独立配 weighted，Buddy 空串（fair_rotation）→ 自定义组合（Buddy 不再跟随 Trae）
    expect(
      matchPreset(
        { strategy: 'weighted', wb_strategy: '', qoder_strategy: '', buddy_ai_strategy: 'weighted' },
        { strategy: 'smart' },
      ),
    ).toBeUndefined();
  });

  it('预设定义完整：5 组且互不重复、均含名称与说明与四池策略', () => {
    expect(DISPATCH_PRESETS).toHaveLength(5);
    const keys = new Set(DISPATCH_PRESETS.map((p) => p.key));
    expect(keys.size).toBe(5);
    for (const p of DISPATCH_PRESETS) {
      expect(p.name).toBeTruthy();
      expect(p.desc).toBeTruthy();
      expect(['smart', 'priority']).toContain(p.inter);
      // 每组预设的池内策略四池齐备且为合法取值（白名单与后端 PoolStrategy::parse
      // 合法值同步，勿漏 fair_rotation）
      for (const s of [p.trae, p.buddy, p.qoder, p.buddyAi]) {
        expect(['expire_first', 'credit_first', 'random', 'weighted', 'p2c', 'fair_rotation']).toContain(s);
      }
    }
  });
});
