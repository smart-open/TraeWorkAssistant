import { describe, expect, it } from 'vitest';

import { DISPATCH_PRESETS, matchPreset } from './dispatchPresets';

describe('matchPreset（调度策略收口：预设匹配）', () => {
  it('数据未就绪（pool/policy 为 null）返回 undefined', () => {
    expect(matchPreset(null, { strategy: 'smart' })).toBeUndefined();
    expect(matchPreset({ strategy: '', wb_strategy: '' }, null)).toBeUndefined();
    expect(matchPreset(null, null)).toBeUndefined();
  });

  it('默认配置（空策略 + 智能调度）命中「积分保鲜」', () => {
    // api_pool.json 默认 strategy 空串（语义等同 expire_first），dispatch_policy 默认 smart
    const hit = matchPreset({ strategy: '', wb_strategy: '' }, { strategy: 'smart' });
    expect(hit?.key).toBe('fresh');
  });

  it('显式 expire_first 双池 + 智能调度命中「积分保鲜」', () => {
    const hit = matchPreset(
      { strategy: 'expire_first', wb_strategy: 'expire_first' },
      { strategy: 'smart' },
    );
    expect(hit?.key).toBe('fresh');
  });

  it('Buddy 池空串跟随 Trae 池生效值（跟随 weighted 命中「智能均衡」）', () => {
    const hit = matchPreset({ strategy: 'weighted', wb_strategy: '' }, { strategy: 'smart' });
    expect(hit?.key).toBe('balanced');
    expect(hit?.recommended).toBe(true);
  });

  it('固定优先级组合命中「固定优先」', () => {
    const hit = matchPreset(
      { strategy: 'expire_first', wb_strategy: 'expire_first' },
      { strategy: 'priority' },
    );
    expect(hit?.key).toBe('fixed');
  });

  it('非预设组合（如 Trae/Buddy 策略不同）返回 undefined（自定义组合）', () => {
    expect(
      matchPreset({ strategy: 'weighted', wb_strategy: 'p2c' }, { strategy: 'smart' }),
    ).toBeUndefined();
    expect(
      matchPreset({ strategy: 'random', wb_strategy: 'random' }, { strategy: 'priority' }),
    ).toBeUndefined();
  });

  it('池间策略一致但池内策略不匹配时不命中', () => {
    expect(
      matchPreset({ strategy: 'random', wb_strategy: 'random' }, { strategy: 'smart' }),
    ).toBeUndefined();
  });

  it('预设定义完整：5 组且互不重复、均含名称与说明', () => {
    expect(DISPATCH_PRESETS).toHaveLength(5);
    const keys = new Set(DISPATCH_PRESETS.map((p) => p.key));
    expect(keys.size).toBe(5);
    for (const p of DISPATCH_PRESETS) {
      expect(p.name).toBeTruthy();
      expect(p.desc).toBeTruthy();
      expect(['smart', 'priority']).toContain(p.inter);
    }
  });
});
