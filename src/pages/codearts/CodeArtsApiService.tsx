import { useCallback, useEffect, useState } from 'react';
import { RefreshCw, Save, ToggleLeft, Layers, Server } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { Badge } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import type {
  ApiServiceStatus,
  ApiPoolFile,
  CodeartsAccountView,
  PoolStatus,
  UnifiedModel,
} from '../../types';

/**
 * CodeArts · 资源调度（华为云码道，国内站）
 *
 * 第六资源池：AK/SK 账号池经 SDK-HMAC-SHA256 签名直连码道模型网关。
 * 本页管理池开关 / 入池白名单 / 调度参数（并发、池粘性、会话粘性、竞速对冲）/
 * 池内策略 / 模型目录；保存即热应用（poolSet 走后端热重载）。
 */
export default function CodeArtsApiService() {
  const pushToast = useAppStore((s) => s.pushToast);
  const [refreshing, setRefreshing] = useState(false);
  const [saving, setSaving] = useState(false);
  const [status, setStatus] = useState<ApiServiceStatus | null>(null);
  const [pool, setPool] = useState<ApiPoolFile | null>(null);
  const [accounts, setAccounts] = useState<CodeartsAccountView[]>([]);
  const [models, setModels] = useState<UnifiedModel[]>([]);
  const [poolStatus, setPoolStatus] = useState<PoolStatus[]>([]);

  const [enabled, setEnabled] = useState(false);
  const [hedgeMs, setHedgeMs] = useState(8000);
  const [stickyEnabled, setStickyEnabled] = useState(true);
  const [concurrency, setConcurrency] = useState(3);
  const [poolStickyTtl, setPoolStickyTtl] = useState(300);
  const [stickyTtl, setStickyTtl] = useState(1800);
  const [strategy, setStrategy] = useState('');
  const [enabledUids, setEnabledUids] = useState<string[] | null>(null);

  const refresh = useCallback(async () => {
    setRefreshing(true);
    try {
      const [st, pf, accs, cat] = await Promise.all([
        api.apiServer.status().catch(() => null),
        api.apiServer.poolList().catch(() => null),
        api.codearts.accountsList().catch(() => [] as CodeartsAccountView[]),
        api.apiServer
          .unifiedModels()
          .then((list) => list.filter((m) => m.sources.some((s) => s.pool === 'codearts')))
          .catch(() => [] as UnifiedModel[]),
      ]);
      setStatus(st);
      setPool(pf);
      setAccounts(accs);
      setModels(cat);
      if (pf) {
        setEnabled(pf.codearts_enabled ?? false);
        setHedgeMs(pf.codearts_hedge_threshold_ms ?? 8000);
        // 回退值与后端 serde 默认一致：粘性默认开（保上游 prompt cache），
        // 并发上限默认 3（= 码道每账号会话名额数）
        setStickyEnabled(pf.codearts_sticky_enabled ?? true);
        setConcurrency(pf.codearts_account_concurrency_limit ?? 3);
        setPoolStickyTtl(pf.codearts_pool_sticky_ttl_secs ?? 300);
        setStickyTtl(pf.codearts_sticky_ttl_secs ?? 1800);
        setStrategy(pf.codearts_strategy ?? '');
        setEnabledUids(pf.codearts_enabled_uids?.length ? pf.codearts_enabled_uids : null);
      }
      void api.apiServer.codeArtsPoolStatus().then(setPoolStatus).catch(() => setPoolStatus([]));
    } catch (err) {
      pushToast('error', `读取 CodeArts 资源状态失败：${String(err)}`);
    } finally {
      setRefreshing(false);
    }
  }, [pushToast]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const save = async () => {
    setSaving(true);
    try {
      await api.apiServer.poolSet(enabledUids ?? [], strategy || undefined, undefined, {
        codeartsEnabled: enabled,
        codeartsHedgeThresholdMs: hedgeMs,
        codeartsStickyEnabled: stickyEnabled,
        codeartsAccountConcurrencyLimit: concurrency,
        codeartsPoolStickyTtlSecs: poolStickyTtl,
        codeartsStickyTtlSecs: stickyTtl,
        codeartsStrategy: strategy,
        ...(enabledUids ? { codeartsUids: enabledUids } : {}),
      });
      pushToast('success', 'CodeArts 池配置已保存并热应用');
      await refresh();
    } catch (err) {
      pushToast('error', `保存失败：${String(err)}`);
    } finally {
      setSaving(false);
    }
  };

  const toggleUid = (id: string) => {
    setEnabledUids((prev) => {
      const cur = prev ?? accounts.map((a) => a.id);
      return cur.includes(id) ? cur.filter((u) => u !== id) : [...cur, id];
    });
  };

  const inputCls =
    'h-8 w-28 rounded-md border border-slate-200 bg-white px-2 text-xs text-slate-700 outline-none focus:border-brand-400 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200';

  return (
    <div className="space-y-4">
      <PageHeader
        title="CodeArts · 资源调度"
        desc="华为云码道（国内站）AK/SK 账号池 · SDK-HMAC-SHA256 签名 · 网关一等池"
        actions={
          <button className="btn btn-ghost" onClick={() => void refresh()} disabled={refreshing}>
            <RefreshCw size={14} className={refreshing ? 'animate-spin' : ''} /> 刷新
          </button>
        }
      />

      <div className="card p-4">
        <div className="flex items-center justify-between">
          <div className="flex items-center gap-2">
            <ToggleLeft size={18} className="text-brand-500" />
            <span className="text-sm font-medium">CodeArts 上游开关</span>
            <Badge tone={enabled ? 'green' : 'slate'}>{enabled ? '已启用' : '已关闭'}</Badge>
          </div>
          <button
            className={`relative h-6 w-11 rounded-full transition-colors ${enabled ? 'bg-brand-500' : 'bg-slate-300 dark:bg-zinc-600'}`}
            onClick={() => setEnabled(!enabled)}
            aria-label="切换 CodeArts 上游开关"
          >
            <span
              className={`absolute top-0.5 h-5 w-5 rounded-full bg-white shadow transition-all ${enabled ? 'left-5' : 'left-0.5'}`}
            />
          </button>
        </div>
        <p className="mt-2 text-xs text-slate-500 dark:text-zinc-400">
          开启后，模型目录中标记为 CodeArts 源的模型将路由到 AK/SK 账号池；关闭时这些模型显式报错。
        </p>
        <div className="mt-3 flex items-center gap-2 text-xs text-slate-500">
          <span>网关状态：</span>
          <Badge tone={status?.running ? 'green' : 'slate'}>{status?.running ? '运行中' : '未启动'}</Badge>
          <span>在池账号：{poolStatus.length}</span>
        </div>
      </div>

      <div className="card p-4">
        <div className="mb-3 flex items-center gap-2">
          <Layers size={16} className="text-brand-500" />
          <span className="text-sm font-medium">入池账号白名单</span>
          <span className="text-xs text-slate-400">（不选 = 全部已录入 AK/SK 账号参与调度）</span>
        </div>
        {accounts.length === 0 ? (
          <p className="text-xs text-slate-400">尚无 CodeArts 账号，请先在「账号管理」录入 AK/SK。</p>
        ) : (
          <div className="flex flex-wrap gap-2">
            {accounts.map((a) => {
              const all = enabledUids ?? accounts.map((x) => x.id);
              const on = all.includes(a.id);
              return (
                <button
                  key={a.id}
                  onClick={() => toggleUid(a.id)}
                  className={`rounded-full border px-3 py-1 text-xs transition-colors ${
                    on
                      ? 'border-brand-400 bg-brand-50 text-brand-700 dark:bg-brand-900/30 dark:text-brand-300'
                      : 'border-slate-200 text-slate-500 dark:border-zinc-700 dark:text-zinc-400'
                  }`}
                >
                  {a.name || a.ak_masked}
                </button>
              );
            })}
          </div>
        )}
      </div>

      <div className="card p-4">
        <div className="mb-3 flex items-center gap-2">
          <Server size={16} className="text-brand-500" />
          <span className="text-sm font-medium">调度参数（保存即热应用）</span>
        </div>
        <div className="grid grid-cols-2 gap-4 md:grid-cols-3">
          <label className="flex flex-col gap-1 text-xs text-slate-500">
            池内策略（空 = 跟随全局）
            <input className={inputCls} value={strategy} onChange={(e) => setStrategy(e.target.value)} placeholder="如 expire_first" />
          </label>
          <label className="flex flex-col gap-1 text-xs text-slate-500">
            账号并发上限（0 = 不限；码道每账号名额为 3）
            <input type="number" min={0} className={inputCls} value={concurrency} onChange={(e) => setConcurrency(Number(e.target.value))} />
          </label>
          <label className="flex flex-col gap-1 text-xs text-slate-500">
            竞速对冲阈值毫秒（0 = 关）
            <input type="number" min={0} step={500} className={inputCls} value={hedgeMs} onChange={(e) => setHedgeMs(Number(e.target.value))} />
          </label>
          <label className="flex flex-col gap-1 text-xs text-slate-500">
            池粘性 TTL 秒
            <input type="number" min={0} className={inputCls} value={poolStickyTtl} onChange={(e) => setPoolStickyTtl(Number(e.target.value))} />
          </label>
          <label className="flex flex-col gap-1 text-xs text-slate-500">
            会话粘性 TTL 秒
            <input type="number" min={0} className={inputCls} value={stickyTtl} onChange={(e) => setStickyTtl(Number(e.target.value))} />
          </label>
          <label className="flex items-center gap-2 text-xs text-slate-500">
            <input type="checkbox" checked={stickyEnabled} onChange={(e) => setStickyEnabled(e.target.checked)} />
            启用会话粘性
          </label>
        </div>
      </div>

      <div className="card p-4">
        <div className="mb-3 flex items-center justify-between">
          <span className="text-sm font-medium">模型目录（CodeArts 源）</span>
          <button
            className="btn btn-ghost"
            onClick={async () => {
              try {
                const n = await api.apiServer.codeArtsCatalogSync();
                pushToast('success', `已同步 ${n} 个 CodeArts 模型`);
                await refresh();
              } catch (err) {
                pushToast('error', `同步失败：${String(err)}`);
              }
            }}
          >
            立即同步目录
          </button>
        </div>
        {models.length === 0 ? (
          <p className="text-xs text-slate-400">
            暂无 CodeArts 模型。请录入 AK/SK 后点击「立即同步目录」，或等待每日定时同步（/v1/model/builtin）。
          </p>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-xs">
              <thead>
                <tr className="border-b border-slate-200 text-left text-slate-400 dark:border-zinc-700">
                  <th className="py-2">模型</th>
                  <th className="py-2">来源池</th>
                  <th className="py-2">状态</th>
                </tr>
              </thead>
              <tbody>
                {models.map((m) => (
                  <tr key={m.id} className="border-b border-slate-100 dark:border-zinc-800">
                    <td className="py-2 font-medium">{m.display || m.id}</td>
                    <td className="py-2">
                      <Badge tone="cyan">CodeArts</Badge>
                    </td>
                    <td className="py-2">{m.sources.some((s) => s.enabled) ? '可用' : '未启用'}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>

      <div className="flex justify-end">
        <button className="btn btn-primary" onClick={() => void save()} disabled={saving}>
          <Save size={14} /> {saving ? '保存中…' : '保存配置'}
        </button>
      </div>
    </div>
  );
}
