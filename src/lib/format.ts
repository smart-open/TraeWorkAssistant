/**
 * 显示格式化纯函数：API Key 打码、token 数紧凑显示。
 * 独立于页面组件，便于 Vitest 单元测试（T7）。
 */

/** 将 API Key 打码：保留前4后4，中间用 **** 替代 */
export function maskApiKey(key: string): string {
  if (!key) return '';
  if (key.length <= 8) return '****';
  return `${key.slice(0, 4)}****${key.slice(-4)}`;
}

/** token 数紧凑显示：1234 → 1.2k */
export function fmtTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return String(n);
}
