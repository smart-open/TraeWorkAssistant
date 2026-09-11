import { useCallback, useEffect, useState } from 'react';
import { RefreshCw, Play, Square, Save, Copy, Coins, TerminalSquare } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { Badge, Spinner } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { withMinDelay } from '../../lib/delay';
import type {
  ApiServiceStatus,
  ApiPoolFile,
  WbModelInfo,
  WorkBuddyAccountView,
} from '../../types';

/**
 * buddy-api-service API 服务（WB 上游管理，方案A UI 隔离）：
 * 与 Trae「API 服务」共用同一网关实例（同端口/同 Keys），本页聚焦 WB 上游：
 * WB 路由开关 + 模型目录 + WB 账号池状态 + 接入示例。
 * Trae 页的「WorkBuddy 上游」开关组与「同步 WB 模型目录」已迁回此处。
 */

const WB_FLAG_FIELDS: { key: 'wbEnabled' | 'wbDefaultThinking' | 'wbToolExec' | 'wbBgDowngrade'; label: string; desc: string }[] = [
  {
    key: 'wbEnabled',
    label: '启用 WB 上游',
    desc: 'WB 目录模型（owned_by=workbuddy）路由到 WB 账号池，消耗 WB 积分',
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
  const [starting, setStarting] = useState(false);
  const [stopping, setStopping] = useState(false);
  const [copying, setCopying] = useState(false);
  const [refreshing, setRefreshing] = useState(false);

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
      pushToast('error', `读取网关状态失败：${String(err)}`);
    } finally {
      setRefreshing(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // 保存 WB 上游开关：uids/strategy/groups 原样回传（本页不改 Trae 池配置）
  const saveFlags = async () => {
    setSaving(true);
    try {
      await withMinDelay(
        api.apiServer.poolSet(pool?.enabled_uids ?? [], pool?.strategy, pool?.group_ids, wbFlags),
        600,
      );
      pushToast('success', 'WB 上游配置已保存');
      if (status?.running) {
        pushToast('info', '需重启 API 服务以应用变更');
      }
    } catch (err) {
      pushToast('error', `保存失败：${String(err)}`);
    } finally {
      setSaving(false);
    }
  };

  const start = async () => {
    setStarting(true);
    try {
      const s = await withMinDelay(api.apiServer.start(), 800);
      setStatus(s);
      useAppStore.setState({ apiStatus: s });
      pushToast('success', `API 服务已启动（端口 ${s.port}）`);
    } catch (err) {
      pushToast('error', `启动失败：${String(err)}`);
    } finally {
      setStarting(false);
    }
  };

  const stop = async () => {
    setStopping(true);
    try {
      await withMinDelay(api.apiServer.stop(), 600);
      setStatus(null);
      useAppStore.setState({ apiStatus: null });
      pushToast('info', 'API 服务已停止');
    } catch (err) {
      pushToast('error', `停止失败：${String(err)}`);
    } finally {
      setStopping(false);
    }
  };

  const syncCatalog = async () => {
    if (syncing) return;
    setSyncing(true);
    try {
      const n = await withMinDelay(api.apiServer.wbCatalogSync(), 800);
      pushToast('success', `WB 模型目录已更新（${n} 个模型），/v1/models 与路由即时生效`);
      setCatalog(await api.apiServer.wbCatalogList());
    } catch (err) {
      pushToast('error', `WB 模型目录同步失败：${String(err).slice(0, 120)}`);
    } finally {
      setSyncing(false);
    }
  };

  const copyExample = async () => {
    const port = status?.port ?? 7864;
    const model = catalog[0]?.id ?? 'hy4';
    const example = `# 客户端配置示例（WB 模型走 OpenAI 兼容格式）
接口地址: http://127.0.0.1:${port}/v1
API Key:  <在 Trae「API 服务」页的 API Keys 管理中创建并复制>
模型 ID:  ${model}（WB 目录模型，消耗 WB 积分）

# cURL 测试（请将 API Key 替换为列表中的完整值）
curl -X POST http://127.0.0.1:${port}/v1/chat/completions \\
  -H "Content-Type: application/json" \\
  -H "Authorization: Bearer your-api-key" \\
  -d '{
    "model": "${model}",
    "messages": [{"role": "user", "content": "你好"}],
    "stream": true
  }'`;
    setCopying(true);
    try {
      await withMinDelay(navigator.clipboard.writeText(example), 300);
      pushToast('success', '配置示例已复制到剪贴板');
    } catch {
      pushToast('error', '复制失败');
    } finally {
      setCopying(false);
    }
  };

  const credAccounts = accounts.filter((a) => a.has_credential);
  const running = status?.running ?? false;

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="WorkBuddy · API 服务"
        desc="WB 积分转标准 API · 网关 WB 上游管理"
        actions={
          <>
            <button className="btn-outline" onClick={() => void refresh()} disabled={refreshing}>
              <RefreshCw size={15} className={refreshing ? 'animate-spin' : ''} /> 刷新
            </button>
            {running ? (
              <button className="btn-outline !text-rose-600 hover:!border-rose-300" onClick={() => void stop()} disabled={stopping}>
                {stopping ? <Spinner /> : <Square size={15} />} 停止服务
              </button>
            ) : (
              <button className="btn-primary" onClick={() => void start()} disabled={starting}>
                {starting ? <Spinner className="text-white" /> : <Play size={15} />} 启动服务
              </button>
            )}
          </>
        }
      />

      {/* 网关状态卡（共享实例说明） */}
      <div className="card p-4">
        <div className="mb-3 flex items-center justify-between">
          <div className="flex items-center gap-2">
            <span className="text-sm font-medium">网关状态</span>
            <Badge tone={running ? 'green' : 'slate'}>{running ? `运行 :${status?.port}` : '未启动'}</Badge>
            {running && status?.total_requests != null && (
              <span className="text-xs text-slate-400">累计请求 {status.total_requests} 次</span>
            )}
          </div>
        </div>
        <div className="rounded-lg bg-slate-50 p-3 text-xs text-slate-500 dark:bg-zinc-800/50 dark:text-zinc-400">
          与 Trae「API 服务」共用同一网关实例（同端口、同 API Keys、同调度进程）：
          本页管理 WorkBuddy 上游路由与模型目录，Trae 模型的网关级配置（默认模型/Keys/用量）在 Trae 页维护。
          开启下方「启用 WB 上游」后，WB 目录模型（owned_by=workbuddy）的请求将路由到 WB 账号池并消耗 WB 积分。
        </div>
      </div>

      {/* WB 上游配置卡 */}
      <div className="mt-4 card p-4">
        <div className="mb-3 flex items-center justify-between">
          <div className="flex items-center gap-2">
            <TerminalSquare size={16} className="text-brand-500" />
            <span className="text-sm font-medium">WorkBuddy 上游</span>
            <Badge tone={wbFlags.wbEnabled ? 'green' : 'slate'}>
              {wbFlags.wbEnabled ? '已启用' : '未启用'}
            </Badge>
          </div>
          <button className="btn-primary" onClick={() => void saveFlags()} disabled={saving}>
            {saving ? <Spinner className="text-white" /> : <Save size={15} />} 保存
          </button>
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
          保存后需重启 API 服务生效；Trae 账号池的调度策略与分组筛选在 Trae「API 服务」页配置，本页不改动。
        </p>
      </div>

      {/* WB 模型目录卡 */}
      <div className="mt-4 card p-4">
        <div className="mb-3 flex items-center justify-between">
          <div className="flex items-center gap-2">
            <Coins size={16} className="text-amber-500" />
            <span className="text-sm font-medium">WB 模型目录</span>
            <span className="text-xs text-slate-400">{catalog.length} 个模型</span>
          </div>
          <button className="btn-outline" onClick={() => void syncCatalog()} disabled={syncing}>
            <RefreshCw size={14} className={syncing ? 'animate-spin' : ''} />
            {syncing ? '同步中…' : '同步 WB 模型目录'}
          </button>
        </div>
        <p className="mb-3 text-xs text-slate-400">
          从 WB 上游模型目录接口拉取并替换 wb_model_catalog.json（倍率/思考档位/图片模态以服务端为准）；
          网关启动时也会自动同步一次。
        </p>
        {catalog.length === 0 ? (
          <p className="py-4 text-center text-xs text-slate-400">暂无目录数据：点击「同步 WB 模型目录」拉取（需至少一个含凭证的 WB 账号）。</p>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full min-w-[640px] text-sm">
              <thead className="bg-slate-50 text-xs uppercase text-slate-500 dark:bg-zinc-900">
                <tr>
                  <th className="px-3 py-2 text-left">模型 ID</th>
                  <th className="px-3 py-2 text-left">展示名</th>
                  <th className="px-3 py-2 text-right">积分倍率</th>
                  <th className="px-3 py-2 text-left">思考档位</th>
                  <th className="px-3 py-2 text-right">上下文</th>
                  <th className="px-3 py-2 text-left">图片</th>
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
                    <td className="px-3 py-2">{m.supports_image ? <Badge tone="violet">支持</Badge> : <span className="text-xs text-slate-300">-</span>}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>

      {/* WB 账号池状态卡 */}
      <div className="mt-4 card p-4">
        <div className="mb-3 flex items-center gap-2">
          <span className="text-sm font-medium">WB 账号池（上游凭证源）</span>
          <span className="text-xs text-slate-400">{credAccounts.length} 个含凭证账号参与 WB 上游调度</span>
        </div>
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
                <span className="w-24 text-right tabular-nums text-xs text-slate-500">
                  {a.credits_balance != null ? `${a.credits_balance.toFixed(2)} 积分` : '余额未知'}
                </span>
              </div>
            ))}
          </div>
        )}
      </div>

      {/* 使用方式卡 */}
      <div className="mt-4 card p-4">
        <div className="mb-2 flex items-center justify-between">
          <span className="text-sm font-medium">使用方式 & 配置示例</span>
          <button className="btn-outline" onClick={() => void copyExample()} disabled={copying}>
            <Copy size={13} /> {copying ? '复制中…' : '复制示例'}
          </button>
        </div>
        <div className="rounded-lg bg-slate-50 p-3 text-xs text-slate-500 dark:bg-zinc-800/50 dark:text-zinc-400">
          <div>
            接口地址：<code className="break-all text-[11px]">http://127.0.0.1:{status?.port ?? 7864}/v1</code>
          </div>
          <div className="mt-1">
            API Key：<code className="text-[11px]">在 Trae「API 服务」页的「API Keys 管理」中创建并复制（网关共用）</code>
          </div>
          <div className="mt-1">
            模型 ID：<code className="text-[11px]">{catalog[0]?.id ?? '（同步目录后展示）'}</code> 及上表其他 WB 模型
          </div>
          <div className="mt-2 text-[11px]">
            Claude Code / Codex CLI 用户：可在 Trae「API 服务」页把网关端点注册进 CC Switch（生态接入），WB 模型经同一端点使用。
          </div>
        </div>
      </div>
    </div>
  );
}
