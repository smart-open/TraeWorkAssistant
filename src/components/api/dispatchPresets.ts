/**
 * 调度策略中心 · 最佳组合预设与匹配逻辑（纯函数，便于单测）
 * 从 ResourceSummary.tsx 抽出：预设定义 + 当前配置 → 预设命中判定。
 * 匹配规则：池间策略一致 + Trae 池策略一致（空串语义等同 expire_first 默认）
 * + Buddy 池策略一致（空 = 跟随 Trae 池生效值）。
 */
import type { ApiPoolFile, DispatchPolicy } from '../../types';

/** 最佳组合预设：inter = 池间策略；trae / buddy = 两池池内策略（预设均为显式策略） */
export interface DispatchPreset {
  key: string;
  name: string;
  desc: string;
  inter: DispatchPolicy['strategy'];
  trae: string;
  buddy: string;
  recommended?: boolean;
}

export const DISPATCH_PRESETS: DispatchPreset[] = [
  {
    key: 'balanced',
    name: '智能均衡',
    desc: '池间智能排序 + 双池三因子加权随机：负载均匀防热点，兼顾到期 / 倍率 / 积分',
    inter: 'smart',
    trae: 'weighted',
    buddy: 'weighted',
    recommended: true,
  },
  {
    key: 'fresh',
    name: '积分保鲜',
    desc: '双池均优先服务积分先到期的账号：优先消耗临期积分包，减少浪费',
    inter: 'smart',
    trae: 'expire_first',
    buddy: 'expire_first',
  },
  {
    key: 'even',
    name: '均匀摊销',
    desc: '双池均优先服务余额最多的账号：积分消耗曲线平滑，余额齐头并进',
    inter: 'smart',
    trae: 'credit_first',
    buddy: 'credit_first',
  },
  {
    key: 'fast',
    name: '低延迟优先',
    desc: '双池 P2C 随机二选一：两次采样择优，天然避开冷却 / 慢账号，请求更快',
    inter: 'smart',
    trae: 'p2c',
    buddy: 'p2c',
  },
  {
    key: 'fixed',
    name: '固定优先',
    desc: '双源模型固定池优先级（可在下方调整顺序），池内积分先过期优先',
    inter: 'priority',
    trae: 'expire_first',
    buddy: 'expire_first',
  },
];

/** 当前配置命中的预设；无命中（自定义组合）或数据未就绪时返回 undefined */
export function matchPreset(
  pool: Pick<ApiPoolFile, 'strategy' | 'wb_strategy'> | null,
  policy: Pick<DispatchPolicy, 'strategy'> | null,
): DispatchPreset | undefined {
  if (!pool || !policy) return undefined;
  const trae = pool.strategy || 'expire_first';
  const buddy = pool.wb_strategy || trae;
  return DISPATCH_PRESETS.find(
    (p) => policy.strategy === p.inter && trae === p.trae && buddy === p.buddy,
  );
}
