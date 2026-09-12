/**
 * 全局 API 管理 · API Keys 管理（unified-api-gateway-design §5.2/§5.3）
 * 自 ApiService.tsx 原样搬移（props 化），行为不变：多 Key 签发 / 每日配额 / 鉴权开关 /
 * 子 Key 调度配置（F-35）。数据源：api_keys_list / api_keys_save（改动立即生效）。
 * 子弹框（子 Key 配置/删除确认）沿用 Modal 组件叠加；onSubModalChange 供主弹窗
 * 在子弹框打开期间屏蔽 ESC 双关（主弹窗 onClose 先于子弹框触发）。
 */
import { useCallback, useEffect, useMemo, useState } from 'react';
import { BarChart3, Copy, KeyRound, Plus, Power, RefreshCw, Trash2 } from 'lucide-react';
import { Badge, Modal } from '../ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { maskApiKey, fmtTokens } from '../../lib/format';
import type { ApiKeyEntry, PoolStatus, UsageDayView } from '../../types';

export default function ApiKeysManager({
  onSubModalChange,
}: {
  /** 子弹框（子 Key 配置/删除确认）开关状态上报（主弹窗据此屏蔽 ESC 双关） */
  onSubModalChange?: (open: boolean) => void;
}) {
  const toast = useAppStore((s) => s.pushToast);
  const [apiKeys, setApiKeys] = useState<ApiKeyEntry[]>([]);
  const [authDisabled, setAuthDisabled] = useState(false);
  const [keysSaving, setKeysSaving] = useState(false);
  const [newKeyName, setNewKeyName] = useState('');
  const [newKeyLimit, setNewKeyLimit] = useState(0);
  const [newKeyValue, setNewKeyValue] = useState('');
  // F-35 子 Key 配置弹框 + 删除确认（禁 window.confirm，红线）
  const [editKey, setEditKey] = useState<ApiKeyEntry | null>(null);
  const [editAllowed, setEditAllowed] = useState<Set<string>>(new Set());
  const [editMode, setEditMode] = useState('expire_first');
  const [editDedicated, setEditDedicated] = useState('');
  const [deleteForKey, setDeleteForKey] = useState<ApiKeyEntry | null>(null);
  // 子 Key 配置候选（服务运行中的上游账号；打开弹框时刷新）
  const [poolStatus, setPoolStatus] = useState<PoolStatus[]>([]);
  // 今日按 Key 的 token 用量（「今日已用」列展示）
  const [usage, setUsage] = useState<UsageDayView[]>([]);

  // ---- 多 API Key 管理（原 ApiService.tsx 逻辑原样搬移） ----
  const loadKeys = useCallback(async () => {
    try {
      const view = await api.apiServer.keysList();
      setApiKeys(view.keys);
      setAuthDisabled(view.auth_disabled);
    } catch {
      /* 保留空列表 */
    }
  }, []);

  /** 生成 sk- 前缀随机 Key（前端 crypto 随机源） */
  const generateKeyValue = useCallback(() => {
    const buf = new Uint8Array(24);
    crypto.getRandomValues(buf);
    // F-35：子 Key 统一 ck_ 前缀（旧 sk- Key 仍兼容鉴权）
    setNewKeyValue('ck_' + [...buf].map((b) => b.toString(16).padStart(2, '0')).join('').slice(0, 32));
  }, []);

  const saveKeys = async (next: ApiKeyEntry[], msg: string, nextAuthDisabled?: boolean) => {
    setKeysSaving(true);
    try {
      await api.apiServer.keysSave(next, nextAuthDisabled);
      setApiKeys(next);
      if (nextAuthDisabled !== undefined) setAuthDisabled(nextAuthDisabled);
      toast('success', msg);
    } catch (e) {
      toast('error', `保存 Key 失败：${String(e).slice(0, 120)}`);
    } finally {
      setKeysSaving(false);
    }
  };

  const toggleAuthDisabled = () => {
    const next = !authDisabled;
    void saveKeys(
      apiKeys,
      next ? '已关闭鉴权：无启用 Key 时任何本机程序均可调用（不推荐）' : '已开启鉴权：未配置启用 Key 时请求将被拒绝',
      next,
    );
  };

  const addKey = () => {
    const name = newKeyName.trim();
    if (!name) {
      toast('error', '请填写 Key 名称');
      return;
    }
    if (!newKeyValue.startsWith('ck_') && !newKeyValue.startsWith('sk-')) {
      toast('error', 'Key 值无效（需 ck_ 或 sk- 前缀），请重新生成');
      return;
    }
    if (apiKeys.some((k) => k.key === newKeyValue)) {
      toast('error', 'Key 值与现有条目重复');
      return;
    }
    const entry: ApiKeyEntry = {
      id: crypto.randomUUID(),
      name,
      key: newKeyValue,
      enabled: true,
      daily_limit: Math.max(0, Math.floor(newKeyLimit) || 0),
      created_at: Math.floor(Date.now() / 1000),
      used_date: '',
      used_today: 0,
      allowed_accounts: [],
      schedule_mode: 'expire_first',
      dedicated_account: '',
      daily_stats: [],
    };
    void saveKeys([...apiKeys, entry], `Key「${name}」已添加`);
    setNewKeyName('');
    setNewKeyLimit(0);
    generateKeyValue();
  };

  const toggleKey = (id: string) => {
    const next = apiKeys.map((k) => (k.id === id ? { ...k, enabled: !k.enabled } : k));
    void saveKeys(next, 'Key 状态已更新');
  };

  const deleteKey = (k: ApiKeyEntry) => {
    setDeleteForKey(k);
  };

  const confirmDeleteKey = async () => {
    if (!deleteForKey) return;
    const k = deleteForKey;
    setDeleteForKey(null);
    await saveKeys(apiKeys.filter((x) => x.id !== k.id), `Key「${k.name}」已删除`);
  };

  // 子 Key 配置弹框（F-35）：限定上游 + 专一/临期优先 + 按日统计展示
  const openKeyEdit = (k: ApiKeyEntry) => {
    setEditKey(k);
    setEditAllowed(new Set(k.allowed_accounts));
    setEditMode(k.schedule_mode || 'expire_first');
    setEditDedicated(k.dedicated_account || '');
    // 服务可能刚启动，候选列表即时刷新
    api.apiServer
      .poolStatus()
      .then(setPoolStatus)
      .catch(() => {
        /* 保留上次候选 */
      });
  };

  const confirmKeyEdit = () => {
    if (!editKey) return;
    const next = apiKeys.map((k) =>
      k.id === editKey.id
        ? {
            ...k,
            allowed_accounts: [...editAllowed],
            schedule_mode: editMode,
            dedicated_account: editMode === 'dedicated' ? editDedicated : '',
          }
        : k,
    );
    void saveKeys(next, `Key「${editKey.name}」调度配置已更新`);
    setEditKey(null);
  };

  const updateKeyLimit = (id: string, limit: number) => {
    const v = Math.max(0, Math.floor(limit) || 0);
    const cur = apiKeys.find((k) => k.id === id);
    if (!cur || cur.daily_limit === v) return;
    void saveKeys(apiKeys.map((k) => (k.id === id ? { ...k, daily_limit: v } : k)), '限额已更新');
  };

  const copyKeyValue = async (k: ApiKeyEntry) => {
    try {
      await navigator.clipboard.writeText(k.key);
      toast('success', 'Key 已复制到剪贴板');
    } catch {
      toast('error', '复制失败');
    }
  };

  useEffect(() => {
    void loadKeys();
    generateKeyValue();
    // 上游账号候选（服务未运行时为空列表）
    api.apiServer
      .poolStatus()
      .then(setPoolStatus)
      .catch(() => {
        /* 保留空列表 */
      });
    // 今日 token 用量（读落盘数据，仅取当日条目）
    api.apiServer
      .usageStats(14)
      .then(setUsage)
      .catch(() => {
        /* 保留空列表 */
      });
  }, [loadKeys, generateKeyValue]);

  // 子弹框开关状态上报（供主弹窗屏蔽 ESC 双关）
  useEffect(() => {
    onSubModalChange?.(editKey != null || deleteForKey != null);
  }, [editKey, deleteForKey, onSubModalChange]);

  // 今日按 Key 的 token 用量（Keys 表「今日已用」并列展示；日期口径与后端一致 = 本地时区 YYYY-MM-DD）
  const todayKey = new Date().toLocaleDateString('sv-SE');
  const todayKeyTokens = useMemo(() => {
    const m = new Map<string, { prompt: number; completion: number }>();
    usage.find((d) => d.date === todayKey)?.key_tokens.forEach((t) =>
      m.set(t.name, { prompt: t.prompt_tokens, completion: t.completion_tokens }),
    );
    return m;
  }, [usage, todayKey]);

  return (
    <div className="card p-4">
      <div className="mb-3 flex items-center gap-2">
        <KeyRound size={16} className="text-brand-500" />
        <h3 className="text-sm font-semibold text-slate-800 dark:text-zinc-100">API Keys 管理</h3>
        <span className="hidden text-xs text-slate-400 sm:inline">
          多个 Key 独立签发并设置每日配额；增删/启停立即生效
        </span>
      </div>

      {/* 新增表单 */}
      <div className="mb-3 flex flex-wrap items-end gap-2 rounded-lg bg-slate-50 p-3 dark:bg-zinc-800/50">
        <div className="w-36">
          <label className="mb-1 block text-xs text-slate-500 dark:text-zinc-400">名称</label>
          <input
            className="input"
            placeholder="如：cli / 小工具"
            value={newKeyName}
            onChange={(e) => setNewKeyName(e.target.value)}
          />
        </div>
        <div className="min-w-64 flex-1">
          <label className="mb-1 block text-xs text-slate-500 dark:text-zinc-400">Key 值</label>
          <input
            className="input font-mono text-xs"
            value={newKeyValue}
            onChange={(e) => setNewKeyValue(e.target.value)}
          />
        </div>
        <button
          className="btn-ghost flex items-center gap-1 !p-2 text-xs"
          onClick={generateKeyValue}
          title="重新生成 Key 值"
        >
          <RefreshCw size={13} />
          重新生成
        </button>
        <div className="w-36">
          <label className="mb-1 block text-xs text-slate-500 dark:text-zinc-400">
            日限额/次（0=不限）
          </label>
          <input
            type="number"
            min={0}
            className="input"
            value={newKeyLimit}
            onChange={(e) => setNewKeyLimit(parseInt(e.target.value) || 0)}
          />
        </div>
        <button
          className="btn-outline flex items-center gap-1 !px-3 text-xs"
          onClick={addKey}
          disabled={keysSaving}
        >
          <Plus size={14} />
          添加 Key
        </button>
      </div>

      {/* 鉴权开关：无启用 Key 时的行为（默认拒绝；显式关闭后才放行） */}
      <div className="flex items-center justify-between rounded-lg border border-slate-200 bg-slate-50 px-3 py-2 text-xs dark:border-zinc-700 dark:bg-zinc-800/50">
        <div>
          <p className="font-medium text-slate-700 dark:text-zinc-200">鉴权开关</p>
          <p className="text-slate-400 dark:text-zinc-500">
            {authDisabled
              ? '已关闭：未配置启用 Key 时任何本机程序均可调用（不推荐）'
              : '已开启：未配置启用 Key 时请求将被拒绝并提示创建 Key'}
          </p>
        </div>
        <button
          className={`rounded-full px-3 py-1 text-xs font-medium transition-colors ${
            authDisabled
              ? 'bg-emerald-500/90 text-white hover:bg-emerald-500'
              : 'bg-amber-500/90 text-white hover:bg-amber-500'
          }`}
          onClick={toggleAuthDisabled}
          disabled={keysSaving}
        >
          {authDisabled ? '开启鉴权' : '关闭鉴权'}
        </button>
      </div>

      {apiKeys.length === 0 ? (
        <p className="py-4 text-center text-sm text-slate-400">
          {authDisabled
            ? '暂无 Key — 鉴权已关闭，任何本机程序无需 Key 即可调用'
            : '暂无 Key — 请求将被拒绝；请添加并启用 Key，或关闭鉴权'}
        </p>
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full text-sm">
            <thead>
              <tr className="border-b border-slate-200 text-left text-xs text-slate-500 dark:border-zinc-700 dark:text-zinc-400">
                <th className="pb-2 pr-4 font-medium">名称</th>
                <th className="pb-2 pr-4 font-medium">Key</th>
                <th className="pb-2 pr-4 font-medium">日限额(次)</th>
                <th className="pb-2 pr-4 font-medium">今日已用(次/tok)</th>
                <th className="pb-2 pr-4 font-medium">调度</th>
                <th className="pb-2 pr-4 font-medium">状态</th>
                <th className="pb-2 font-medium">操作</th>
              </tr>
            </thead>
            <tbody>
              {apiKeys.map((k) => {
                const exhausted = k.daily_limit > 0 && k.used_today >= k.daily_limit;
                const kt = todayKeyTokens.get(k.id);
                const ktTotal = kt ? kt.prompt + kt.completion : 0;
                return (
                  <tr
                    key={k.id}
                    className="row-hover border-b border-slate-100 last:border-0 dark:border-zinc-800"
                  >
                    <td className="py-2 pr-4 font-medium text-slate-700 dark:text-zinc-200">
                      {k.name}
                    </td>
                    <td className="py-2 pr-4 font-mono text-xs text-slate-500 dark:text-zinc-400">
                      {maskApiKey(k.key)}
                    </td>
                    <td className="py-2 pr-4">
                      <input
                        type="number"
                        min={0}
                        className="input !w-24 !px-2 !py-1 text-xs"
                        defaultValue={k.daily_limit}
                        onBlur={(e) => {
                          const raw = e.target.value.trim();
                          if (!/^\d+$/.test(raw)) {
                            // 空/非法输入不落 0（不限），还原显示并提示
                            e.target.value = String(k.daily_limit);
                            toast('error', '日限额需为非负整数，已还原原值');
                            return;
                          }
                          updateKeyLimit(k.id, parseInt(raw, 10));
                        }}
                        title="0 表示不限；失焦自动保存"
                      />
                    </td>
                    <td
                      className={
                        'py-2 pr-4 tabular-nums ' +
                        (exhausted
                          ? 'font-semibold text-amber-600 dark:text-amber-400'
                          : 'text-slate-500 dark:text-zinc-400')
                      }
                    >
                      {k.used_today}
                      {k.daily_limit > 0 ? ` / ${k.daily_limit}` : ''} 次
                      {ktTotal > 0 && ` · ${fmtTokens(ktTotal)} tok`}
                    </td>
                    <td className="py-2 pr-4">
                      {(k.schedule_mode || 'expire_first') === 'dedicated' ? (
                        <Badge tone="violet">专一</Badge>
                      ) : (
                        <Badge tone="slate">临期优先</Badge>
                      )}
                      {k.allowed_accounts.length > 0 && (
                        <span className="ml-1 text-xs text-slate-400" title={k.allowed_accounts.join(', ')}>
                          限{k.allowed_accounts.length}账号
                        </span>
                      )}
                    </td>
                    <td className="py-2 pr-4">
                      {k.enabled ? <Badge tone="green">启用中</Badge> : <Badge tone="slate">已禁用</Badge>}
                    </td>
                    <td className="py-2">
                      <div className="flex items-center gap-1">
                        <button
                          className="btn-ghost !p-1.5"
                          title={k.enabled ? '禁用' : '启用'}
                          onClick={() => toggleKey(k.id)}
                          disabled={keysSaving}
                        >
                          <Power
                            size={14}
                            className={k.enabled ? 'text-emerald-500' : 'text-slate-400'}
                          />
                        </button>
                        <button
                          className="btn-ghost !p-1.5"
                          title="复制完整 Key"
                          onClick={() => void copyKeyValue(k)}
                        >
                          <Copy size={14} />
                        </button>
                        <button
                          className="btn-ghost !p-1.5"
                          title="调度配置（限定上游 / 专一 / 临期优先）"
                          onClick={() => openKeyEdit(k)}
                        >
                          <BarChart3 size={14} />
                        </button>
                        <button
                          className="btn-ghost !p-1.5"
                          title="删除"
                          onClick={() => deleteKey(k)}
                          disabled={keysSaving}
                        >
                          <Trash2 size={14} className="text-rose-500" />
                        </button>
                      </div>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}

      {/* 子 Key 调度配置弹框（F-35：限定上游 + 专一/临期优先 + 按日统计） */}
      <Modal
        open={editKey != null}
        onClose={() => setEditKey(null)}
        title={`调度配置 · ${editKey?.name ?? ''}`}
        footer={
          <>
            <button className="btn-outline" onClick={() => setEditKey(null)}>取消</button>
            <button className="btn-primary" onClick={confirmKeyEdit} disabled={keysSaving}>保存</button>
          </>
        }
      >
        <div className="space-y-4 text-sm">
          <div>
            <div className="mb-1.5 text-xs font-medium text-slate-500">调度模式</div>
            <div className="flex gap-2">
              {[
                { key: 'expire_first', label: '临期优先', desc: '按积分最早到期取上游' },
                { key: 'dedicated', label: '专一', desc: '固定绑定单一上游账号' },
              ].map((m) => (
                <button
                  key={m.key}
                  className={`flex-1 rounded-lg border p-2.5 text-left text-xs ${editMode === m.key ? 'border-indigo-400 bg-indigo-50 dark:bg-indigo-500/10' : 'border-slate-200 dark:border-zinc-700'}`}
                  onClick={() => setEditMode(m.key)}
                >
                  <div className="font-medium">{m.label}</div>
                  <div className="mt-0.5 text-slate-400">{m.desc}</div>
                </button>
              ))}
            </div>
          </div>
          {editMode === 'dedicated' && (
            <label className="block">
              <span className="mb-1 block text-xs font-medium text-slate-500">专一账号</span>
              <select className="input w-full" value={editDedicated} onChange={(e) => setEditDedicated(e.target.value)}>
                <option value="">— 默认取限定上游首个 —</option>
                {poolStatus.map((p) => (
                  <option key={p.uid} value={p.uid}>
                    {p.name || p.uid}
                    {p.credits != null ? `（${p.credits.toFixed(1)} 积分）` : ''}
                  </option>
                ))}
              </select>
              {poolStatus.length === 0 && (
                <span className="mt-1 block text-xs text-amber-500">服务未运行，暂无上游账号候选；可保存后稍后调整。</span>
              )}
            </label>
          )}
          <div>
            <div className="mb-1.5 text-xs font-medium text-slate-500">
              限定上游<span className="ml-1 font-normal text-slate-400">（不勾选 = 使用全部上游账号）</span>
            </div>
            <div className="max-h-40 space-y-1 overflow-y-auto rounded-lg border border-slate-200 p-2 dark:border-zinc-700">
              {poolStatus.length === 0 ? (
                <div className="py-2 text-center text-xs text-slate-400">服务未运行，暂无上游账号候选</div>
              ) : (
                poolStatus.map((p) => (
                  <label key={p.uid} className="flex items-center gap-2 text-xs">
                    <input
                      type="checkbox"
                      checked={editAllowed.has(p.uid)}
                      onChange={() => {
                        const next = new Set(editAllowed);
                        if (next.has(p.uid)) next.delete(p.uid);
                        else next.add(p.uid);
                        setEditAllowed(next);
                      }}
                    />
                    <span className="truncate">{p.name || p.uid}</span>
                    {p.credits != null && <span className="ml-auto tabular-nums text-slate-400">{p.credits.toFixed(1)}</span>}
                  </label>
                ))
              )}
            </div>
          </div>
          {editKey && (editKey.daily_stats?.length ?? 0) > 0 && (
            <div>
              <div className="mb-1.5 text-xs font-medium text-slate-500">近 7 日请求统计</div>
              <div className="flex items-end gap-1.5">
                {editKey.daily_stats.slice(-7).map((d) => {
                  const max = Math.max(...editKey.daily_stats.slice(-7).map((x) => x.requests), 1);
                  return (
                    <div key={d.date} className="flex flex-1 flex-col items-center gap-1" title={`${d.date}：${d.requests} 次`}>
                      <span className="text-[10px] tabular-nums text-slate-400">{d.requests}</span>
                      <div
                        className="w-full rounded-t bg-indigo-400"
                        style={{ height: `${Math.max(4, (d.requests / max) * 40)}px` }}
                      />
                      <span className="text-[10px] text-slate-400">{d.date.slice(5)}</span>
                    </div>
                  );
                })}
              </div>
            </div>
          )}
        </div>
      </Modal>

      {/* Key 删除确认弹框（禁 window.confirm，红线） */}
      <Modal
        open={deleteForKey != null}
        onClose={() => setDeleteForKey(null)}
        title="删除 API Key"
        footer={
          <>
            <button className="btn-outline" onClick={() => setDeleteForKey(null)}>取消</button>
            <button className="btn-primary !bg-rose-600 hover:!bg-rose-500" onClick={() => void confirmDeleteKey()}>确认删除</button>
          </>
        }
      >
        <div className="text-sm">
          确认删除 Key「{deleteForKey?.name}」？
          <div className="mt-1 text-xs text-slate-400">使用该 Key 的客户端将立即无法访问（401）。</div>
        </div>
      </Modal>
    </div>
  );
}
