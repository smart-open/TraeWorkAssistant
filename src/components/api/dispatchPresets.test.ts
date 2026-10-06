import { describe, expect, it } from 'vitest';

import type { ApiPoolFile } from '../../types';
import { DISPATCH_PRESETS, matchPreset } from './dispatchPresets';

describe('matchPreset（调度策略收口：预设匹配）', () => {
  it('数据未就绪（pool/policy 为 null）返回 undefined', () => {
    expect(matchPreset(null, { strategy: 'smart' })).toBeUndefined();
    expect(matchPreset({ strategy: '', wb_strategy: '', qoder_strategy: '' }, null)).toBeUndefined();
    expect(matchPreset(null, null)).toBeUndefined();
  });

  it('默认配置（空策略 + 智能调度）命中「积分保鲜」', () => {
    // api_pool.json 默认 strategy 空串（语义等同 expire_first），dispatch_policy 默认 smart
    const hit = matchPreset({ strategy: '', wb_strategy: '', qoder_strategy: '' }, { strategy: 'smart' });
    expect(hit?.key).toBe('fresh');
  });

  it('显式 expire_first 三池 + 智能调度命中「积分保鲜」', () => {
    const hit = matchPreset(
      { strategy: 'expire_first', wb_strategy: 'expire_first', qoder_strategy: 'expire_first' },
      { strategy: 'smart' },
    );
    expect(hit?.key).toBe('fresh');
  });

  it('Buddy/Qoder 池空串跟随 Trae 池生效值（跟随 weighted 命中「智能均衡」）', () => {
    const hit = matchPreset(
      { strategy: 'weighted', wb_strategy: '', qoder_strategy: '' },
      { strategy: 'smart' },
    );
    expect(hit?.key).toBe('balanced');
    expect(hit?.recommended).toBe(true);
  });

  it('固定优先级组合命中「固定优先」', () => {
    const hit = matchPreset(
      { strategy: 'expire_first', wb_strategy: 'expire_first', qoder_strategy: 'expire_first' },
      { strategy: 'priority' },
    );
    expect(hit?.key).toBe('fixed');
  });

  it('非预设组合（如 Trae/Buddy 策略不同）返回 undefined（自定义组合）', () => {
    expect(
      matchPreset(
        { strategy: 'weighted', wb_strategy: 'p2c', qoder_strategy: 'weighted' },
        { strategy: 'smart' },
      ),
    ).toBeUndefined();
    expect(
      matchPreset(
        { strategy: 'random', wb_strategy: 'random', qoder_strategy: 'random' },
        { strategy: 'priority' },
      ),
    ).toBeUndefined();
  });

  it('池间策略一致但池内策略不匹配时不命中', () => {
    expect(
      matchPreset(
        { strategy: 'random', wb_strategy: 'random', qoder_strategy: 'random' },
        { strategy: 'smart' },
      ),
    ).toBeUndefined();
  });

  it('Qoder 池内策略纳入匹配（qoder_strategy 可配）：偏离预设即自定义组合', () => {
    // 三池中仅 Qoder 独立配了 p2c（其余 weighted）→ 不再命中「智能均衡」
    expect(
      matchPreset(
        { strategy: 'weighted', wb_strategy: 'weighted', qoder_strategy: 'p2c' },
        { strategy: 'smart' },
      ),
    ).toBeUndefined();
    // 三池全部 p2c → 命中「低延迟优先」
    expect(
      matchPreset(
        { strategy: 'p2c', wb_strategy: 'p2c', qoder_strategy: 'p2c' },
        { strategy: 'smart' },
      )?.key,
    ).toBe('fast');
    // qoder_strategy 显式独立配 weighted，其余空（跟随 weighted）→ 命中「智能均衡」
    expect(
      matchPreset(
        { strategy: 'weighted', wb_strategy: '', qoder_strategy: 'weighted' },
        { strategy: 'smart' },
      )?.key,
    ).toBe('balanced');
  });

  it('预设定义完整：5 组且互不重复、均含名称与说明与三池策略', () => {
    expect(DISPATCH_PRESETS).toHaveLength(5);
    const keys = new Set(DISPATCH_PRESETS.map((p) => p.key));
    expect(keys.size).toBe(5);
    for (const p of DISPATCH_PRESETS) {
      expect(p.name).toBeTruthy();
      expect(p.desc).toBeTruthy();
      expect(['smart', 'priority']).toContain(p.inter);
      // 每组预设的池内策略三池齐备且为合法取值
      for (const s of [p.trae, p.buddy, p.qoder]) {
        expect(['expire_first', 'credit_first', 'random', 'weighted', 'p2c']).toContain(s);
      }
    }
  });
});
