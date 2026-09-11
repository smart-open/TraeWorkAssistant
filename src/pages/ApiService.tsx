import { useEffect, useState, useCallback, useMemo } from 'react';
import {
  Play,
  Square,
  Save,
  RefreshCw,
  CheckCircle2,
  XCircle,
  Activity,
  Globe,
  Eraser,
  Copy,
  Info,
  BarChart3,
  KeyRound,
  Plus,
  Trash2,
  Power,
  Plug,
} from 'lucide-react';
import {
  Bar,
  BarChart,
  CartesianGrid,
  Legend,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
} from 'recharts';
import PageHeader from '../components/PageHeader';
import { Badge, StatCard, Modal } from '../components/ui';
import { useAppStore } from '../store';
import { api } from '../lib/tauri';
import { withMinDelay } from '../lib/delay';
import { maskApiKey, fmtTokens } from '../lib/format';
import type {
  Settings,
  ApiServiceStatus,
  PoolStatus,
  ModelOption,
  UsageDayView,
  ApiKeyEntry,
  GroupView,
} from '../types';

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
  const [poolStrategy, setPoolStrategy] = useState('expire_first');
  const [poolGroups, setPoolGroups] = useState<Set<string>>(new Set());
  const [groups, setGroups] = useState<GroupView[]>([]);
  const [starting, setStarting] = useState(false);
  const [stopping, setStopping] = useState(false);
  const [savingPool, setSavingPool] = useState(false);
  const [clearingCooldowns, setClearingCooldowns] = useState(false);
  const [refreshingPool, setRefreshingPool] = useState(false);
  const [copying, setCopying] = useState(false);
  const [models, setModels] = useState<ModelOption[]>([]);
  const [syncingModels, setSyncingModels] = useState(false);
  const [usage, setUsage] = useState<UsageDayView[]>([]);
  const [usageDays, setUsageDays] = useState(14);
  const [usageLoading, setUsageLoading] = useState(false);
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

  useEffect(() => {
    void refreshSettings();
    void refreshAccounts();
    void loadPool();
    void refreshStatus();
    void loadModels();
    void loadGroups();
  }, [refreshSettings, refreshAccounts]);

  useEffect(() => {
    if (settings && !form) {
      setForm(settings);
    }
  }, [settings, form]);

  // 用量统计：直接读落盘数据，服务未运行也可查看
  const loadUsage = useCallback(async (days: number) => {
    setUsageLoading(true);
    try {
      setUsage(await api.apiServer.usageStats(days));
    } catch {
      /* 加载失败保留上次数据 */
    } finally {
      setUsageLoading(false);
    }
  }, []);

  useEffect(() => {
    void loadUsage(usageDays);
  }, [usageDays, loadUsage]);

  // ---- 多 API Key 管理 ----
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

  useEffect(() => {
    void loadKeys();
    generateKeyValue();
  }, [loadKeys, generateKeyValue]);

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

  const loadPool = async (manual = false) => {
    setRefreshingPool(true);
    try {
      const pool = await withMinDelay(api.apiServer.poolList());
      setEnabledUids(new Set(pool.enabled_uids));
      setPoolStrategy(pool.strategy || 'expire_first');
      setPoolGroups(new Set(pool.group_ids ?? []));
    } catch {
      // 初始化加载失败静默保留空列表；手动点击刷新失败需给出提示
      if (manual) toast('error', '加载账号池失败，请重试');
    } finally {
      setRefreshingPool(false);
    }
  };

  // 分组列表（供账号池分组筛选使用）
  const loadGroups = async () => {
    try {
      setGroups(await api.groups.list());
    } catch {
      /* 保留空列表 */
    }
  };

  // 分组筛选实时预览（T10）：按当前勾选 + 所选分组即时计算将纳入池中的账号，
  // 让「点选分组」有立即可见的过滤反馈（实际入池在保存并重启 API 服务后生效）
  const groupUidSets = useMemo(
    () => groups.map((g) => ({ id: g.id, uids: new Set(g.uids ?? []) })),
    [groups],
  );
  const poolPreview = useMemo(() => {
    if (poolGroups.size === 0) return { inPool: enabledUids.size, excluded: 0 };
    let inPool = 0;
    let excluded = 0;
    for (const uid of enabledUids) {
      if (groupUidSets.some((g) => poolGroups.has(g.id) && g.uids.has(uid))) inPool += 1;
      else excluded += 1;
    }
    return { inPool, excluded };
  }, [enabledUids, poolGroups, groupUidSets]);
  // 判断账号在当前分组筛选下是否参与调度（账号列表标记用）
  const inPoolFilter = (uid: string) =>
    poolGroups.size === 0 || groupUidSets.some((g) => poolGroups.has(g.id) && g.uids.has(uid));

  // 今日按 Key 的 token 用量（Keys 表「今日已用」并列展示；日期口径与后端一致 = 本地时区 YYYY-MM-DD）
  const todayKey = new Date().toLocaleDateString('sv-SE');
  const todayKeyTokens = useMemo(() => {
    const m = new Map<string, { prompt: number; completion: number }>();
    usage.find((d) => d.date === todayKey)?.key_tokens.forEach((t) =>
      m.set(t.name, { prompt: t.prompt_tokens, completion: t.completion_tokens }),
    );
    return m;
  }, [usage, todayKey]);

  // 加载模型列表（api_models.json，缺失时后端写入默认列表）
  const loadModels = async () => {
    try {
      setModels(await api.apiServer.modelsList());
    } catch {
      /* 加载失败保留空列表，用户可点同步按钮重试 */
    }
  };

  // 从官网同步最新模型列表（batch_get_detail_param 配置接口，不消耗积分）
  const syncModels = async () => {
    if (syncingModels) return;
    setSyncingModels(true);
    try {
      const list = await withMinDelay(api.apiServer.modelsSync());
      setModels(list);
      toast('success', `官网模型同步成功（共 ${list.length} 个）`);
    } catch (e) {
      toast('error', `官网模型同步失败: ${String(e).slice(0, 120)}`);
    } finally {
      setSyncingModels(false);
    }
  };

  // T5.7/F-43：CC Switch 注册状态（注册 Trae 侧条目；WB 侧条目在 Buddy「API 服务」页）
  const [ccBusy, setCcBusy] = useState<'claude' | 'codex' | null>(null);
  const [ecoNote, setEcoNote] = useState('');

  const registerCcSwitch = async (appType: 'claude' | 'codex') => {
    if (ccBusy) return;
    setCcBusy(appType);
    setEcoNote('');
    try {
      const msg = await withMinDelay(api.apiServer.ccSwitchRegister(appType, 'trae'));
      setEcoNote(`✓ ${msg}`);
    } catch (e) {
      setEcoNote(`✗ CC Switch 注册失败：${String(e).slice(0, 160)}`);
    } finally {
      setCcBusy(null);
    }
  };

  const update = <K extends keyof Settings>(key: K, val: Settings[K]) => {
    setForm((prev) => (prev ? { ...prev, [key]: val } : prev));
  };

  const save = async () => {
    if (!form) return;
    setSaving(true);
    try {
      await withMinDelay(saveSettings(form));
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
      // 启动前自动保存当前勾选的账号池，避免用户忘记点"保存"
      // WB 开关字段不传，Rust 端保留 api_pool.json 原值（由 Buddy「API 服务」页维护）
      await api.apiServer.poolSet([...enabledUids], poolStrategy, [...poolGroups]);
      const s = await withMinDelay(api.apiServer.start());
      setStatus(s);
      useAppStore.setState({ apiStatus: s });
      toast('success', `API 服务已启动（端口 ${s.port}，池内 ${enabledUids.size} 个账号）`);
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
      await withMinDelay(api.apiServer.stop());
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
    setSavingPool(true);
    try {
      await withMinDelay(api.apiServer.poolSet([...enabledUids], poolStrategy, [...poolGroups]));
      toast('success', '账号池已更新');
      if (status?.running) {
        toast('info', '需重启 API 服务以应用变更');
      }
    } catch (err) {
      toast('error', `保存账号池失败：${String(err)}`);
    } finally {
      setSavingPool(false);
    }
  };

  const clearAllCooldowns = async () => {
    setClearingCooldowns(true);
    try {
      const cleared = await withMinDelay(api.accounts.cooldownClearAll());
      if (cleared > 0) {
        toast('success', `已清除 ${cleared} 个账号的冷却状态`);
        void refreshStatus();
      } else {
        toast('info', '当前无冷却中的账号');
      }
    } catch (err) {
      toast('error', `清除冷却失败：${String(err)}`);
    } finally {
      setClearingCooldowns(false);
    }
  };

  const copyConfigExample = async () => {
    setCopying(true);
    const port = form?.api_port ?? 7864;
    const model = form?.api_default_model ?? 'glm-5.2';
    const example = `# 客户端配置示例（OpenAI 兼容格式）
接口地址: http://127.0.0.1:${port}/v1
API Key:  <在下方「API Keys 管理」中创建并复制>
模型 ID:  ${model}

# Anthropic 兼容端点（Claude Code 等工具直连）
POST http://127.0.0.1:${port}/v1/messages
鉴权头: x-api-key: your-api-key 或 Authorization: Bearer

# cURL 测试（请将 API Key 替换为列表中的完整值）
curl -X POST http://127.0.0.1:${port}/v1/chat/completions \\
  -H "Content-Type: application/json" \\
  -H "Authorization: Bearer your-api-key" \\
  -d '{
    "model": "${model}",
    "messages": [{"role": "user", "content": "你好"}],
    "stream": true
  }'

# Anthropic /v1/messages 测试
curl -X POST http://127.0.0.1:${port}/v1/messages \\
  -H "Content-Type: application/json" \\
  -H "x-api-key: your-api-key" \\
  -H "anthropic-version: 2023-06-01" \\
  -d '{
    "model": "${model}",
    "max_tokens": 1024,
    "messages": [{"role": "user", "content": "你好"}]
  }'`;
    try {
      await withMinDelay(navigator.clipboard.writeText(example));
      toast('success', '配置示例已复制到剪贴板');
    } catch {
      toast('error', '复制失败');
    } finally {
      setCopying(false);
    }
  };

  const running = status?.running ?? false;
  const poolCount = enabledUids.size;

  // 用量汇总（跨天聚合）+ 图表数据
  const usageSummary = useMemo(() => {
    const models = new Map<string, { requests: number; ok: number; errors: number }>();
    const t = usage.reduce(
      (acc, d) => {
        acc.requests += d.total_requests;
        acc.ok += d.ok;
        acc.errors += d.errors;
        acc.prompt += d.prompt_tokens;
        acc.completion += d.completion_tokens;
        acc.weightedDuration += d.avg_duration_ms * d.total_requests;
        for (const m of d.models) {
          const e = models.get(m.name) ?? { requests: 0, ok: 0, errors: 0 };
          e.requests += m.requests;
          e.ok += m.ok;
          e.errors += m.errors;
          models.set(m.name, e);
        }
        return acc;
      },
      { requests: 0, ok: 0, errors: 0, prompt: 0, completion: 0, weightedDuration: 0 },
    );
    const topModels = [...models.entries()]
      .sort((a, b) => b[1].requests - a[1].requests)
      .slice(0, 5)
      .map(([name, v]) => ({ name, ...v }));
    return {
      ...t,
      topModels,
      successRate: t.requests > 0 ? ((t.ok / t.requests) * 100).toFixed(1) : '—',
      avgDuration: t.requests > 0 ? Math.round(t.weightedDuration / t.requests) : 0,
    };
  }, [usage]);

  const usageChartData = useMemo(
    () => usage.map((d) => ({ date: d.date.slice(5), 成功: d.ok, 失败: d.errors })),
    [usage],
  );

  // 账号池仅展示/可选有通用积分的账号（本服务消耗通用积分，零积分账号无法服务请求）
  const poolAccounts = useMemo(
    () => accounts.filter((a) => (a.general_credits ?? 0) > 0),
    [accounts],
  );
  const totalGeneral = useMemo(
    () => accounts.reduce((s, a) => s + (a.general_credits ?? 0), 0),
    [accounts],
  );

  return (
    <div>
      <PageHeader
        title="Trae · API 服务"
        desc="OpenAI / Anthropic 兼容接口，通过账号池轮转实现多账号负载均衡（消耗通用积分）"
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

      {/* 积分体系说明 — 紧凑面板 */}
      <div className="mb-5 rounded-xl border border-amber-300/70 bg-amber-50/80 px-3.5 py-2.5 dark:border-amber-700/40 dark:bg-amber-900/10">
        <div className="flex flex-wrap items-center gap-2">
          <Info size={14} className="shrink-0 text-amber-500 dark:text-amber-400" />
          <span className="text-xs font-semibold text-amber-800 dark:text-amber-200">积分体系说明</span>
          <span className="rounded bg-amber-200 px-1.5 py-0.5 text-[10px] font-medium text-amber-700 dark:bg-amber-700/50 dark:text-amber-100">本服务消耗通用积分</span>
        </div>
        <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-slate-500 dark:text-zinc-400">
          <span>
            Trae 通用积分（product_id 208）为账号统一积分：签到奖励、每月登录赠送与购买套餐均计入，IDE 聊天与本 API 服务共用扣减；上游接口{' '}
            <code className="font-mono text-amber-700 dark:text-amber-300">llm_utils_chat</code>，明文 JSON。
          </span>
          <span className="ml-auto">
            当前全部账号通用积分总余额：
            <span className="font-bold tabular-nums text-amber-700 dark:text-amber-300">
              {totalGeneral.toLocaleString('zh-CN', { maximumFractionDigits: 0 })}
            </span>
          </span>
        </div>
      </div>

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
                默认模型
              </label>
              <div className="flex items-center gap-2">
                <select
                  className="input flex-1"
                  value={form?.api_default_model ?? 'glm-5.2'}
                  onChange={(e) => update('api_default_model', e.target.value)}
                  disabled={running}
                >
                  {(models.some((m) => m.id === (form?.api_default_model ?? 'glm-5.2'))
                    ? models
                    : [
                        {
                          id: form?.api_default_model ?? 'glm-5.2',
                          label: form?.api_default_model ?? 'glm-5.2',
                        },
                        ...models,
                      ]
                  ).map((m) => (
                    <option key={m.id} value={m.id}>
                      {m.label}
                    </option>
                  ))}
                </select>
                <button
                  className="btn-ghost flex shrink-0 items-center gap-1 !p-2 text-xs"
                  onClick={syncModels}
                  disabled={syncingModels}
                  title="从官网拉取最新模型列表（不消耗积分）"
                >
                  <RefreshCw size={13} className={syncingModels ? 'animate-spin' : ''} />
                  {syncingModels ? '同步中…' : '同步官网模型'}
                </button>
              </div>
              <p className="mt-1 text-xs text-slate-400">
                上游接口：llm_utils_chat（通用积分，product_id 208）
              </p>
            </div>

            <div className="rounded-lg bg-slate-50 p-3 text-xs text-slate-500 dark:bg-zinc-800/50 dark:text-zinc-400">
              <div className="mb-2 flex items-center justify-between">
                <p className="font-medium">使用方式 & 配置示例</p>
                <button
                  className="btn-ghost flex items-center gap-1 !p-1 text-xs"
                  onClick={copyConfigExample}
                  disabled={copying}
                  title="复制完整配置示例"
                >
                  <Copy size={12} className={copying ? 'animate-pulse' : ''} />
                  {copying ? '复制中…' : '复制示例'}
                </button>
              </div>
              <div className="space-y-1.5">
                <div>
                  <span className="text-slate-400">接口地址：</span>
                  <code className="break-all text-[11px]">
                    http://127.0.0.1:{form?.api_port ?? 7864}/v1
                  </code>
                </div>
                <div>
                  <span className="text-slate-400">API Key：</span>
                  <code className="text-[11px]">在下方「API Keys 管理」中创建并复制</code>
                </div>
                <div>
                  <span className="text-slate-400">模型 ID：</span>
                  <code className="text-[11px]">{form?.api_default_model ?? 'glm-5.2'}</code>
                </div>
                <div className="pt-1">
                  <span className="text-slate-400">其他端点：</span>
                </div>
                <code className="block break-all text-[11px]">
                  POST http://127.0.0.1:{form?.api_port ?? 7864}/v1/messages（Anthropic 兼容，x-api-key 鉴权）
                </code>
                <code className="block break-all text-[11px]">
                  GET http://127.0.0.1:{form?.api_port ?? 7864}/v1/models
                </code>
                <code className="block break-all text-[11px]">
                  GET http://127.0.0.1:{form?.api_port ?? 7864}/health
                </code>
              </div>
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

        {/* T5.7 生态接入：CC Switch 协同（独立面板，结构对齐 Buddy「API 服务」页） */}
        <div className="mt-5 card p-5">
          <div className="mb-2 flex items-center gap-2">
            <Plug size={16} className="text-emerald-500" />
            <span className="text-sm font-medium text-slate-800 dark:text-zinc-100">生态接入</span>
          </div>
          <div className="flex flex-wrap items-center gap-2">
            <button
              className="btn-outline !py-1.5 text-xs"
              onClick={() => void registerCcSwitch('claude')}
              disabled={ccBusy !== null}
              title="把网关 Anthropic 端点（/v1/messages）注册进 CC Switch（Trae 侧条目「AI Work 助手网关」），由 CC Switch 负责切换"
            >
              注册到 CC Switch（Claude Code）
            </button>
            <button
              className="btn-outline !py-1.5 text-xs"
              onClick={() => void registerCcSwitch('codex')}
              disabled={ccBusy !== null}
              title="把网关 Responses 端点（/v1/responses）注册进 CC Switch（Trae 侧条目「AI Work 助手网关」），由 CC Switch 负责切换"
            >
              注册到 CC Switch（Codex）
            </button>
          </div>
          {ecoNote && (
            <p className="mt-2 break-all text-[11px] leading-4 text-slate-500 dark:text-zinc-400">
              {ecoNote}
            </p>
          )}
          <p className="mt-1 text-[11px] text-slate-400 dark:text-zinc-500">
            CC Switch 注册会先整库备份至 ~/.cc-switch/backups/，仅写入本网关条目、不改其它
            provider；写入后需重启 CC Switch 生效。此处注册的是 Trae 模型网关条目（默认模型 glm-5.3），
            WB 上游的 CC Switch 条目在 Buddy「API 服务」页注册，互不覆盖。
          </p>
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
              onClick={() => void loadPool(true)}
              disabled={refreshingPool}
            >
              <RefreshCw size={13} className={refreshingPool ? 'animate-spin' : ''} />
              {refreshingPool ? '刷新中…' : '刷新'}
            </button>
          </div>

          {poolAccounts.length === 0 ? (
            <p className="flex-1 py-8 text-center text-sm text-slate-400">
              暂无含通用积分的账号，请先签到或刷新积分后重试
            </p>
          ) : (
            <>
              {/* 调度策略 + 分组筛选（T10，保存后需重启 API 服务生效） */}
              <div className="mb-3 space-y-2 rounded-lg bg-slate-50 p-3 dark:bg-zinc-800/50">
                <div className="flex items-center gap-2">
                  <label className="shrink-0 text-xs text-slate-500 dark:text-zinc-400">调度策略</label>
                  <select
                    className="h-7 flex-1 rounded-md border border-slate-200 bg-white px-2 text-xs text-slate-700 outline-none focus:border-brand-400 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                    value={poolStrategy}
                    onChange={(e) => setPoolStrategy(e.target.value)}
                  >
                    <option value="expire_first">积分先过期优先（默认）</option>
                    <option value="credit_first">剩余积分多优先</option>
                    <option value="random">随机</option>
                  </select>
                </div>
                {groups.length > 0 && (
                  <div className="flex items-start gap-2">
                    <label className="shrink-0 pt-1 text-xs text-slate-500 dark:text-zinc-400">
                      分组筛选
                    </label>
                    <div className="flex flex-1 flex-wrap gap-1">
                      {groups.map((g) => {
                        const active = poolGroups.has(g.id);
                        return (
                          <button
                            key={g.id}
                            type="button"
                            className={`rounded-full border px-2 py-0.5 text-xs transition ${
                              active
                                ? 'border-brand-400 bg-brand-50 text-brand-700 dark:border-brand-500 dark:bg-brand-500/15 dark:text-brand-300'
                                : 'border-slate-200 text-slate-500 hover:border-slate-300 dark:border-zinc-700 dark:text-zinc-400 dark:hover:border-zinc-600'
                            }`}
                            onClick={() =>
                              setPoolGroups((prev) => {
                                const next = new Set(prev);
                                if (next.has(g.id)) next.delete(g.id);
                                else next.add(g.id);
                                return next;
                              })
                            }
                          >
                            {g.name}
                          </button>
                        );
                      })}
                      {poolGroups.size > 0 && (
                        <span className="pt-0.5 text-xs text-slate-400">
                          将纳入 {poolPreview.inPool} 个账号
                          {poolPreview.excluded > 0 &&
                            `，${poolPreview.excluded} 个分组外账号不参与调度`}
                        </span>
                      )}
                    </div>
                  </div>
                )}
                <p className="text-xs text-slate-400 dark:text-zinc-500">
                  分组筛选与调度策略作用于网关取号范围，保存后需重启 API 服务生效；不选分组 = 全部参与
                </p>
              </div>

              <div className="mb-3 flex items-center gap-2">
                <button
                  className="text-xs text-brand-600 hover:underline dark:text-brand-400"
                  onClick={() =>
                    setEnabledUids(new Set(poolAccounts.map((a) => a.user_id)))
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
                  已选 {enabledUids.size} / {poolAccounts.length}
                </span>
              </div>

              <div className="flex-1 space-y-1">
                {poolAccounts.map((a) => {
                  const checked = enabledUids.has(a.user_id);
                  const poolItem = poolStatus.find((p) => p.uid === a.user_id);
                  const filteredOut = !inPoolFilter(a.user_id);
                  return (
                    <label
                      key={a.user_id}
                      className={`flex cursor-pointer items-center gap-3 rounded-lg px-3 py-2 transition hover:bg-slate-50 dark:hover:bg-zinc-800/50 ${
                        filteredOut ? 'opacity-50' : ''
                      }`}
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
                        {filteredOut && <Badge tone="slate">分组外</Badge>}
                        {(a.general_credits ?? 0) > 0 && (
                          <span className="text-xs tabular-nums text-slate-500 dark:text-zinc-400">
                            {(a.general_credits ?? 0).toFixed(0)} 通用积分
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
                disabled={savingPool}
              >
                <Save size={15} />
                {savingPool ? '保存中…' : '保存账号池'}
              </button>
            </>
          )}
        </div>
      </div>

      {/* API Keys 管理（多 Key + 每日配额，改动立即生效） */}
      <div className="mt-5 card p-5">
        <div className="mb-4 flex items-center gap-2">
          <KeyRound size={18} className="text-brand-500" />
          <h2 className="text-sm font-semibold text-slate-800 dark:text-zinc-100">API Keys 管理</h2>
          <span className="hidden text-xs text-slate-400 sm:inline">
            多个 Key 独立签发并设置每日配额；增删/启停立即生效
          </span>
        </div>

        {/* 新增表单 */}
        <div className="mb-4 flex flex-wrap items-end gap-2 rounded-lg bg-slate-50 p-3 dark:bg-zinc-800/50">
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
            className="btn-primary flex items-center gap-1 !px-3 text-xs"
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
      </div>

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

      {/* 运行中池状态详情 */}
      {running && poolStatus.length > 0 && (
        <div className="mt-5 card p-5">
          <div className="mb-3 flex items-center justify-between">
            <div className="flex items-center gap-2">
              <CheckCircle2 size={18} className="text-emerald-500" />
              <h2 className="text-sm font-semibold text-slate-800 dark:text-zinc-100">
                池实时状态
              </h2>
            </div>
            <button
              className="btn-ghost flex items-center gap-1 text-xs"
              onClick={clearAllCooldowns}
              disabled={clearingCooldowns}
              title="清除所有账号的冷却状态"
            >
              <Eraser size={13} className={clearingCooldowns ? 'animate-pulse' : ''} />
              {clearingCooldowns ? '清除中…' : '清除冷却'}
            </button>
          </div>
          <div className="overflow-x-auto">
            <table className="w-full text-sm">
              <thead>
                <tr className="border-b border-slate-200 text-left text-xs text-slate-500 dark:border-zinc-700 dark:text-zinc-400">
                  <th className="pb-2 pr-4 font-medium">账号</th>
                  <th className="pb-2 pr-4 font-medium">UID</th>
                  <th className="pb-2 pr-4 font-medium">通用积分</th>
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

      {/* 用量统计（落盘数据，服务未运行也可查看） */}
      <div className="mt-5 card p-5">
        <div className="mb-4 flex flex-wrap items-center justify-between gap-2">
          <div className="flex items-center gap-2">
            <BarChart3 size={18} className="text-brand-500" />
            <h2 className="text-sm font-semibold text-slate-800 dark:text-zinc-100">用量统计</h2>
            <span className="hidden text-xs text-slate-400 sm:inline">按日落盘 · 独立于服务运行状态</span>
          </div>
          <div className="flex items-center gap-1">
            {[7, 14, 30].map((d) => (
              <button
                key={d}
                className={
                  'rounded-md px-2 py-1 text-xs transition ' +
                  (usageDays === d
                    ? 'bg-brand-500/10 font-medium text-brand-600 dark:text-brand-400'
                    : 'text-slate-500 hover:bg-slate-100 dark:text-zinc-400 dark:hover:bg-zinc-800')
                }
                onClick={() => setUsageDays(d)}
              >
                {d}天
              </button>
            ))}
            <button
              className="btn-ghost ml-1 flex items-center gap-1 text-xs"
              onClick={() => void loadUsage(usageDays)}
              disabled={usageLoading}
            >
              <RefreshCw size={13} className={usageLoading ? 'animate-spin' : ''} />
              刷新
            </button>
          </div>
        </div>

        <div className="mb-4 grid grid-cols-2 gap-3 sm:grid-cols-4">
          <StatCard
            label="总请求数"
            value={usageSummary.requests}
            tone="brand"
            hint={`近 ${usageDays} 天`}
          />
          <StatCard
            label="成功率"
            value={usageSummary.successRate === '—' ? '—' : `${usageSummary.successRate}%`}
            tone="green"
            hint={`失败 ${usageSummary.errors} 次`}
          />
          <StatCard
            label="Token 消耗"
            value={fmtTokens(usageSummary.prompt + usageSummary.completion)}
            tone="amber"
            hint={`输入 ${fmtTokens(usageSummary.prompt)} / 输出 ${fmtTokens(usageSummary.completion)}`}
          />
          <StatCard
            label="平均耗时"
            value={usageSummary.requests > 0 ? `${usageSummary.avgDuration}ms` : '—'}
            tone="blue"
            hint="按请求加权"
          />
        </div>

        {usageSummary.requests > 0 ? (
          <div className="h-56 text-slate-500 dark:text-zinc-400">
            <ResponsiveContainer width="100%" height="100%">
              <BarChart data={usageChartData} margin={{ top: 4, right: 8, bottom: 0, left: -16 }}>
                <CartesianGrid strokeDasharray="3 3" stroke="currentColor" opacity={0.15} vertical={false} />
                <XAxis dataKey="date" tick={{ fill: 'currentColor', fontSize: 11 }} tickLine={false} />
                <YAxis allowDecimals={false} tick={{ fill: 'currentColor', fontSize: 11 }} tickLine={false} />
                <Tooltip
                  contentStyle={{
                    borderRadius: 8,
                    border: '1px solid rgba(120,120,120,0.25)',
                    fontSize: 12,
                  }}
                />
                <Legend wrapperStyle={{ fontSize: 12 }} />
                <Bar dataKey="成功" stackId="s" fill="#10b981" radius={[0, 0, 0, 0]} />
                <Bar dataKey="失败" stackId="s" fill="#f43f5e" radius={[3, 3, 0, 0]} />
              </BarChart>
            </ResponsiveContainer>
          </div>
        ) : (
          <p className="py-6 text-center text-sm text-slate-400">
            暂无请求数据 — 发起一次 API 调用后这里会展示按日趋势
          </p>
        )}

        {usageSummary.topModels.length > 0 && (
          <div className="mt-4">
            <p className="mb-2 text-xs font-medium text-slate-500 dark:text-zinc-400">
              模型分布（近 {usageDays} 天 Top 5）
            </p>
            <div className="overflow-x-auto">
              <table className="w-full text-sm">
                <thead>
                  <tr className="border-b border-slate-200 text-left text-xs text-slate-500 dark:border-zinc-700 dark:text-zinc-400">
                    <th className="pb-2 pr-4 font-medium">模型</th>
                    <th className="pb-2 pr-4 font-medium">请求数</th>
                    <th className="pb-2 pr-4 font-medium">成功</th>
                    <th className="pb-2 pr-4 font-medium">失败</th>
                    <th className="pb-2 font-medium">占比</th>
                  </tr>
                </thead>
                <tbody>
                  {usageSummary.topModels.map((m) => {
                    const pct = usageSummary.requests > 0 ? (m.requests / usageSummary.requests) * 100 : 0;
                    return (
                      <tr
                        key={m.name}
                        className="border-b border-slate-100 last:border-0 dark:border-zinc-800"
                      >
                        <td className="py-2 pr-4 font-mono text-xs font-medium text-slate-700 dark:text-zinc-200">
                          {m.name}
                        </td>
                        <td className="py-2 pr-4 tabular-nums text-slate-600 dark:text-zinc-300">{m.requests}</td>
                        <td className="py-2 pr-4 tabular-nums text-emerald-600 dark:text-emerald-400">{m.ok}</td>
                        <td className="py-2 pr-4 tabular-nums text-rose-600 dark:text-rose-400">{m.errors}</td>
                        <td className="w-40 py-2">
                          <div className="flex items-center gap-2">
                            <div className="h-1.5 flex-1 overflow-hidden rounded-full bg-slate-100 dark:bg-zinc-800">
                              <div
                                className="h-full rounded-full bg-brand-500"
                                style={{ width: `${Math.min(100, pct)}%` }}
                              />
                            </div>
                            <span className="w-12 shrink-0 text-right text-xs tabular-nums text-slate-400">
                              {pct.toFixed(1)}%
                            </span>
                          </div>
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
