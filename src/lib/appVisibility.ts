import type { AppKey, Settings } from '../types';

/** 全部合法应用 key */
export const ALL_APPS: AppKey[] = ['trae', 'buddy', 'qoder', 'doubao', 'buddy-ai'];

/** 参与资源池调度的应用（豆包无池；custom 池恒可见不在此列） */
export const POOL_APPS: AppKey[] = ['trae', 'buddy', 'qoder', 'buddy-ai'];

/** 资源池 key → 所属应用（custom 恒可见不在映射内；未知池视为可见） */
const POOL_TO_APP: Partial<Record<string, AppKey>> = {
  trae: 'trae',
  buddy: 'buddy',
  qoder: 'qoder',
  'buddy-ai': 'buddy-ai',
};

/** 固定应用归一：非法值/缺失回退 trae，buddy-ai 不参与固定 */
export function getPinnedApp(settings: Settings | null): AppKey {
  const pinned = settings?.pinned_app;
  return ALL_APPS.includes(pinned as AppKey) && pinned !== 'buddy-ai' ? (pinned as AppKey) : 'trae';
}

/**
 * 应用是否可见（系统设置「应用显示」三字段口径，与 Sidebar 一致）。
 * settings 为 null 时按默认值判定：固定 trae、buddy-ai 不显示、无隐藏列表。
 */
export function isAppVisible(settings: Settings | null, app: AppKey): boolean {
  if (app === getPinnedApp(settings)) return true;
  if (app === 'buddy-ai') return settings?.buddy_ai_visible === true;
  return !(settings?.hidden_apps ?? []).includes(app);
}

/** 资源池是否可见：custom 恒可见；未知池视为可见；其余跟随所属应用显隐 */
export function isPoolVisible(settings: Settings | null, pool: string): boolean {
  if (pool === 'custom') return true;
  const app = POOL_TO_APP[pool];
  return app != null ? isAppVisible(settings, app) : true;
}

/** 可见应用列表（按固定顺序） */
export function getVisibleApps(settings: Settings | null): AppKey[] {
  return ALL_APPS.filter((a) => isAppVisible(settings, a));
}

/**
 * 应用展示顺序（用户自定义 + 默认兜底）：app_order 中出现的合法 key 按存储顺序在前
 * （后端 state.rs 已去重，这里再防御一次），未提及的按 ALL_APPS 默认顺序补在末尾——
 * 新版本新增应用对老配置（app_order 缺该 key）天然兼容；空 app_order = 全默认顺序
 */
export function getOrderedApps(settings: Settings | null): AppKey[] {
  const seen = new Set<AppKey>();
  const saved: AppKey[] = [];
  for (const k of settings?.app_order ?? []) {
    if (ALL_APPS.includes(k) && !seen.has(k)) {
      seen.add(k);
      saved.push(k);
    }
  }
  return [...saved, ...ALL_APPS.filter((a) => !seen.has(a))];
}

/** 可见资源池列表（trae/buddy/qoder/buddy-ai 的可见子集） */
export function getVisiblePools(settings: Settings | null): string[] {
  return POOL_APPS.filter((p) => isPoolVisible(settings, p));
}

/** 池 → 展示名（说明/提示文案用；buddy-ai 文案带「国际」与历史文案一致） */
const POOL_TEXT: Record<string, string> = {
  trae: 'Trae',
  buddy: 'Buddy',
  qoder: 'Qoder',
  'buddy-ai': 'BuddyAI 国际',
};

/** 可见池名拼接（如 "Trae / Buddy"）；四池全隐藏时返回空串，调用方自行回退通用表述 */
export function getVisiblePoolText(settings: Settings | null): string {
  return getVisiblePools(settings)
    .map((p) => POOL_TEXT[p] ?? p)
    .join(' / ');
}
