import { describe, expect, it } from 'vitest';

import type { Settings } from '../types';

import { ALL_APPS, getOrderedApps } from './appVisibility';

/** 仅含 app_order 的最小 Settings（getOrderedApps 不读其他字段） */
const s = (app_order: unknown[]): Settings => ({ app_order }) as unknown as Settings;

describe('getOrderedApps', () => {
  it('null settings / 空 app_order 返回全默认顺序', () => {
    expect(getOrderedApps(null)).toEqual(ALL_APPS);
    expect(getOrderedApps(s([]))).toEqual(['trae', 'buddy', 'qoder', 'doubao', 'buddy-ai']);
  });

  it('自定义顺序按 app_order 存储顺序输出，未提及项默认补尾', () => {
    expect(getOrderedApps(s(['doubao', 'trae']))).toEqual([
      'doubao',
      'trae',
      'buddy',
      'qoder',
      'buddy-ai',
    ]);
  });

  it('非法 key 丢弃，不影响其余排序', () => {
    expect(getOrderedApps(s(['qoder', 'unknown', 'doubao']))).toEqual([
      'qoder',
      'doubao',
      'trae',
      'buddy',
      'buddy-ai',
    ]);
  });

  it('重复 key 保序去重（后端归一化的前端二次防御）', () => {
    expect(getOrderedApps(s(['buddy', 'trae', 'buddy', 'trae']))).toEqual([
      'buddy',
      'trae',
      'qoder',
      'doubao',
      'buddy-ai',
    ]);
  });

  it('老配置只含部分应用时按默认顺序补尾（新版本新增应用兼容）', () => {
    expect(getOrderedApps(s(['doubao']))).toEqual([
      'doubao',
      'trae',
      'buddy',
      'qoder',
      'buddy-ai',
    ]);
  });
});
