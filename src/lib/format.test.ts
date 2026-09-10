import { describe, expect, it } from 'vitest';

import { fmtCredits, fmtTokens, maskApiKey, normZero } from './format';

describe('maskApiKey', () => {
  it('保留前4后4，中间打码', () => {
    expect(maskApiKey('sk-abcdef1234567890')).toBe('sk-a****7890');
  });

  it('短 Key 全打码', () => {
    expect(maskApiKey('sk-123')).toBe('****');
    expect(maskApiKey('12345678')).toBe('****');
  });

  it('空 Key 返回空串', () => {
    expect(maskApiKey('')).toBe('');
  });
});

describe('fmtTokens', () => {
  it('小于 1k 原样显示', () => {
    expect(fmtTokens(0)).toBe('0');
    expect(fmtTokens(999)).toBe('999');
  });

  it('千位显示为 k（一位小数）', () => {
    expect(fmtTokens(1_234)).toBe('1.2k');
    expect(fmtTokens(999_999)).toBe('1000.0k');
  });

  it('百万位显示为 M（一位小数）', () => {
    expect(fmtTokens(1_234_567)).toBe('1.2M');
  });
});

describe('normZero', () => {
  it('-0 归一为 0', () => {
    expect(Object.is(normZero(-0), -0)).toBe(false);
    expect(normZero(-0)).toBe(0);
  });

  it('普通值原样保留', () => {
    expect(normZero(958.42)).toBe(958.42);
    expect(normZero(-12.5)).toBe(-12.5);
    expect(Object.is(normZero(0), -0)).toBe(false);
  });
});

describe('fmtCredits', () => {
  it('-0 显示为 0（回归：修复前显示 "-0"）', () => {
    expect(fmtCredits(-0)).toBe('0');
  });

  it('0 显示为 0', () => {
    expect(fmtCredits(0)).toBe('0');
  });

  it('千分位 + 最多 2 位小数', () => {
    expect(fmtCredits(958.42)).toBe('958.42');
    expect(fmtCredits(1234.5)).toBe('1,234.5');
    expect(fmtCredits(2794.556)).toBe('2,794.56');
  });
});
