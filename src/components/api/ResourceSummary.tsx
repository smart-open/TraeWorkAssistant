/**
 * 全局 API 管理 · 目前资源（unified-api-gateway-design §5.2/§5.3）
 * Trae / Buddy / 自定义 三池压缩只读摘要；前两者详情引导至各应用「资源调度」页
 * （Phase 3 已改造），自定义池详情引导至「自定义模型」Tab。
 * 数据源：pool_list / accounts_list（Trae 池）、pool_list.wb_enabled / workbuddy_accounts_list
 * （Buddy 池）、custom_models_list（自定义模型池）。
 */
import { useEffect, useState, type ReactNode } from 'react';
import { Blocks, Bot, Layers, Server } from 'lucide-react';
import { Badge } from '../ui';
import { api } from '../../lib/tauri';
import { fmtCredits } from '../../lib/format';
import type { AccountView, ApiPoolFile, CustomModel, WorkBuddyAccountView } from '../../types';

function Row({ label, value }: { label: string; value: ReactNode }) {
  return (
    <div className="flex items-center justify-between gap-2 text-xs">
      <span className="text-slate-400 dark:text-zinc-500">{label}</span>
      <span className="tabular-nums text-slate-700 dark:text-zinc-200">{value}</span>
    </div>
  );
}

/** Trae 池调度策略文案（与账号池选择下拉一致） */
const STRATEGY_LABELS: Record<string, string> = {
  expire_first: '积分先过期优先',
  credit_first: '剩余积分多优先',
  random: '随机',
};

export default function ResourceSummary() {
  const [pool, setPool] = useState<ApiPoolFile | null>(null);
  const [accounts, setAccounts] = useState<AccountView[]>([]);
  const [wbAccounts, setWbAccounts] = useState<WorkBuddyAccountView[]>([]);
  const [customModels, setCustomModels] = useState<CustomModel[]>([]);

  useEffect(() => {
    api.apiServer
      .poolList()
      .then(setPool)
      .catch(() => {
        /* 保留空摘要 */
      });
    api.accounts
      .list()
      .then(setAccounts)
      .catch(() => {
        /* 保留空摘要 */
      });
    api.workbuddy
      .accountsList()
      .then(setWbAccounts)
      .catch(() => {
        /* 保留空摘要 */
      });
    api.apiServer
      .customModelsList()
      .then(setCustomModels)
      .catch(() => {
        /* 保留空摘要 */
      });
  }, []);

  // Trae 池：池内账号与通用积分（本服务消耗通用积分，零积分账号无法服务请求）
  const traePoolCount = pool?.enabled_uids.length ?? 0;
  const traeWithCredits = accounts.filter((a) => (a.general_credits ?? 0) > 0).length;
  const traeTotalCredits = accounts.reduce((s, a) => s + (a.general_credits ?? 0), 0);

  // Buddy 池：上游开关 + 含凭证账号与积分余额
  const buddyEnabled = pool?.wb_enabled ?? false;
  const credAccounts = wbAccounts.filter((a) => a.has_credential);
  const buddyTotalCredits = credAccounts.reduce((s, a) => s + (a.credits_balance ?? 0), 0);

  // 自定义模型池：启用条目数 / 总条目数
  const customEnabled = customModels.filter((m) => m.enabled).length;

  return (
    <div className="grid grid-cols-1 gap-4 lg:grid-cols-3">
      {/* Trae 资源池摘要 */}
      <div className="card p-4">
        <div className="mb-3 flex items-center gap-2">
          <Server size={16} className="text-brand-500" />
          <span className="text-sm font-medium text-slate-800 dark:text-zinc-100">Trae 资源池</span>
          <span className="ml-auto text-[11px] text-slate-400">消耗通用积分（product_id 208）</span>
        </div>
        <div className="space-y-1.5">
          <Row label="池内账号" value={traePoolCount} />
          <Row label="含通用积分账号" value={traeWithCredits} />
          <Row
            label="通用积分总余额"
            value={<span className="font-semibold text-amber-600 dark:text-amber-400">{fmtCredits(traeTotalCredits)}</span>}
          />
          <Row
            label="调度策略"
            value={
              pool?.strategy
                ? (STRATEGY_LABELS[pool.strategy] ?? pool.strategy)
                : '—'
            }
          />
        </div>
        <p className="mt-3 border-t border-slate-100 pt-2 text-[11px] text-slate-400 dark:border-zinc-800 dark:text-zinc-500">
          详情（账号池选择 / 分组筛选 / 模型目录）见 Trae「资源调度」页
        </p>
      </div>

      {/* Buddy 资源池摘要 */}
      <div className="card p-4">
        <div className="mb-3 flex items-center gap-2">
          <Bot size={16} className="text-violet-500" />
          <span className="text-sm font-medium text-slate-800 dark:text-zinc-100">Buddy 资源池</span>
          <Badge tone={buddyEnabled ? 'green' : 'slate'}>{buddyEnabled ? '上游已启用' : '上游未启用'}</Badge>
        </div>
        <div className="space-y-1.5">
          <Row label="含凭证账号" value={credAccounts.length} />
          <Row label="账号总数" value={wbAccounts.length} />
          <Row
            label="Buddy 积分总余额"
            value={
              <span className="font-semibold text-amber-600 dark:text-amber-400">
                {credAccounts.some((a) => a.credits_balance != null) ? fmtCredits(buddyTotalCredits) : '未知'}
              </span>
            }
          />
          <Row
            label="消耗口径"
            value="Buddy 积分（按模型倍率）"
          />
        </div>
        <p className="mt-3 border-t border-slate-100 pt-2 text-[11px] text-slate-400 dark:border-zinc-800 dark:text-zinc-500">
          <Layers size={11} className="mr-1 inline" />
          详情（上游开关 / 模型目录 / 账号池）见 Buddy「资源调度」页
        </p>
      </div>

      {/* 自定义模型池摘要 */}
      <div className="card p-4">
        <div className="mb-3 flex items-center gap-2">
          <Blocks size={16} className="text-sky-500" />
          <span className="text-sm font-medium text-slate-800 dark:text-zinc-100">自定义资源池</span>
          <Badge tone={customEnabled > 0 ? 'green' : 'slate'}>
            {customEnabled > 0 ? '调度中' : '无启用条目'}
          </Badge>
        </div>
        <div className="space-y-1.5">
          <Row label="启用模型" value={customEnabled} />
          <Row label="模型总数" value={customModels.length} />
          <Row label="消耗口径" value="上游自有计费" />
          <Row label="调度语义" value="模型名命中即直达" />
        </div>
        <p className="mt-3 border-t border-slate-100 pt-2 text-[11px] text-slate-400 dark:border-zinc-800 dark:text-zinc-500">
          <Blocks size={11} className="mr-1 inline" />
          详情（新增 / 编辑 / 启停）见「自定义模型」Tab
        </p>
      </div>
    </div>
  );
}
