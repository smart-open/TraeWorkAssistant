import { useEffect, useState, useCallback } from 'react';
import {
  Play,
  Square,
  Save,
  RefreshCw,
  CheckCircle2,
  XCircle,
  Activity,
  Globe,
} from 'lucide-react';
import PageHeader from '../components/PageHeader';
import { Badge, StatCard } from '../components/ui';
import { useAppStore } from '../store';
import { api } from '../lib/tauri';
import type { Settings, ApiServiceStatus, PoolStatus } from '../types';

const MODEL_OPTIONS = [
  'glm-5.2',
  'glm-5-turbo',
  'glm-5',
  'DeepSeek-V4-Pro',
  'DeepSeek-V4-Flash',
  'kimi-k3',
  'Doubao-Seed-2.1-Pro',
  'Doubao-Seed-2.0-Code',
  'minimax-m3',
  'qwen-3.7-plus',
];

export default function ApiService() {
  const settings = useAppStore((s) => s.settings);
  const saveSettings = useAppStore((s) => s.saveSettings);
  const refreshSettings = useAppStore((s) => s.refreshSettings);
  const accounts = useAppStore((s) => s.accounts);
  const refreshAccounts = useAppStore((s) => s.refreshAccounts);
  const toast = useAppStore((s) => s.pushToast);

  const [form, setForm] = useState<Settings | null>(null);
  const [saving, setSaving] = useState(false);
  const [status, setStatus] = useState<ApiServiceStatus | null>(null);
  const [poolStatus, setPoolStatus] = useState<PoolStatus[]>([]);
  const [enabledUids, setEnabledUids] = useState<Set<string>>(new Set());
  const [starting, setStarting] = useState(false);
  const [stopping, setStopping] = useState(false);

  useEffect(() => {
    void refreshSettings();
    void refreshAccounts();
    void loadPool();
    void refreshStatus();
  }, [refreshSettings, refreshAccounts]);

  useEffect(() => {
    if (settings && !form) {
      setForm(settings);
    }
  }, [settings, form]);

  const refreshStatus = useCallback(async () => {
    try {
      const s = await api.apiServer.status();
      setStatus(s);
      if (s.running) {
        try {
          const ps = await api.apiServer.poolStatus();
          setPoolStatus(ps);
        } catch {
          /* ignore */
        }
      } else {
        setPoolStatus([]);
      }
    } catch {
      /* ignore */
    }
  }, []);

  useEffect(() => {
    if (!status?.running) return;
    const id = setInterval(() => void refreshStatus(), 3000);
    return () => clearInterval(id);
  }, [status?.running, refreshStatus]);

  const loadPool = async () => {
    try {
      const pool = await api.apiServer.poolList();
      setEnabledUids(new Set(pool.enabled_uids));
    } catch {
      /* ignore */
    }
  };

  const update = <K extends keyof Settings>(key: K, val: Settings[K]) => {
    setForm((prev) => (prev ? { ...prev, [key]: val } : prev));
  };

  const save = async () => {
    if (!form) return;
    setSaving(true);
    try {
      await saveSettings(form);
      toast('success', '配置已保存');
    } catch {
      /* toast 已发出 */
    } finally {
      setSaving(false);
    }
  };

  const start = async () => {
    setStarting(true);
    try {
      const s = await api.apiServer.start();
      setStatus(s);
      useAppStore.setState({ apiStatus: s });
      toast('success', `API 服务已启动（端口 ${s.port}）`);
      void refreshStatus();
    } catch (err) {
      toast('error', `启动失败：${String(err)}`);
    } finally {
      setStarting(false);
    }
  };

  const stop = async () => {
    setStopping(true);
    try {
      await api.apiServer.stop();
      toast('info', 'API 服务已停止');
      setStatus(null);
      setPoolStatus([]);
      useAppStore.setState({ apiStatus: null });
    } catch (err) {
      toast('error', `停止失败：${String(err)}`);
    } finally {
      setStopping(false);
    }
  };

  const toggleUid = (uid: string) => {
    setEnabledUids((prev) => {
      const next = new Set(prev);
      if (next.has(uid)) next.delete(uid);
      else next.add(uid);
      return next;
    });
  };

  const savePool = async () => {
    try {
      await api.apiServer.poolSet([...enabledUids]);
      toast('success', '账号池已更新');
      if (status?.running) {
        toast('info', '需重启 API 服务以应用变更');
      }
    } catch (err) {
      toast('error', `保存账号池失败：${String(err)}`);
    }
  };

  const running = status?.running ?? false;
  const poolCount = enabledUids.size;

  return (
    <div>
      <PageHeader
        title="API 服务"
        desc="OpenAI 兼容接口，通过账号池轮转实现多账号负载均衡"
        actions={
          running ? (
            <button
              className="btn-danger flex items-center gap-2"
              onClick={stop}
              disabled={stopping}
            >
              <Square size={16} />
              {stopping ? '停止中…' : '停止服务'}
            </button>
          ) : (
            <button
              className="btn-primary flex items-center gap-2"
              onClick={start}
              disabled={starting || poolCount === 0}
            >
              <Play size={16} />
              {starting ? '启动中…' : poolCount === 0 ? '请先选择账号' : '启动服务'}
            </button>
          )
        }
      />

      {/* 状态卡片 */}
      <div className="mb-5 grid grid-cols-2 gap-3 sm:grid-cols-4">
        <StatCard
          label="运行状态"
          value={running ? '运行中' : '已停止'}
          tone={running ? 'green' : 'slate'}
          hint={running ? `127.0.0.1:${status?.port ?? 0}` : '未启动'}
        />
        <StatCard
          label="总请求数"
          value={status?.total_requests ?? 0}
          tone="brand"
          hint="累计处理的 API 调用"
        />
        <StatCard
          label="活跃账号"
          value={status?.active_uid ? status.active_uid.slice(0, 8) + '…' : '—'}
          tone="blue"
          hint={status?.active_uid ? '当前正在处理请求' : '空闲'}
        />
        <StatCard
          label="池内账号"
          value={poolCount}
          tone="violet"
          hint="已选入轮转池的账号数"
        />
      </div>

      {status?.last_error && (
        <div className="mb-5 flex items-center gap-2 rounded-lg border border-rose-200 bg-rose-50 px-4 py-3 text-sm text-rose-700 dark:border-rose-800 dark:bg-rose-900/20 dark:text-rose-300">
          <XCircle size={16} className="shrink-0" />
          <span className="truncate">{status.last_error}</span>
        </div>
      )}

      <div className="grid grid-cols-1 items-stretch gap-5 lg:grid-cols-2">
        {/* 配置卡片 */}
        <div className="card flex flex-col p-5">
          <div className="mb-4 flex items-center gap-2">
            <Globe size={18} className="text-brand-500" />
            <h2 className="text-sm font-semibold text-slate-800 dark:text-zinc-100">接口配置</h2>
          </div>

          <div className="flex-1 space-y-4">
            <div>
              <label className="mb-1 block text-xs font-medium text-slate-500 dark:text-zinc-400">
                监听端口
              </label>
              <input
                type="number"
                className="input"
                value={form?.api_port ?? 7864}
                onChange={(e) => update('api_port', parseInt(e.target.value) || 7864)}
                disabled={running}
              />
              <p className="mt-1 text-xs text-slate-400">服务运行时无法修改</p>
            </div>

            <div>
              <label className="mb-1 block text-xs font-medium text-slate-500 dark:text-zinc-400">
                API Key（留空则不鉴权）
              </label>
              <input
                type="password"
                className="input"
                placeholder="sk-..."
                value={form?.api_key ?? ''}
                onChange={(e) => update('api_key', e.target.value)}
                disabled={running}
              />
              <p className="mt-1 text-xs text-slate-400">
                客户端请求需携带 Authorization: Bearer &lt;key&gt;
              </p>
            </div>

            <div>
              <label className="mb-1 block text-xs font-medium text-slate-500 dark:text-zinc-400">
                默认模型
              </label>
              <select
                className="input"
                value={form?.api_default_model ?? 'glm-5.2'}
                onChange={(e) => update('api_default_model', e.target.value)}
                disabled={running}
              >
                {MODEL_OPTIONS.map((m) => (
                  <option key={m} value={m}>
                    {m}
                  </option>
                ))}
              </select>
            </div>

            <div className="rounded-lg bg-slate-50 p-3 text-xs text-slate-500 dark:bg-zinc-800/50 dark:text-zinc-400">
              <p className="mb-1 font-medium">使用方式：</p>
              <code className="block break-all text-[11px]">
                POST http://127.0.0.1:{form?.api_port ?? 7864}/v1/chat/completions
              </code>
              <code className="mt-1 block break-all text-[11px]">
                GET http://127.0.0.1:{form?.api_port ?? 7864}/v1/models
              </code>
              <code className="mt-1 block break-all text-[11px]">
                GET http://127.0.0.1:{form?.api_port ?? 7864}/health
              </code>
              <code className="mt-1 block break-all text-[11px]">
                GET http://127.0.0.1:{form?.api_port ?? 7864}/status
              </code>
            </div>

            <button
              className="btn-secondary flex items-center gap-2"
              onClick={save}
              disabled={saving || running}
            >
              <Save size={15} />
              {saving ? '保存中…' : '保存配置'}
            </button>
          </div>
        </div>

        {/* 账号池卡片 */}
        <div className="card flex flex-col p-5">
          <div className="mb-4 flex items-center justify-between">
            <div className="flex items-center gap-2">
              <Activity size={18} className="text-brand-500" />
              <h2 className="text-sm font-semibold text-slate-800 dark:text-zinc-100">
                账号池选择
              </h2>
            </div>
            <button
              className="btn-ghost flex items-center gap-1 text-xs"
              onClick={() => void loadPool()}
            >
              <RefreshCw size={13} />
              刷新
            </button>
          </div>

          {accounts.length === 0 ? (
            <p className="flex-1 py-8 text-center text-sm text-slate-400">暂无账号，请先在账号管理中添加</p>
          ) : (
            <>
              <div className="mb-3 flex items-center gap-2">
                <button
                  className="text-xs text-brand-600 hover:underline dark:text-brand-400"
                  onClick={() =>
                    setEnabledUids(new Set(accounts.map((a) => a.user_id)))
                  }
                >
                  全选
                </button>
                <span className="text-slate-300">|</span>
                <button
                  className="text-xs text-brand-600 hover:underline dark:text-brand-400"
                  onClick={() => setEnabledUids(new Set())}
                >
                  清空
                </button>
                <span className="ml-auto text-xs text-slate-400">
                  已选 {enabledUids.size} / {accounts.length}
                </span>
              </div>

              <div className="flex-1 space-y-1">
                {accounts.map((a) => {
                  const checked = enabledUids.has(a.user_id);
                  const poolItem = poolStatus.find((p) => p.uid === a.user_id);
                  return (
                    <label
                      key={a.user_id}
                      className="flex cursor-pointer items-center gap-3 rounded-lg px-3 py-2 transition hover:bg-slate-50 dark:hover:bg-zinc-800/50"
                    >
                      <input
                        type="checkbox"
                        className="h-4 w-4 rounded border-slate-300 text-brand-600 focus:ring-brand-500"
                        checked={checked}
                        onChange={() => toggleUid(a.user_id)}
                      />
                      <div className="min-w-0 flex-1">
                        <div className="truncate text-sm font-medium text-slate-700 dark:text-zinc-200">
                          {a.name}
                        </div>
                        <div className="truncate text-xs text-slate-400">
                          {a.user_id}
                        </div>
                      </div>
                      <div className="flex shrink-0 items-center gap-2">
                        {a.remaining_credits != null && (
                          <span className="text-xs tabular-nums text-slate-500 dark:text-zinc-400">
                            {a.remaining_credits.toFixed(0)} 积分
                          </span>
                        )}
                        {poolItem?.cooling && (
                          <Badge tone="amber">冷却中</Badge>
                        )}
                        {poolItem?.disabled && (
                          <Badge tone="red">已禁用</Badge>
                        )}
                        {running && poolItem && !poolItem.cooling && !poolItem.disabled && (
                          <Badge tone="green">就绪</Badge>
                        )}
                      </div>
                    </label>
                  );
                })}
              </div>

              <button
                className="btn-secondary mt-3 flex w-full items-center justify-center gap-2"
                onClick={savePool}
              >
                <Save size={15} />
                保存账号池
              </button>
            </>
          )}
        </div>
      </div>

      {/* 运行中池状态详情 */}
      {running && poolStatus.length > 0 && (
        <div className="mt-5 card p-5">
          <div className="mb-3 flex items-center gap-2">
            <CheckCircle2 size={18} className="text-emerald-500" />
            <h2 className="text-sm font-semibold text-slate-800 dark:text-zinc-100">
              池实时状态
            </h2>
          </div>
          <div className="overflow-x-auto">
            <table className="w-full text-sm">
              <thead>
                <tr className="border-b border-slate-200 text-left text-xs text-slate-500 dark:border-zinc-700 dark:text-zinc-400">
                  <th className="pb-2 pr-4 font-medium">账号</th>
                  <th className="pb-2 pr-4 font-medium">UID</th>
                  <th className="pb-2 pr-4 font-medium">积分</th>
                  <th className="pb-2 pr-4 font-medium">状态</th>
                  <th className="pb-2 pr-4 font-medium">错误次数</th>
                  <th className="pb-2 font-medium">冷却原因</th>
                </tr>
              </thead>
              <tbody>
                {poolStatus.map((p) => (
                  <tr
                    key={p.uid}
                    className="border-b border-slate-100 last:border-0 dark:border-zinc-800"
                  >
                    <td className="py-2 pr-4 font-medium text-slate-700 dark:text-zinc-200">
                      {p.name}
                    </td>
                    <td className="py-2 pr-4 text-xs text-slate-400">{p.uid.slice(0, 12)}…</td>
                    <td className="py-2 pr-4 tabular-nums text-slate-600 dark:text-zinc-300">
                      {p.credits != null ? p.credits.toFixed(0) : '—'}
                    </td>
                    <td className="py-2 pr-4">
                      {p.disabled ? (
                        <Badge tone="red">已禁用</Badge>
                      ) : p.cooling ? (
                        <Badge tone="amber">冷却中</Badge>
                      ) : (
                        <Badge tone="green">就绪</Badge>
                      )}
                    </td>
                    <td className="py-2 pr-4 tabular-nums text-slate-500">{p.err_count}</td>
                    <td className="py-2 text-xs text-slate-400">
                      {p.cooldown_reason ?? '—'}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
      )}
    </div>
  );
}
