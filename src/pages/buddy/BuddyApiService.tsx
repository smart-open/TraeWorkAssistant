import { useCallback, useEffect, useMemo, useState } from 'react';
import { RefreshCw, Save, Coins, ToggleLeft, Activity } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { Badge, Spinner, StatCard } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { withMinDelay } from '../../lib/delay';
import type {
  ApiServiceStatus,
  ApiPoolFile,
  UsageDayView,
  WbModelInfo,
  WorkBuddyAccountView,
} from '../../types';

/**
 * Buddy · 资源调度（unified-api-gateway-design §6.2，Phase 3）
 * 页内仅保留 Buddy 资源级内容：积分体系说明 / 池指标行 / 左列（资源开关（Buddy 独有）+
 * 账号池选择，窄列上下排布）/ 右列 模型目录（Buddy，wb_model_catalog 同步），同行左右两列。
 * 网关级功能（服务启停 / 使用方式 / 生态接入 / WB 用量统计）已全部迁至
 * 全局 API 管理弹窗（左侧栏 KeyRound 图标），页内不再出现网关级内容。
 */

// Buddy 资源开关字段（wb_enabled 上游总开关 / 默认深度思考 / 工具执行 / 后台任务降级）
const WB_FLAG_FIELDS: { key: 'wbEnabled' | 'wbDefaultThinking' | 'wbToolExec' | 'wbBgDowngrade'; label: string; desc: string }[] = [
  {
    key: 'wbEnabled',
    label: '启用 WB 上游',
    desc: 'Buddy 目录模型路由到 WB 账号池，消耗 Buddy 积分（关闭后仅 Buddy 源模型显式报错）',
  },
  {
    key: 'wbDefaultThinking',
    label: '默认深度思考',
    desc: '客户端未显式请求 reasoning_effort 时默认注入 high',
  },
  {
    key: 'wbToolExec',
    label: '网关工具代执行',
    desc: '/v1/responses 声明 web_search 时由代理侧执行搜索并回喂（最多 3 轮）',
  },
  {
    key: 'wbBgDowngrade',
    label: '后台任务降级',
    desc: '标题/摘要类短请求（≤128 token 且 ≤512 字符）路由到最低倍率模型',
  },
];

export default function BuddyApiService() {
  const pushToast = useAppStore((s) => s.pushToast);
  const [status, setStatus] = useState<ApiServiceStatus | null>(null);
  const [pool, setPool] = useState<ApiPoolFile | null>(null);
  const [wbFlags, setWbFlags] = useState({
    wbEnabled: false,
    wbDefaultThinking: false,
    wbToolExec: false,
    wbBgDowngrade: false,
  });
  const [accounts, setAccounts] = useState<WorkBuddyAccountView[]>([]);
  const [catalog, setCatalog] = useState<WbModelInfo[]>([]);
  const [syncing, setSyncing] = useState(false);
  const [saving, setSaving] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  // 当日活跃账号数据源（今日 WB 用量桶的账号维度计数）
  const [usage, setUsage] = useState<UsageDayView[]>([]);

  const refresh = useCallback(async () => {
    setRefreshing(true);
    try {
      const [st, pf, accs, cat] = await Promise.all([
        api.apiServer.status().catch(() => null),
        api.apiServer.poolList().catch(() => null),
        api.workbuddy.accountsList().catch(() => [] as WorkBuddyAccountView[]),
        api.apiServer.wbCatalogList().catch(() => [] as WbModelInfo[]),
      ]);
      setStatus(st);
      setPool(pf);
      setAccounts(accs);
      setCatalog(cat);
      if (pf) {
        setWbFlags({
          wbEnabled: pf.wb_enabled ?? false,
          wbDefaultThinking: pf.wb_default_thinking ?? false,
          wbToolExec: pf.wb_tool_exec ?? false,
          wbBgDowngrade: pf.wb_bg_downgrade ?? false,
        });
      }
    } catch (err) {
      pushToast('error', `读取资源状态失败：${String(err)}`);
    } finally {
      setRefreshing(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // 当日 WB 用量（query_recent 含今日；仅用于「今日使用」池指标）
  const loadTodayUsage = useCallback(async () => {
    try {
      setUsage(await api.apiServer.wbUsageStats(1));
    } catch {
      /* 加载失败按无数据处理 */
    }
  }, []);

  useEffect(() => {
    void loadTodayUsage();
  }, [loadTodayUsage]);

  // 保存资源开关：uids/strategy/groups 原样回传（本页不改 Trae 池配置）
  const saveFlags = async () => {
    setSaving(true);
    try {
      await withMinDelay(
        api.apiServer.poolSet(pool?.enabled_uids ?? [], pool?.strategy, pool?.group_ids, wbFlags),
        600,
      );
      pushToast('success', '资源开关已保存');
      if (status?.running) {
        pushToast('info', '需重启 API 服务以应用变更');
      }
    } catch (err) {
      pushToast('error', `保存失败：${String(err)}`);
    } finally {
      setSaving(false);
    }
  };

  const syncCatalog = async () => {
    if (syncing) return;
    setSyncing(true);
    try {
      const n = await withMinDelay(api.apiServer.wbCatalogSync(), 800);
      pushToast('success', `Buddy 模型目录已更新（${n} 个模型），/v1/models 与路由即时生效`);
      setCatalog(await api.apiServer.wbCatalogList());
    } catch (err) {
      pushToast('error', `Buddy 模型目录同步失败：${String(err).slice(0, 120)}`);
    } finally {
      setSyncing(false);
    }
  };

  const credAccounts = accounts.filter((a) => a.has_credential);

  // 池指标（§6.2 Buddy 口径）：健康 = 含凭证且无需重新登录；今日使用 = 当日被调度使用；池内 = 含凭证账号总数
  const todayKey = new Date().toLocaleDateString('sv-SE');
  const healthyCount = credAccounts.filter((a) => !a.needs_relogin).length;
  const activeToday =
    usage.find((d) => d.date === todayKey)?.accounts.filter((a) => a.requests > 0).length ?? 0;
  const totalBuddyCredits = useMemo(
    () => credAccounts.reduce((s, a) => s + (a.credits_balance ?? 0), 0),
    [credAccounts],
  );

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="Buddy · 资源调度"
        desc="服务资源池管理 · 上游开关与模型目录 · 资源提供给API网关使用"
        actions={
          <button className="btn-outline" onClick={() => void refresh()} disabled={refreshing}>
            <RefreshCw size={15} className={refreshing ? 'animate-spin' : ''} /> 刷新
          </button>
        }
      />

      {/* 积分体系说明（资源级） */}
      <div className="rounded-xl border border-amber-300/70 bg-amber-50/80 px-3.5 py-2.5 dark:border-amber-700/40 dark:bg-amber-900/10">
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-xs font-semibold text-amber-800 dark:text-amber-200">积分体系说明</span>
          <span className="rounded bg-amber-200 px-1.5 py-0.5 text-[10px] font-medium text-amber-700 dark:bg-amber-700/50 dark:text-amber-100">本服务消耗 Buddy 积分</span>
        </div>
        <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-slate-500 dark:text-zinc-400">
          <span>
            Buddy 积分：签到 / 成长中心获取，各模型按倍率消耗；Buddy 目录模型的请求由 WB 账号池服务，
            轮换消耗各账号积分（粘性会话 / 模型级冷却同现有实现）。
          </span>
          <span className="ml-auto">
            含凭证账号 Buddy 积分总余额：
            <span className="font-bold tabular-nums text-amber-700 dark:text-amber-300">
              {credAccounts.some((a) => a.credits_balance != null)
                ? totalBuddyCredits.toLocaleString('zh-CN', { maximumFractionDigits: 2 })
                : '未知'}
            </span>
          </span>
        </div>
      </div>

      {/* 池指标行 */}
      <div className="mt-4 grid grid-cols-3 gap-3">
        <StatCard
          label="健康账号"
          value={healthyCount}
          tone="green"
          hint="含凭证且无需重新登录，可参与调度"
        />
        <StatCard
          label="今日使用"
          value={activeToday}
          tone="blue"
          hint="当日被调度使用过的账号"
        />
        <StatCard
          label="池内账号"
          value={credAccounts.length}
          tone="violet"
          hint="含凭证账号总数（wb_pool）"
        />
      </div>

      {/* 资源开关 + 账号池选择（左列）｜模型目录（Buddy）（右列），同行两列各占 1/2 */}
      <div className="mt-4 grid grid-cols-12 items-start gap-4">
        {/* 左列：资源开关 + 账号池选择（上下排布） */}
        <div className="col-span-6 space-y-4">
          {/* 资源开关卡（Buddy 独有） */}
          <div className="card p-4">
            <div className="mb-3 flex items-center justify-between">
              <div className="flex items-center gap-2">
                <ToggleLeft size={16} className="text-brand-500" />
                <span className="text-sm font-medium">资源开关</span>
              </div>
              <button className="btn-outline" onClick={() => void saveFlags()} disabled={saving}>
                {saving ? <Spinner /> : <Save size={15} />} 保存
              </button>
            </div>
            <div className="mb-2">
              {wbFlags.wbEnabled ? (
                <Badge tone="green">上游已启用</Badge>
              ) : (
                <Badge tone="amber">上游未启用 — Buddy 源模型将显式报错</Badge>
              )}
            </div>
            <div className="space-y-1.5">
              {WB_FLAG_FIELDS.map((item) => (
                <label
                  key={item.key}
                  className="flex cursor-pointer items-start gap-2.5 rounded-md px-1.5 py-1.5 transition hover:bg-slate-50 dark:hover:bg-zinc-800/50"
                >
                  <input
                    type="checkbox"
                    className="mt-0.5 h-3.5 w-3.5 rounded border-slate-300 text-brand-600 focus:ring-brand-500"
                    checked={wbFlags[item.key]}
                    onChange={() => setWbFlags((prev) => ({ ...prev, [item.key]: !prev[item.key] }))}
                  />
                  <span className="min-w-0">
                    <span className="block text-xs text-slate-700 dark:text-zinc-200">{item.label}</span>
                    <span className="block text-[11px] leading-4 text-slate-400 dark:text-zinc-500">{item.desc}</span>
                  </span>
                </label>
              ))}
            </div>
            <p className="mt-2 text-xs text-slate-400 dark:text-zinc-500">
              保存后需重启 API 服务生效；Trae 池的调度策略与分组筛选在 Trae「资源调度」页配置，本页不改动。
            </p>
          </div>

          {/* 账号池选择卡（现有 WB 池状态卡功能保留迁移） */}
          <div className="card p-4">
            <div className="mb-3 flex items-center gap-2">
              <Activity size={16} className="text-brand-500" />
              <span className="text-sm font-medium">账号池选择</span>
            </div>
            <p className="mb-2 text-xs text-slate-400">
              {credAccounts.length} 个含凭证账号参与 WB 上游调度
            </p>
            {credAccounts.length === 0 ? (
              <p className="py-4 text-center text-xs text-slate-400">
                暂无含凭证账号：请先在「账号管理」导入本机账号或 OAuth 扫码入池（凭证写入本地 token store）。
              </p>
            ) : (
              <div className="space-y-1">
                {credAccounts.map((a) => (
                  <div
                    key={a.id}
                    className="flex items-center gap-3 rounded-lg border border-slate-100 px-3 py-2 text-sm dark:border-zinc-800"
                  >
                    <div className="min-w-0 flex-1 truncate font-medium">{a.nickname || a.id}</div>
                    {a.is_current && <Badge tone="green">在线</Badge>}
                    {a.needs_relogin && <Badge tone="amber">需重新登录</Badge>}
                    <span className="shrink-0 text-right tabular-nums text-xs text-slate-500">
                      {a.credits_balance != null ? `${a.credits_balance.toFixed(2)} 积分` : '余额未知'}
                    </span>
                  </div>
                ))}
              </div>
            )}
          </div>
        </div>

        {/* 右列：模型目录（Buddy）卡（现有目录同步能力保留） */}
        <div className="card col-span-6 p-4">
          <div className="mb-3 flex items-center justify-between">
            <div className="flex items-center gap-2">
              <Coins size={16} className="text-amber-500" />
              <span className="text-sm font-medium">模型目录（Buddy）</span>
              <span className="text-xs text-slate-400">{catalog.length} 个模型</span>
            </div>
            <button className="btn-outline" onClick={() => void syncCatalog()} disabled={syncing}>
              <RefreshCw size={14} className={syncing ? 'animate-spin' : ''} />
              {syncing ? '同步中…' : '同步目录'}
            </button>
          </div>
          <p className="mb-3 text-xs text-slate-400">
            从 Buddy 上游模型目录接口拉取并替换 wb_model_catalog.json（倍率/思考档位/图片模态以服务端为准）；
            网关启动时也会自动同步一次。
          </p>
          {catalog.length === 0 ? (
            <p className="py-4 text-center text-xs text-slate-400">暂无目录数据：点击「同步目录」拉取（需至少一个含凭证的 WB 账号）。</p>
          ) : (
            <div className="overflow-x-auto">
              <table className="w-full min-w-[560px] text-sm">
                <thead className="bg-slate-50 text-xs uppercase text-slate-500 dark:bg-zinc-900">
                  <tr>
                    <th className="px-3 py-2 text-left">模型 ID</th>
                    <th className="px-3 py-2 text-left">展示名</th>
                    <th className="px-3 py-2 text-right">积分倍率</th>
                    <th className="px-3 py-2 text-left">思考档位</th>
                    <th className="px-3 py-2 text-right">上下文</th>
                    <th className="px-3 py-2 text-center">图片支持</th>
                  </tr>
                </thead>
                <tbody>
                  {catalog.map((m) => (
                    <tr key={m.id} className="row-hover border-t border-slate-200 dark:border-zinc-800">
                      <td className="px-3 py-2 font-mono text-xs">{m.id}</td>
                      <td className="px-3 py-2">{m.display || '—'}</td>
                      <td className="px-3 py-2 text-right tabular-nums text-amber-600 dark:text-amber-400">{m.rate.toFixed(2)}</td>
                      <td className="px-3 py-2 text-xs text-slate-500">
                        {m.effort_override
                          ? `${m.effort_override}（修正）`
                          : m.supported_efforts.length > 0
                            ? m.supported_efforts.join(' / ')
                            : '—'}
                      </td>
                      <td className="px-3 py-2 text-right tabular-nums text-xs text-slate-500">
                        {(m.context_length / 1000).toFixed(0)}k
                      </td>
                      <td className="px-3 py-2 text-center text-xs">
                        {m.supports_image ? (
                          <span className="font-semibold text-emerald-600 dark:text-emerald-400">✓</span>
                        ) : (
                          <span className="text-slate-400 dark:text-zinc-500">✗</span>
                        )}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
