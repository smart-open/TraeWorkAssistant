/**
 * 全局 API 管理 · API Keys 管理（unified-api-gateway-design §5.2/§5.3）
 * 自 ApiService.tsx 原样搬移（props 化），行为不变：多 Key 签发 / 每日配额 / 鉴权开关 /
 * 子 Key 调度配置（F-35）。数据源：api_keys_list / api_keys_save（改动立即生效）。
 * 子弹框（子 Key 配置/删除确认）沿用 Modal 组件叠加；onSubModalChange 供主弹窗
 * 在子弹框打开期间屏蔽 ESC 双关（主弹窗 onClose 先于子弹框触发）。
 */
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Check, Copy, KeyRound, Plus, Power, RefreshCw, Route, Trash2 } from 'lucide-react';
import { Badge, Modal } from '../ui';
import { api } from '../../lib/tauri';
import { copyText } from '../../lib/clipboard';
import { useAppStore } from '../../store';
import { maskApiKey, fmtTokens } from '../../lib/format';
import { isPoolVisible } from '../../lib/appVisibility';
import type { ApiKeyEntry, GroupView, PoolStatus, TagView, UsageDayView } from '../../types';

/** issue #30 混合白名单：候选账号平台标记 */
type CandidatePool = 'trae' | 'buddy' | 'buddy-ai' | 'qoder';
/** 调度配置候选：上游账号 + 所属池标记 */
interface PoolCandidate extends PoolStatus {
  pool: CandidatePool;
}

/** 池标记 → 徽标/文案映射（候选列表与专一/限定上游文案共用） */
const POOL_TAG: Record<CandidatePool, { label: string; tone: 'blue' | 'green' | 'violet' | 'cyan' }> = {
  trae: { label: 'Trae', tone: 'blue' },
  buddy: { label: 'Buddy', tone: 'green' },
  'buddy-ai': { label: 'BuddyAI', tone: 'cyan' },
  qoder: { label: 'Qoder', tone: 'violet' },
};
/** 绑定池 → 展示文案（空串由调用方处理为跨池文案） */
const bindPoolLabel = (p: string) =>
  p === 'trae'
    ? 'Trae 池'
    : p === 'buddy'
      ? 'Buddy 池'
      : p === 'buddy-ai'
        ? 'BuddyAI 国际池'
        : p === 'qoder'
          ? 'Qoder 池'
          : p === 'custom'
            ? '自定义上游池'
            : 'Trae/Buddy/Qoder/BuddyAI/自定义';

export default function ApiKeysManager({
  onSubModalChange,
}: {
  /** 子弹框（子 Key 配置/删除确认）开关状态上报（主弹窗据此屏蔽 ESC 双关） */
  onSubModalChange?: (open: boolean) => void;
}) {
  const toast = useAppStore((s) => s.pushToast);
  // 系统设置「应用显示」联动（隐藏不停功能：存量 Key 的隐藏池绑定/白名单仍可管理）
  const settings = useAppStore((s) => s.settings);
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
  // 共享分组/标签绑定（P2 需求④，D4/D5）：运行时实时展开 + 组∩标签交集
  const [editGroups, setEditGroups] = useState<Set<string>>(new Set());
  const [editTags, setEditTags] = useState<Set<string>>(new Set());
  const [sharedGroups, setSharedGroups] = useState<GroupView[]>([]);
  const [sharedTags, setSharedTags] = useState<TagView[]>([]);
  // 覆盖账号数预览（实时展开计算；total=0 强提示）
  const [orgPreview, setOrgPreview] = useState<Record<string, number> | null>(null);
  // key 列表行的覆盖账号数批量预览（仅对配置了组/标签的 key 计算）
  const [listOrgPreview, setListOrgPreview] = useState<Record<string, Record<string, number>>>({});
  // issue #25 资源池绑定："" = 跟随全局调度 | "trae" | "buddy" | "buddy-ai" | "qoder"
  const [editBindPool, setEditBindPool] = useState('');
  // issue #30：打开弹框时的原始白名单/专一值（未归一）。保存时若用户未改动勾选
  // （归一化后等价），写回原值——旧裸 uid 数据保持旧语义，避免「仅打开保存一次」
  // 就把 trae 侧从「不限」静默升级为「排除」（混合白名单零条目池 = 排除）
  const editOrigRef = useRef<{ allowed: string[]; dedicated: string }>({ allowed: [], dedicated: '' });
  // 新增 Key 时的资源池选择（issue #25）
  const [newBindPool, setNewBindPool] = useState('');
  const [deleteForKey, setDeleteForKey] = useState<ApiKeyEntry | null>(null);
  // F8：关闭鉴权二次确认（全局安全开关，误触即放行所有无 Key 请求；重新开启不确认）
  const [authConfirmOpen, setAuthConfirmOpen] = useState(false);
  // 子 Key 配置候选（四池上游账号；打开弹框时刷新）：
  // poolStatus = Trae 池（invoke pool_status），buddyPoolStatus = Buddy/WB 池，
  // buddyAiPoolStatus = BuddyAI 国际池，qoderPoolStatus = Qoder 池（fail-open 全量含凭证账号）
  const [poolStatus, setPoolStatus] = useState<PoolStatus[]>([]);
  const [buddyPoolStatus, setBuddyPoolStatus] = useState<PoolStatus[]>([]);
  const [buddyAiPoolStatus, setBuddyAiPoolStatus] = useState<PoolStatus[]>([]);
  const [qoderPoolStatus, setQoderPoolStatus] = useState<PoolStatus[]>([]);
  // 今日按 Key 的 token 用量（「今日已用」列展示；四桶分账记账，跨桶合并后按 Key 聚合）
  const [usage, setUsage] = useState<UsageDayView[][]>([]);
  // Qoder 上游开关（对齐方案 §5.3 警告徽标数据源，已交付归档）：
  // Key 绑定 Qoder 池但未启用时显式告警；null = 未知（不告警）
  const [qoderEnabled, setQoderEnabled] = useState<boolean | null>(null);
  // BuddyAI 国际上游开关（同上口径）：Key 绑定 buddy-ai 池但未启用时显式告警
  const [buddyAiEnabled, setBuddyAiEnabled] = useState<boolean | null>(null);

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

  const saveKeys = async (next: ApiKeyEntry[], msg: string, nextAuthDisabled?: boolean): Promise<boolean> => {
    setKeysSaving(true);
    try {
      await api.apiServer.keysSave(next, nextAuthDisabled);
      setApiKeys(next);
      if (nextAuthDisabled !== undefined) setAuthDisabled(nextAuthDisabled);
      toast('success', msg);
      return true;
    } catch (e) {
      toast('error', `保存 Key 失败：${String(e).slice(0, 120)}`);
      return false;
    } finally {
      setKeysSaving(false);
    }
  };

  const toggleAuthDisabled = () => {
    const next = !authDisabled;
    // F8：关闭鉴权 = 放行所有无 Key 请求的全局安全开关，补确认 Modal；重新开启不确认
    if (next) {
      setAuthConfirmOpen(true);
      return;
    }
    void saveKeys(apiKeys, '已开启鉴权：未配置启用 Key 时请求将被拒绝', next);
  };

  /** F8：确认关闭鉴权（二次确认后实际写盘生效） */
  const confirmAuthDisable = () => {
    setAuthConfirmOpen(false);
    void saveKeys(
      apiKeys,
      '已关闭鉴权：无启用 Key 时任何本机程序均可调用（不推荐）',
      true,
    );
  };

  const addKey = async () => {
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
    // 新增 Key 的资源池选择：选中池所属应用被隐藏时回落全局（新增不选隐藏池）
    const effNewBindPool =
      newBindPool === '' || isPoolVisible(settings, newBindPool) ? newBindPool : '';
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
      bind_pool: effNewBindPool,
      allowed_groups: [],
      allowed_tags: [],
      daily_stats: [],
    };
    // 保存成功后才清空表单：失败时保留用户已填的名称/限额，免重填
    const ok = await saveKeys([...apiKeys, entry], `Key「${name}」已添加`);
    if (ok) {
      setNewKeyName('');
      setNewKeyLimit(0);
      setNewBindPool('');
      generateKeyValue();
    }
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

  // 子 Key 配置弹框（F-35 + issue #25 资源池绑定）：限定上游 + 专一/临期优先 + 按日统计展示
  const openKeyEdit = (k: ApiKeyEntry) => {
    setEditKey(k);
    // issue #30：bind="" 旧数据裸 uid 显示时归一为 buddy: 前缀（与候选编码对齐，
    // UI 勾选状态与实际限定一致）；原值存入 editOrigRef，保存时未改动则写回原值
    const norm = (v: string) =>
      !k.bind_pool && v && !/^(trae|buddy|buddy-ai|qoder|custom):/i.test(v) ? `buddy:${v}` : v;
    editOrigRef.current = { allowed: k.allowed_accounts, dedicated: k.dedicated_account || '' };
    setEditAllowed(new Set(k.allowed_accounts.map(norm)));
    setEditMode(k.schedule_mode || 'expire_first');
    setEditDedicated(norm(k.dedicated_account || ''));
    setEditBindPool(k.bind_pool || '');
    setEditGroups(new Set(k.allowed_groups ?? []));
    setEditTags(new Set(k.allowed_tags ?? []));
    // 共享分组/标签候选（全局一份；含各池成员计数）
    // F3（审查修复清单 v1.4.9）：弹框候选列表加载失败不再静默吞掉——区域空白
    // 无解释会让用户以为无分组/标签可选，明示失败并引导重开弹框重试
    api.groups
      .sharedList()
      .then(setSharedGroups)
      .catch(() => toast('error', '分组候选加载失败：请关闭弹框后重新打开重试'));
    api.tags
      .list()
      .then(setSharedTags)
      .catch(() => toast('error', '标签候选加载失败：请关闭弹框后重新打开重试'));
    // 服务可能刚启动，四池候选列表即时刷新
    api.apiServer
      .poolStatus()
      .then(setPoolStatus)
      .catch(() => {
        /* 保留上次候选 */
      });
    api.apiServer
      .wbPoolStatus()
      .then(setBuddyPoolStatus)
      .catch(() => {
        /* 保留上次候选 */
      });
    api.apiServer
      .buddyAiPoolStatus()
      .then(setBuddyAiPoolStatus)
      .catch(() => {
        /* 保留上次候选 */
      });
    api.apiServer
      .qoderPoolStatus()
      .then(setQoderPoolStatus)
      .catch(() => {
        /* 保留上次候选 */
      });
  };

  const confirmKeyEdit = () => {
    if (!editKey) return;
    // issue #30：归一化比较——用户未改动白名单/专一（仅打开弹框触发的前缀归一）
    // 时写回原值，旧裸 uid 保持旧语义（trae 不限）；改动过才落前缀编码（升级语义）
    const stripTag = (v: string) => v.replace(/^(trae|buddy|buddy-ai|qoder):/i, '');
    const normSig = (arr: string[]) =>
      arr
        .map(stripTag)
        .sort()
        .join('\n');
    const allowedChanged = normSig([...editAllowed]) !== normSig(editOrigRef.current.allowed);
    const dedicatedChanged = stripTag(editDedicated) !== stripTag(editOrigRef.current.dedicated);
    const next = apiKeys.map((k) =>
      k.id === editKey.id
        ? {
            ...k,
            allowed_accounts: allowedChanged ? [...editAllowed] : editOrigRef.current.allowed,
            schedule_mode: editMode,
            dedicated_account:
              editMode === 'dedicated' ? (dedicatedChanged ? editDedicated : editOrigRef.current.dedicated) : '',
            bind_pool: editBindPool,
            allowed_groups: [...editGroups],
            allowed_tags: [...editTags],
          }
        : k,
    );
    void saveKeys(next, `Key「${editKey.name}」调度配置已更新`);
    setEditKey(null);
  };

  // 覆盖账号数实时预览（D4/D5）：effective = 精确白名单 ∪ (组 ∩ 标签)；组/标签
  // 均未勾选时不显示。后端 org_index 实时展开，成员变化即时生效
  // 列表行批量预览：仅对配置了组/标签的 key 调用（一次批量，避免 N 次 invoke）
  // F4：失败置错误态（行内显示 -- + 点击重试），不再永久停留 '…' 占位
  const [listPreviewError, setListPreviewError] = useState(false);
  const [listPreviewTick, setListPreviewTick] = useState(0);
  useEffect(() => {
    const bound = apiKeys.filter((k) => (k.allowed_groups?.length ?? 0) > 0 || (k.allowed_tags?.length ?? 0) > 0);
    if (bound.length === 0) {
      setListOrgPreview({});
      setListPreviewError(false);
      return;
    }
    let cancelled = false;
    api.apiServer
      .keysOrgPreview(bound)
      .then((r) => {
        if (!cancelled) {
          setListOrgPreview(r);
          setListPreviewError(false);
        }
      })
      .catch(() => {
        if (!cancelled) {
          setListOrgPreview({});
          setListPreviewError(true);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [apiKeys, listPreviewTick]);
  useEffect(() => {
    if (!editKey) {
      setOrgPreview(null);
      return;
    }
    if (editGroups.size === 0 && editTags.size === 0) {
      setOrgPreview(null);
      return;
    }
    const entry: ApiKeyEntry = {
      ...editKey,
      allowed_accounts: [...editAllowed],
      allowed_groups: [...editGroups],
      allowed_tags: [...editTags],
    };
    let cancelled = false;
    api.apiServer
      .keysOrgPreview([entry])
      .then((r) => {
        if (!cancelled) setOrgPreview(r[entry.id] ?? null);
      })
      .catch(() => {
        if (!cancelled) setOrgPreview(null);
      });
    return () => {
      cancelled = true;
    };
  }, [editKey, editGroups, editTags, editAllowed]);

  const updateKeyLimit = (id: string, limit: number, revert?: () => void) => {
    const v = Math.max(0, Math.floor(limit) || 0);
    const cur = apiKeys.find((k) => k.id === id);
    if (!cur || cur.daily_limit === v) return;
    void saveKeys(apiKeys.map((k) => (k.id === id ? { ...k, daily_limit: v } : k)), '限额已更新').then(
      (ok) => {
        // 保存失败（uncontrolled 输入框已显示新值）→ 回写原值，保持显示与持久化一致
        if (!ok) revert?.();
      },
    );
  };

  // 复制成功按钮级反馈（不依赖 toast，通知方式设为 none/system 时也可见）
  const [copiedId, setCopiedId] = useState<string | null>(null);

  const copyKeyValue = async (k: ApiKeyEntry) => {
    const ok = await copyText(k.key);
    if (ok) {
      setCopiedId(k.id);
      setTimeout(() => setCopiedId((cur) => (cur === k.id ? null : cur)), 1500);
      toast('success', 'Key 已复制到剪贴板');
    } else {
      toast('error', '复制失败');
    }
  };

  useEffect(() => {
    void loadKeys();
    generateKeyValue();
    // 上游账号候选（服务未运行时为空列表）：Trae 池 + Buddy 池 + BuddyAI 国际池 + Qoder 池
    api.apiServer
      .poolStatus()
      .then(setPoolStatus)
      .catch(() => {
        /* 保留空列表 */
      });
    api.apiServer
      .wbPoolStatus()
      .then(setBuddyPoolStatus)
      .catch(() => {
        /* 保留空列表 */
      });
    api.apiServer
      .buddyAiPoolStatus()
      .then(setBuddyAiPoolStatus)
      .catch(() => {
        /* 保留空列表 */
      });
    api.apiServer
      .qoderPoolStatus()
      .then(setQoderPoolStatus)
      .catch(() => {
        /* 保留空列表 */
      });
    // 今日 token 用量（读落盘数据，仅取当日条目）：五桶并拉——key_tokens 按池分账
    // （Trae/Buddy/BuddyAI 国际/Qoder/Custom 各自的 DayStats），只读 Trae 桶会漏记
    // 非 Trae 流量的 Key 用量
    Promise.all([
      api.apiServer.usageStats(14).catch(() => [] as UsageDayView[]),
      api.apiServer.wbUsageStats(14).catch(() => [] as UsageDayView[]),
      api.apiServer.buddyAiUsageStats(14).catch(() => [] as UsageDayView[]),
      api.apiServer.customUsageStats(14).catch(() => [] as UsageDayView[]),
      api.apiServer.qoderUsageStats(14).catch(() => [] as UsageDayView[]),
    ]).then(([trae, wb, buddyAi, custom, qoder]) => setUsage([trae, wb, buddyAi, custom, qoder]));
    // Qoder / BuddyAI 国际上游开关（Key 绑定对应池的警告徽标数据源）
    api.apiServer
      .poolList()
      .then((p) => {
        setQoderEnabled(p.qoder_enabled ?? false);
        setBuddyAiEnabled(p.buddy_ai_enabled ?? false);
      })
      .catch(() => {
        /* 未知状态不告警 */
      });
  }, [loadKeys, generateKeyValue]);

  // 子弹框开关状态上报（供主弹窗屏蔽 ESC 双关）
  useEffect(() => {
    onSubModalChange?.(editKey != null || deleteForKey != null || authConfirmOpen);
  }, [editKey, deleteForKey, authConfirmOpen, onSubModalChange]);

  // 今日按 Key 的 token 用量（Keys 表「今日已用」并列展示；日期口径与后端一致 = 本地时区 YYYY-MM-DD）
  // 五桶合并：同一 Key 的请求可能落 Trae/Buddy/BuddyAI/Qoder/Custom 任一桶，按 Key 名跨桶求和
  const todayKey = new Date().toLocaleDateString('sv-SE');
  const todayKeyTokens = useMemo(() => {
    const m = new Map<string, { prompt: number; completion: number }>();
    for (const bucket of usage) {
      bucket
        .find((d) => d.date === todayKey)
        ?.key_tokens.forEach((t) => {
          const ex = m.get(t.name) ?? { prompt: 0, completion: 0 };
          ex.prompt += t.prompt_tokens;
          ex.completion += t.completion_tokens;
          m.set(t.name, ex);
        });
    }
    return m;
  }, [usage, todayKey]);

  // issue #25 资源池绑定 + issue #30 混合白名单：限定上游/专一候选按绑定池切换数据源。
  // 跟随全局调度（bind=""）合并展示 Trae + Buddy + BuddyAI 国际 + Qoder 四池账号并标记
  // 平台，可跨池勾选；绑定池时仅展示对应池（trae → Trae 池；buddy → Buddy 池；
  // buddy-ai → BuddyAI 国际池；qoder → Qoder 池）。
  // 修复历史问题：原候选列表拉的是 Trae 池，而后端白名单只作用于 WB 池
  const editCandidates: PoolCandidate[] = useMemo(() => {
    const tag = (pool: CandidatePool, list: PoolStatus[]) => list.map((p) => ({ ...p, pool }));
    if (editBindPool === 'trae') return tag('trae', poolStatus);
    if (editBindPool === 'buddy') return tag('buddy', buddyPoolStatus);
    if (editBindPool === 'buddy-ai') return tag('buddy-ai', buddyAiPoolStatus);
    if (editBindPool === 'qoder') return tag('qoder', qoderPoolStatus);
    return [
      ...tag('trae', poolStatus),
      ...tag('buddy', buddyPoolStatus),
      ...tag('buddy-ai', buddyAiPoolStatus),
      ...tag('qoder', qoderPoolStatus),
    ];
  }, [editBindPool, poolStatus, buddyPoolStatus, buddyAiPoolStatus, qoderPoolStatus]);

  // issue #30 编码：跟随全局调度时白名单/专一值携带池前缀（trae:/buddy:），
  // 后端按前缀分池作用域；绑定池时存裸 uid（与旧格式一致）
  const encodeUid = useCallback(
    (pool: CandidatePool, uid: string) => (editBindPool === '' ? `${pool}:${uid}` : uid),
    [editBindPool],
  );

  // 可见池名串（限定上游提示文案用；全部隐藏时兜底占位）
  const visiblePoolsText =
    (['trae', 'buddy', 'qoder', 'buddy-ai'] as CandidatePool[])
      .filter((p) => isPoolVisible(settings, p))
      .map((p) => POOL_TAG[p].label)
      .join(' + ') || '无可见池';

  const switchBindPool = (p: string) => {
    if (p === editBindPool) return;
    setEditBindPool(p);
    // 切换池清空已勾选账号与专一账号（uid 体系不同）
    setEditAllowed(new Set());
    setEditDedicated('');
  };

  return (
    <div className="card p-4">
      <div className="mb-3 flex items-center gap-2">
        <KeyRound size={16} className="text-brand-500" />
        <h3 className="text-sm font-semibold text-slate-800 dark:text-zinc-100">API Keys 管理</h3>
        <span className="hidden text-xs text-slate-400 sm:inline">
          多个 Key 独立签发并设置每日配额；增删/启停立即生效
        </span>
      </div>

      {/* 新增表单（单行紧凑布局：Key 值定宽不占满，资源池按钮自适应，不折叠换行；
          窄窗口横向滚动兜底，不裁剪控件） */}
      <div className="mb-3 flex flex-nowrap items-end gap-2 overflow-x-auto rounded-lg bg-slate-50 p-3 dark:bg-zinc-800/50">
        <div className="w-32 shrink-0">
          <label className="mb-1 block text-xs text-slate-500 dark:text-zinc-400">名称</label>
          <input
            className="input"
            placeholder="如：cli / 小工具"
            value={newKeyName}
            onChange={(e) => setNewKeyName(e.target.value)}
          />
        </div>
        <div className="w-64 shrink-0">
          <label className="mb-1 block text-xs text-slate-500 dark:text-zinc-400">Key 值</label>
          <input
            className="input font-mono text-xs"
            value={newKeyValue}
            onChange={(e) => setNewKeyValue(e.target.value)}
          />
        </div>
        <button
          className="btn-ghost flex shrink-0 items-center gap-1 !p-2 text-xs"
          onClick={generateKeyValue}
          title="重新生成 Key 值"
        >
          <RefreshCw size={13} />
          重新生成
        </button>
        <div className="w-32 shrink-0">
          <label
            className="mb-1 block text-xs text-slate-500 dark:text-zinc-400"
            title="每日请求次数上限，0 = 不限"
          >
            日限额/次
          </label>
          <input
            type="number"
            min={0}
            className="input"
            title="0 = 不限"
            value={newKeyLimit}
            onChange={(e) => setNewKeyLimit(parseInt(e.target.value) || 0)}
          />
        </div>
        <div className="shrink-0">
          <label className="mb-1 block whitespace-nowrap text-xs text-slate-500 dark:text-zinc-400">
            资源池<span className="ml-1 font-normal text-slate-400">（优先走所选池）</span>
          </label>
          <div className="flex gap-1">
            {[
              { key: '', label: '全局', title: '不绑定，按系统策略选池' },
              { key: 'trae', label: 'Trae', title: '优先走 Trae，异常可回退' },
              { key: 'buddy', label: 'Buddy', title: '优先走 Buddy，异常可回退' },
              { key: 'buddy-ai', label: 'BuddyAI', title: '优先走 BuddyAI 国际池，异常可回退' },
              { key: 'qoder', label: 'Qoder', title: '优先走 Qoder，异常可回退' },
              { key: 'custom', label: '自定义', title: '优先走自定义上游组，异常可回退' },
              // 应用显隐联动：新增 Key 不提供隐藏应用的池选项
            ].filter((p) => p.key === '' || isPoolVisible(settings, p.key)).map((p) => (
              <button
                key={p.key}
                className={`shrink-0 whitespace-nowrap rounded-lg border px-2 py-1.5 text-xs font-medium transition-colors ${
                  newBindPool === p.key
                    ? 'border-indigo-400 bg-indigo-50 text-indigo-600 dark:border-indigo-500 dark:bg-indigo-500/10 dark:text-indigo-300'
                    : 'border-slate-200 text-slate-500 hover:border-slate-300 dark:border-zinc-700 dark:text-zinc-400'
                }`}
                onClick={() => setNewBindPool(p.key)}
                title={p.title}
              >
                {p.label}
              </button>
            ))}
          </div>
        </div>
        <button
          className="btn-outline flex shrink-0 items-center gap-1 !px-3 text-xs"
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
                          const input = e.target;
                          updateKeyLimit(k.id, parseInt(raw, 10), () => {
                            input.value = String(k.daily_limit);
                          });
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
                      {/* issue #25 资源池绑定徽标 */}
                      {k.bind_pool === 'trae' && <Badge tone="blue" className="ml-1">Trae 池</Badge>}
                      {k.bind_pool === 'buddy' && <Badge tone="green" className="ml-1">Buddy 池</Badge>}
                      {k.bind_pool === 'buddy-ai' && <Badge tone="cyan" className="ml-1">BuddyAI 国际池</Badge>}
                      {k.bind_pool === 'qoder' && <Badge tone="violet" className="ml-1">Qoder 池</Badge>}
                      {/* §5.3 保存后校验提示：绑定 Qoder 池但上游未启用 → 警告徽标 */}
                      {k.bind_pool === 'qoder' && qoderEnabled === false && (
                        <span
                          className="ml-1 inline-flex align-middle"
                          title="Qoder 上游未启用：该 Key 的请求将显式报错；开启入口在 Qoder「资源调度」页或全局 API 管理·调度策略中心"
                        >
                          <Badge tone="amber">上游未启用</Badge>
                        </span>
                      )}
                      {/* 同上口径：绑定 BuddyAI 国际池但上游未启用 → 警告徽标 */}
                      {k.bind_pool === 'buddy-ai' && buddyAiEnabled === false && (
                        <span
                          className="ml-1 inline-flex align-middle"
                          title="BuddyAI 国际上游未启用：该 Key 的请求将显式报错；开启入口在 BuddyAI 国际「资源调度」页"
                        >
                          <Badge tone="amber">上游未启用</Badge>
                        </span>
                      )}
                      {k.allowed_accounts.length > 0 && (
                        <span className="ml-1 text-xs text-slate-400" title={k.allowed_accounts.join(', ')}>
                          限{k.allowed_accounts.length}账号
                        </span>
                      )}
                      {/* P2 文档补齐：key 列表展示组/标签覆盖账号数预览（实时展开计算） */}
                      {((k.allowed_groups?.length ?? 0) > 0 || (k.allowed_tags?.length ?? 0) > 0) && (
                        listPreviewError ? (
                          // F4：预览失败显示 -- 并提供点击重试（tick 自增重拉），不停留 '…'
                          <button
                            className="ml-1 text-xs text-slate-400 underline decoration-dotted"
                            title="覆盖账号数预览失败，点击重试"
                            onClick={() => setListPreviewTick((t) => t + 1)}
                          >
                            覆盖--账号（点击重试）
                          </button>
                        ) : (
                          <span
                            className={`ml-1 text-xs ${listOrgPreview[k.id]?.total === 0 ? 'font-medium text-rose-500' : 'text-slate-400'}`}
                            title={`组/标签实时展开覆盖 ${listOrgPreview[k.id]?.total ?? '…'} 个账号（组 ∩ 标签，D5）`}
                          >
                            覆盖{listOrgPreview[k.id]?.total ?? '…'}账号
                            {listOrgPreview[k.id]?.total === 0 && ' ⚠ 空集'}
                          </span>
                        )
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
                          title={copiedId === k.id ? '已复制' : '复制完整 Key'}
                          onClick={() => void copyKeyValue(k)}
                        >
                          {copiedId === k.id ? (
                            <Check size={14} className="text-emerald-500" />
                          ) : (
                            <Copy size={14} />
                          )}
                        </button>
                        <button
                          className="btn-ghost !p-1.5"
                          title="调度配置（限定上游 / 专一 / 临期优先）"
                          onClick={() => openKeyEdit(k)}
                        >
                          <Route size={14} />
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
        widthClass="max-w-[42.7rem]"
        footer={
          <>
            <button className="btn-outline" onClick={() => setEditKey(null)}>取消</button>
            <button className="btn-primary" onClick={confirmKeyEdit} disabled={keysSaving}>保存</button>
          </>
        }
      >
        <div className="space-y-4 text-sm">
          <div>
            <div className="mb-1.5 text-xs font-medium text-slate-500">
              资源池<span className="ml-1 font-normal text-slate-400">（绑定后该 Key 的请求优先走所选池）</span>
            </div>
            <div className="flex gap-2">
              {[
                { key: '', label: '跟随全局调度', desc: '不绑定，按系统策略选池' },
                { key: 'trae', label: 'Trae 池', desc: '优先走 Trae，异常可回退' },
                { key: 'buddy', label: 'Buddy 池', desc: '优先走 Buddy，异常可回退' },
                { key: 'buddy-ai', label: 'BuddyAI 池', desc: '优先走 BuddyAI，异常可回退' },
                { key: 'qoder', label: 'Qoder 池', desc: '优先走 Qoder，异常可回退' },
                { key: 'custom', label: '自定义上游池', desc: '优先走自定义上游组，异常可回退' },
                // 应用显隐联动：保留全量按钮（存量 bind_pool 指向隐藏池时需有选中态），
                // 隐藏池加「已隐藏」徽标——隐藏不停功能、不删数据
              ].map((p) => (
                <button
                  key={p.key}
                  className={`flex-1 rounded-lg border p-2.5 text-left text-xs ${editBindPool === p.key ? 'border-indigo-400 bg-indigo-50 dark:bg-indigo-500/10' : 'border-slate-200 dark:border-zinc-700'}`}
                  onClick={() => switchBindPool(p.key)}
                >
                  <div className="font-medium">
                    {p.label}
                    {p.key !== '' && !isPoolVisible(settings, p.key) && (
                      <Badge tone="slate" className="ml-1 !px-1.5 !text-[10px]">已隐藏</Badge>
                    )}
                  </div>
                  <div className="mt-0.5 text-slate-400">{p.desc}</div>
                </button>
              ))}
            </div>
            {/* §5.3：绑定 Qoder 池但上游未启用 → 弹框内即时提示 */}
            {editBindPool === 'qoder' && qoderEnabled === false && (
              <p className="mt-1.5 text-xs text-amber-500">
                Qoder 上游当前未启用：保存后该 Key 的请求将显式报错；请先到 Qoder「资源调度」页
                或上方「调度策略中心」开启「启用 Qoder 上游」。
              </p>
            )}
            {/* 同上口径：绑定 BuddyAI 国际池但上游未启用 → 弹框内即时提示 */}
            {editBindPool === 'buddy-ai' && buddyAiEnabled === false && (
              <p className="mt-1.5 text-xs text-amber-500">
                BuddyAI 国际上游当前未启用：保存后该 Key 的请求将显式报错；请先到
                BuddyAI 国际「资源调度」页开启「启用 BuddyAI 国际上游」。
              </p>
            )}
          </div>
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
              <span className="mb-1 block text-xs font-medium text-slate-500">
                专一账号
                <span className="ml-1 font-normal text-slate-400">
                  （{editBindPool ? bindPoolLabel(editBindPool) : visiblePoolsText.replace(/ \+ /g, '/')}账号）
                </span>
              </span>
              <select className="input w-full" value={editDedicated} onChange={(e) => setEditDedicated(e.target.value)}>
                <option value="">— 默认取限定上游首个 —</option>
                {editCandidates.map((c) => (
                  <option key={encodeUid(c.pool, c.uid)} value={encodeUid(c.pool, c.uid)}>
                    [{POOL_TAG[c.pool].label}] {c.name || c.uid}
                    {!isPoolVisible(settings, c.pool) ? '（已隐藏池）' : ''}
                    {c.credits != null ? `（${c.credits.toFixed(1)} 积分）` : ''}
                  </option>
                ))}
              </select>
              {editCandidates.length === 0 && (
                <span className="mt-1 block text-xs text-amber-500">服务未运行，暂无上游账号候选；可保存后稍后调整。</span>
              )}
            </label>
          )}
          <div>
            <div className="mb-1.5 text-xs font-medium text-slate-500">
              限定上游
              <span className="ml-1 font-normal text-slate-400">
                （{editBindPool ? `${bindPoolLabel(editBindPool)}账号` : `${visiblePoolsText}账号`}
                ；不勾选 = 使用全部上游账号）
              </span>
            </div>
            <div className="max-h-40 space-y-1 overflow-y-auto rounded-lg border border-slate-200 p-2 dark:border-zinc-700">
              {editCandidates.length === 0 ? (
                <div className="py-2 text-center text-xs text-slate-400">服务未运行，暂无上游账号候选</div>
              ) : (
                editCandidates.map((c) => {
                  const val = encodeUid(c.pool, c.uid);
                  return (
                    <label key={val} className="flex items-center gap-2 text-xs">
                      <input
                        type="checkbox"
                        checked={editAllowed.has(val)}
                        onChange={() => {
                          const next = new Set(editAllowed);
                          if (next.has(val)) next.delete(val);
                          else next.add(val);
                          setEditAllowed(next);
                        }}
                      />
                      <Badge tone={POOL_TAG[c.pool].tone} className="!px-1.5 !text-[10px]">
                        {POOL_TAG[c.pool].label}
                      </Badge>
                      {!isPoolVisible(settings, c.pool) && (
                        <Badge tone="slate" className="!px-1.5 !text-[10px]">已隐藏</Badge>
                      )}
                      <span className="truncate">{c.name || c.uid}</span>
                      {c.credits != null && <span className="ml-auto tabular-nums text-slate-400">{c.credits.toFixed(1)}</span>}
                    </label>
                  );
                })
              )}
            </div>
          </div>
          {/* 共享分组/标签绑定（P2 需求④，D5 交集语义）：绑定组织维度而非具体账号，
              分组成员变化无需改 Key 即时生效；与上方精确白名单取并集 */}
          <p className="text-xs text-slate-400">
            精确白名单与分组/标签为「或」关系：账号只要命中其一即可被调度
            （生效范围 = 精确白名单 ∪ 组∩标签展开）。不勾选分组/标签 = 不启用该维度。
          </p>
          <div className="grid grid-cols-2 gap-3">
            <div>
              <div className="mb-1.5 text-xs font-medium text-slate-500">
                分组
                <span className="ml-1 font-normal text-slate-400">（不勾选 = 不限）</span>
              </div>
              <div className="max-h-32 space-y-1 overflow-y-auto rounded-lg border border-slate-200 p-2 dark:border-zinc-700">
                {sharedGroups.length === 0 ? (
                  <div className="py-1 text-center text-xs text-slate-400">暂无共享分组</div>
                ) : (
                  sharedGroups.map((g) => (
                    <label key={g.id} className="flex items-center gap-2 text-xs">
                      <input
                        type="checkbox"
                        checked={editGroups.has(g.id)}
                        onChange={() => {
                          const next = new Set(editGroups);
                          if (next.has(g.id)) next.delete(g.id);
                          else next.add(g.id);
                          setEditGroups(next);
                        }}
                      />
                      <span className="inline-block h-2.5 w-2.5 shrink-0 rounded-full" style={{ background: g.color }} />
                      <span className="truncate">{g.name}</span>
                      <span className="ml-auto text-slate-400">{g.count}</span>
                    </label>
                  ))
                )}
              </div>
            </div>
            <div>
              <div className="mb-1.5 text-xs font-medium text-slate-500">
                标签
                <span className="ml-1 font-normal text-slate-400">（不勾选 = 不限）</span>
              </div>
              <div className="max-h-32 space-y-1 overflow-y-auto rounded-lg border border-slate-200 p-2 dark:border-zinc-700">
                {sharedTags.length === 0 ? (
                  <div className="py-1 text-center text-xs text-slate-400">暂无共享标签</div>
                ) : (
                  sharedTags.map((t) => (
                    <label key={t.id} className="flex items-center gap-2 text-xs">
                      <input
                        type="checkbox"
                        checked={editTags.has(t.id)}
                        onChange={() => {
                          const next = new Set(editTags);
                          if (next.has(t.id)) next.delete(t.id);
                          else next.add(t.id);
                          setEditTags(next);
                        }}
                      />
                      <span className="inline-block h-2.5 w-2.5 shrink-0 rounded-full" style={{ background: t.color }} />
                      <span className="truncate">{t.name}</span>
                      <span className="ml-auto text-slate-400">{t.count}</span>
                    </label>
                  ))
                )}
              </div>
            </div>
          </div>
          {editGroups.size > 0 && editTags.size > 0 && (
            <p className="text-xs text-sky-500">
              分组与标签同时勾选时，取「既属于所选分组、又带有所选标签」的账号（交集）。
            </p>
          )}
          {orgPreview && (
            <p className={`text-xs ${orgPreview.total === 0 ? 'font-medium text-rose-500' : 'text-slate-400'}`}>
              覆盖账号数（实时展开）：{orgPreview.total}
              {orgPreview.trae ? ` · Trae ${orgPreview.trae}` : ''}
              {orgPreview.buddy ? ` · Buddy ${orgPreview.buddy}` : ''}
              {orgPreview['buddy-ai'] ? ` · BuddyAI 国际 ${orgPreview['buddy-ai']}` : ''}
              {orgPreview.qoder ? ` · Qoder ${orgPreview.qoder}` : ''}
              {orgPreview.total === 0 && ' ⚠ 展开为空集：该 Key 将无任何可取号账号，请调整分组/标签组合'}
            </p>
          )}
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

      {/* F8：关闭鉴权二次确认弹框（禁 window.confirm，红线） */}
      <Modal
        open={authConfirmOpen}
        onClose={() => setAuthConfirmOpen(false)}
        title="确认关闭鉴权"
        footer={
          <>
            <button className="btn-outline" onClick={() => setAuthConfirmOpen(false)}>取消</button>
            <button
              className="btn-primary !bg-rose-600 hover:!bg-rose-500"
              onClick={confirmAuthDisable}
              disabled={keysSaving}
            >
              我已知晓风险，关闭鉴权
            </button>
          </>
        }
      >
        <div className="space-y-2 text-sm">
          <div>关闭鉴权后，未配置启用 Key 时任何本机程序都无需 Key 即可调用模型网关。</div>
          <div className="text-xs text-slate-400">仅建议在完全本机、无可信边界顾虑的环境临时使用；用完请及时重新开启。</div>
        </div>
      </Modal>
    </div>
  );
}
