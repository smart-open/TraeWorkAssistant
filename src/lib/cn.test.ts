import { describe, expect, it } from 'vitest';

import { cn } from './cn';

describe('cn', () => {
  it('拼接多个类名', () => {
    expect(cn('a', 'b', 'c')).toBe('a b c');
  });

  it('过滤假值（null/undefined/false）', () => {
    expect(cn('a', null, 'b', undefined, false, 'c')).toBe('a b c');
  });

  it('支持对象与数组语法', () => {
    expect(cn({ hidden: false, active: true }, ['x', 'y'])).toBe('active x y');
  });

  it('空输入返回空字符串', () => {
    expect(cn()).toBe('');
  });
});
