/** 主题定义与轮询辅助（与 index.css 中 data-theme 覆盖块一一对应） */

export interface ThemeDef {
  id: string;
  name: string;
  dark: boolean;
}

/** 全部主题（顺序即左下角轮询切换顺序）；「跟随系统」不参与轮询，仅设置页可选 */
export const THEMES: ThemeDef[] = [
  { id: 'graphite', name: '石墨灰（浅色）', dark: false },
  { id: 'charcoal', name: '炭黑（深色）', dark: true },
  { id: 'violet-night', name: '暗夜紫', dark: true },
  { id: 'ink-green', name: '墨绿', dark: true },
  { id: 'amber-night', name: '琥珀暖夜', dark: true },
  { id: 'tech-blue', name: '科技蓝', dark: true },
];

export const DEFAULT_THEME = 'charcoal';

/**
 * 解析设置值为主题定义。
 * - 具体主题 id → 直接命中
 * - 'system'（跟随系统）/ 旧值 light / dark / 未设置 → 按系统偏好回落
 */
export function resolveTheme(
  raw: string | undefined,
  systemPrefersDark: boolean,
): ThemeDef {
  if (raw) {
    const hit = THEMES.find((t) => t.id === raw);
    if (hit) return hit;
    if (raw === 'light') return THEMES[0];
    if (raw === 'dark') return THEMES[1];
  }
  return systemPrefersDark
    ? THEMES.find((t) => t.id === DEFAULT_THEME) ?? THEMES[1]
    : THEMES[0];
}

/** 计算下一个轮询主题（旧值先归一化） */
export function nextTheme(raw: string | undefined): ThemeDef {
  let cur = raw ?? '';
  if (cur === 'light') cur = 'graphite';
  if (cur === 'dark' || cur === 'system' || !cur) cur = DEFAULT_THEME;
  const idx = THEMES.findIndex((t) => t.id === cur);
  return THEMES[(idx + 1) % THEMES.length];
}
