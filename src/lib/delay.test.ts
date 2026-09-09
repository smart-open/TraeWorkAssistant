import { describe, expect, it, vi } from 'vitest';

import { withMinDelay } from './delay';

describe('withMinDelay', () => {
  it('返回原 Promise 的结果', async () => {
    const result = await withMinDelay(Promise.resolve(42), 0);
    expect(result).toBe(42);
  });

  it('保证最少等待时间', async () => {
    vi.useFakeTimers();
    const start = Date.now();
    const p = withMinDelay(Promise.resolve('ok'), 500);
    // 立即推进 100ms：任务尚未到最小等待时间
    vi.advanceTimersByTime(100);
    // 真实时间等待会挂起 fake timer 的 setTimeout，需 flush
    await vi.runAllTimersAsync();
    const result = await p;
    expect(Date.now() - start).toBeGreaterThanOrEqual(0);
    expect(result).toBe('ok');
    vi.useRealTimers();
  });

  it('Promise 被拒绝时错误照常抛出', async () => {
    await expect(
      withMinDelay(Promise.reject(new Error('boom')), 0),
    ).rejects.toThrow('boom');
  });
});
