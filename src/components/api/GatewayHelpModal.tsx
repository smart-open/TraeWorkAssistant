/**
 * 全局 API 管理 · 网关使用帮助弹窗（GatewayHeader 帮助图标入口）
 * 左侧使用说明（端点 / 鉴权 / cURL 示例 / 资源调度 / 模型档位与 Max Mode，与 InterfaceConfig 文案一致）+
 * 右侧当前支持模型列表（供应商 / 模型名称 / 倍率 / 档位 / Max Mode / 图片 / 来源池，
 * table-fixed 定宽防横向滚动，带搜索过滤）。
 * 资源调度说明覆盖三层语义：自定义直达 → 池间选池（smart/priority+回退）→ 池内取号，
 * 并指向「资源总览 → 调度策略中心」调整入口（任务7）。
 * 数据源：unified_models（实时聚合 Trae / Buddy / BuddyAI 国际 / Qoder / 自定义 多池）+ gateway_settings_get（默认模型）。
 */
import { useEffect, useMemo, useRef, useState } from 'react';
import { CircleHelp, Copy, Search } from 'lucide-react';
import { Badge, Modal } from '../ui';
import { api } from '../../lib/tauri';
import { gatewayBaseUrl } from '../../lib/gateway';
import { copyText } from '../../lib/clipboard';
import { isPoolVisible } from '../../lib/appVisibility';
import { useAppStore } from '../../store';
import type { GatewaySettings, Settings, UnifiedModel } from '../../types';

const POOL_LABELS: Record<string, string> = {
  trae: 'Trae',
  buddy: 'Buddy',
  'buddy-ai': 'BuddyAI 国际',
  qoder: 'Qoder',
  custom: '自定义',
};

/** 倍率取当前可用且可见来源池最小值（同一模型多池服务时按最低消耗展示）。
 * 仅统计 enabled 且未被 L1 禁用（disabled，issue #145）且可见的来源：不可用/已禁池不承接
 * 请求、隐藏池不面向当前用户展示，其倍率不应参与展示，否则用户会看到实际路由不到的低价
 * （与「实际生效倍率」口径背离）；全部不可用或未声明 → null（显示 —） */
function minRateOf(m: UnifiedModel, settings: Settings | null): number | null {
  const rates = m.sources
    .filter((s) => s.enabled && !s.disabled && isPoolVisible(settings, s.pool))
    .map((s) => s.rate)
    .filter((r): r is number => r != null && Number.isFinite(r));
  return rates.length ? Math.min(...rates) : null;
}

function Endpoint({ method, path, note }: { method: string; path: string; note: string }) {
  return (
    <div className="flex items-baseline gap-2">
      <Badge tone={method === 'POST' ? 'blue' : 'slate'}>{method}</Badge>
      <code className="break-all text-[11px]">{path}</code>
      <span className="ml-auto shrink-0 text-[11px] text-slate-400 dark:text-zinc-500">{note}</span>
    </div>
  );
}

export default function GatewayHelpModal({ open, onClose }: { open: boolean; onClose: () => void }) {
  const toast = useAppStore((s) => s.pushToast);
  const settings = useAppStore((s) => s.settings);
  const [gw, setGw] = useState<GatewaySettings | null>(null);
  const [models, setModels] = useState<UnifiedModel[]>([]);
  const [query, setQuery] = useState('');
  const [copied, setCopied] = useState(false);
  // 审查修复（L36）：复制成功态停留 2 秒的定时器句柄（重复点击重置、卸载清理防泄漏）
  const copiedTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const base = gatewayBaseUrl();

  useEffect(() => {
    if (!open) return;
    api.apiServer.gatewaySettingsGet().then(setGw).catch(() => {
      /* 保留默认端口 */
    });
    api.apiServer
      .unifiedModels()
      .then(setModels)
      .catch(() => {
        /* 保留空列表 */
      });
  }, [open]);

  /** 可见模型基座（系统设置「应用显示」联动）：仅保留至少有一个可见来源池的模型 */
  const visibleModels = useMemo(
    () => models.filter((m) => m.sources.some((s) => isPoolVisible(settings, s.pool))),
    [models, settings],
  );
  // cURL 示例模型：默认模型属已隐藏池时回落首个可见模型（示例须实际可请求）
  const exampleModel =
    gw?.default_model && visibleModels.some((m) => m.id === gw.default_model)
      ? gw.default_model
      : (visibleModels[0]?.id ?? '');

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return visibleModels;
    return visibleModels.filter(
      (m) =>
        m.id.toLowerCase().includes(q) ||
        m.display.toLowerCase().includes(q) ||
        m.vendor.toLowerCase().includes(q),
    );
  }, [visibleModels, query]);

  const copyCurl = async () => {
    const m = exampleModel || 'glm-5.3';
    const example = `curl -X POST ${base}/v1/chat/completions \\
  -H "Content-Type: application/json" \\
  -H "Authorization: Bearer your-api-key" \\
  -d '{
    "model": "${m}",
    "messages": [{"role": "user", "content": "你好"}],
    "stream": true
  }'`;
    try {
      // 复制成功后才置 copied
      if (await copyText(example)) {
        setCopied(true);
        toast('success', 'cURL 示例已复制到剪贴板');
        // 审查修复（L36）：成功态停留 2 秒再复位（原先 finally 立即复位，用户看不到「已复制」反馈）；
        // 重复点击时重置定时器，卸载时统一清理防泄漏
        if (copiedTimerRef.current) clearTimeout(copiedTimerRef.current);
        copiedTimerRef.current = setTimeout(() => setCopied(false), 2000);
      } else toast('error', '复制失败');
    } catch {
      toast('error', '复制失败');
    }
  };

  // 审查修复（L36）：卸载时清理成功态定时器，避免泄漏
  useEffect(
    () => () => {
      if (copiedTimerRef.current) clearTimeout(copiedTimerRef.current);
    },
    [],
  );

  return (
    <Modal
      open={open}
      onClose={onClose}
      title="API 使用帮助"
      size="2xl"
      bodyClass="max-h-[76vh] overflow-y-auto"
    >
      <div className="grid grid-cols-1 gap-4 lg:grid-cols-5">
        {/* 左：使用说明 */}
        <div className="space-y-3 lg:col-span-2">
          <div className="rounded-lg bg-slate-50 p-3 text-xs dark:bg-zinc-800/50">
            <p className="mb-2 font-medium text-slate-700 dark:text-zinc-200">接入方式（OpenAI / Anthropic 兼容）</p>
            <div className="space-y-1.5 text-slate-500 dark:text-zinc-400">
              <div>
                <span className="text-slate-400 dark:text-zinc-500">接口地址：</span>
                <code className="text-[11px]">{base}/v1</code>
              </div>
              <div>
                <span className="text-slate-400 dark:text-zinc-500">API Key：</span>
                <span className="text-[11px]">在「API Keys 管理」中创建，请求头携带 Authorization: Bearer</span>
              </div>
              <div>
                <span className="text-slate-400 dark:text-zinc-500">模型：</span>
                <span className="text-[11px]">
                  请求按模型 ID 匹配资源池；未指定时用默认模型{' '}
                  <code>
                    {gw?.default_model || '—'}
                    {gw?.default_model &&
                    // 复审修复：仅「目录中存在但仅由隐藏池服务」才标注；目录中不存在
                    // 的存量默认模型不加误导性标注（placeholder 场景另行兜底展示）
                    models.some((m) => m.id === gw.default_model) &&
                    !visibleModels.some((m) => m.id === gw.default_model)
                      ? '（属已隐藏池）'
                      : ''}
                  </code>
                </span>
              </div>
            </div>
          </div>

          <div className="space-y-1.5 text-xs">
            <p className="font-medium text-slate-700 dark:text-zinc-200">端点</p>
            <Endpoint method="POST" path="/v1/chat/completions" note="OpenAI 兼容" />
            <Endpoint method="POST" path="/v1/messages" note="Anthropic 兼容（x-api-key 或 Bearer）" />
            <Endpoint method="GET" path="/v1/models" note="统一模型目录" />
            <Endpoint method="GET" path="/health" note="健康检查" />
          </div>

          <div className="rounded-lg bg-slate-50 p-3 text-xs dark:bg-zinc-800/50">
            <div className="mb-2 flex items-center justify-between">
              <p className="font-medium text-slate-700 dark:text-zinc-200">cURL 测试</p>
              <button
                className="btn-ghost flex items-center gap-1 !p-1 text-[11px]"
                onClick={() => void copyCurl()}
                disabled={copied}
                title="复制 cURL 示例"
              >
                <Copy size={12} />
                {copied ? '已复制' : '复制'}
              </button>
            </div>
            <code className="block break-all text-[11px] leading-relaxed text-slate-500 dark:text-zinc-400">
              curl -X POST {base}/v1/chat/completions \<br />
              &nbsp;&nbsp;-H &quot;Authorization: Bearer your-api-key&quot; \<br />
              &nbsp;&nbsp;-d &apos;&#123;&quot;model&quot;: &quot;{exampleModel || '模型 ID'}&quot;, &quot;messages&quot;:
              [...&#125;&apos;
            </code>
          </div>

          <div className="rounded-lg bg-slate-50 p-3 text-xs dark:bg-zinc-800/50">
            <p className="mb-2 font-medium text-slate-700 dark:text-zinc-200">资源调度说明</p>
            <div className="space-y-1.5 leading-relaxed text-slate-500 dark:text-zinc-400">
              <p>
                <span className="font-medium text-slate-600 dark:text-zinc-300">自定义模型：</span>
                同名多条上游记录组成上游组（组内加权轮询负载均衡，可标记备胎），失败自动切换组内其他上游、
                连续故障熔断冷却自动恢复；作为第五资源池参与统一调度——默认候选序位于尾部兜底，
                纯自定义模型（无同名内置源）直达；可用 <code>custom/模型名</code> 前缀或 Key 绑定「自定义上游池」显式优先。
              </p>
              <p>
                <span className="font-medium text-slate-600 dark:text-zinc-300">多源同名模型（Trae/Buddy/BuddyAI 国际/Qoder/自定义）：</span>
                按「池间调度策略」选池——智能调度（默认）按 积分先到期 → 免费/低倍率 → 积分多 排序，全并列时按池优先级序；
                固定优先级按序取首选可用池（Qoder / BuddyAI 国际 / 自定义默认序位于尾部，可在调度策略中心上移）；可选「首选池不可用时跨池回退」。仅单源可用时直接路由该池。
              </p>
              <p>
                <span className="font-medium text-slate-600 dark:text-zinc-300">模型目录标注来源池：</span>
                「资源总览 → 调度策略中心」开启后，/v1/models 目录为多源同名模型附带{' '}
                <code>trae/xxx</code>、<code>buddy/xxx</code>、<code>buddy-ai/xxx</code>、<code>qoder/xxx</code>、<code>custom/xxx</code>{' '}
                别名条目（各池同名模型倍率与效果不同），
                右表「来源」列同步标注模型由哪些池服务；请求使用带池前缀的模型 ID 即点名锁定该池——
                单池服务、不参与跨池回退，该池不可用时直接报错（前缀优先级高于 Key 绑定与全局策略）；关闭时目录保持原形态。
              </p>
              <p>
                <span className="font-medium text-slate-600 dark:text-zinc-300">池内取号：</span>
                Trae 池 / Buddy 池 / BuddyAI 国际池 / Qoder 池各自独立配置策略（默认积分先过期优先，可选 余额多优先 / 随机 / 三因子加权 / P2C）。
              </p>
              <p className="text-slate-400 dark:text-zinc-500">
                调整入口：「资源总览」Tab → 调度策略中心（内置最佳组合预设一键应用，运行中网关即时生效，无需重启）。
              </p>
            </div>
          </div>

          <div className="rounded-lg bg-slate-50 p-3 text-xs dark:bg-zinc-800/50">
            <p className="mb-2 font-medium text-slate-700 dark:text-zinc-200">模型档位与 Max Mode</p>
            <div className="space-y-1.5 leading-relaxed text-slate-500 dark:text-zinc-400">
              <p>
                <span className="font-medium text-slate-600 dark:text-zinc-300">思考档位：</span>
                统一六档 minimal / low / medium / high / xhigh / max。请求体{' '}
                <code>reasoning_effort</code>（OpenAI 兼容）或 <code>thinking</code>（Anthropic 兼容）
                传入后按池自动映射——Trae 三档 light / high / extra_high，
                Buddy / BuddyAI 国际 按上游声明下发，
                Qoder 暂不支持档位透传（目录思考档位仅展示）；
                档位列取多池声明并集（统一档位值）。Trae 侧未实证的模型显式请求档位时按
                统一映射填充默认下发，未显式请求时走上游默认（默认深度思考 high 仅对已实证模型生效）。
              </p>
              <p>
                <span className="font-medium text-slate-600 dark:text-zinc-300">Max Mode（1M 上下文，仅 Trae 池）：</span>
                右表「Max」列为 ✓ 的模型在 ID 后加 <code>-max</code> 后缀即可启用（如 <code>glm-5.3-max</code>），
                网关自动剥离后缀并向 Trae 注入；也可在 OpenAI 兼容请求体直传{' '}
                <code>&quot;is_max_mode&quot;: true</code>；或在 Trae 资源调度页的模型目录勾选该模型的「长上下文」，
                网关对该模型的每个 Trae 请求自动启用（不影响 Qoder 等其他池）。表外模型加后缀不生效；
                特例 <code>qwen3.8-max</code> 本身以 -max 结尾，启用需写 <code>qwen3.8-max-max</code>。
              </p>
            </div>
          </div>
        </div>

        {/* 右：支持模型列表 */}
        <div className="lg:col-span-3">
          <div className="mb-2 flex items-center gap-2">
            <p className="text-xs font-medium text-slate-700 dark:text-zinc-200">
              支持模型<span className="ml-1 text-slate-400">（{filtered.length}/{visibleModels.length}）</span>
            </p>
            <div className="relative ml-auto">
              <Search size={12} className="absolute left-2 top-1/2 -translate-y-1/2 text-slate-400" />
              <input
                className="h-7 w-44 rounded-md border border-slate-200 bg-white pl-7 pr-2 text-xs text-slate-700 outline-none focus:border-brand-400 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
                placeholder="搜索模型 / 供应商"
                value={query}
                onChange={(e) => setQuery(e.target.value)}
              />
            </div>
          </div>
          <div className="overflow-hidden rounded-lg border border-slate-200 dark:border-zinc-700">
            <table className="w-full table-fixed text-left text-xs">
              <thead>
                <tr className="border-b border-slate-200 bg-slate-50 text-[11px] text-slate-400 dark:border-zinc-700 dark:bg-zinc-800/50 dark:text-zinc-500">
                  <th className="w-[12%] px-2 py-2 font-medium">供应商</th>
                  <th className="w-[21%] px-2 py-2 font-medium">模型名称</th>
                  <th className="w-[11%] px-2 py-2 text-right font-medium">倍率</th>
                  <th className="w-[21%] px-2 py-2 font-medium" title="思考档位（统一六档）">档位</th>
                  <th className="w-[6%] px-2 py-2 text-center font-medium" title="Max Mode（1M 上下文，仅 Trae 池）">Max</th>
                  <th className="w-[6%] px-2 py-2 text-center font-medium">图片</th>
                  <th className="w-[23%] px-2 py-2 font-medium">来源</th>
                </tr>
              </thead>
              <tbody>
                {filtered.map((m) => {
                  // 行内来源按可见性过滤（隐藏池徽章不展示，避免泄露）；enabled 口径同步取
                  // 可见且未被 L1 禁用（issue #145）的来源——被禁模型不发布 /v1/models，
                  // 此处不能呈现为可用
                  const visibleSources = m.sources.filter((s) => isPoolVisible(settings, s.pool));
                  const enabled = visibleSources.some((s) => s.enabled && !s.disabled);
                  const rate = minRateOf(m, settings);
                  return (
                    <tr
                      key={m.id}
                      className="border-b border-slate-100 text-slate-700 last:border-0 dark:border-zinc-800 dark:text-zinc-200"
                    >
                      <td className="truncate px-2 py-2" title={m.vendor || undefined}>
                        {m.vendor || '—'}
                      </td>
                      <td className="px-2 py-2">
                        <span className="block break-all font-mono leading-snug">{m.id}</span>
                      </td>
                      <td className="px-2 py-2 text-right tabular-nums" title="多来源模型取各池最小倍率">
                        {rate == null ? (
                          <span className="text-slate-400 dark:text-zinc-500">—</span>
                        ) : rate === 0 ? (
                          <Badge tone="green">免费</Badge>
                        ) : (
                          `${rate.toFixed(2)}x`
                        )}
                      </td>
                      <td className="truncate px-2 py-2 text-[11px]" title={m.efforts.length ? m.efforts.join(' / ') : undefined}>
                        {m.efforts.length ? (
                          <span className="font-mono text-slate-500 dark:text-zinc-400">{m.efforts.join('/')}</span>
                        ) : (
                          <span className="text-slate-400 dark:text-zinc-500">—</span>
                        )}
                      </td>
                      <td className="px-2 py-2 text-center" title={m.max_mode ? '支持 Max Mode（模型 ID 加 -max 后缀启用）' : undefined}>
                        {m.max_mode ? (
                          <span className="text-emerald-600 dark:text-emerald-400">✓</span>
                        ) : (
                          <span className="text-slate-400 dark:text-zinc-500">—</span>
                        )}
                      </td>
                      <td className="px-2 py-2 text-center">
                        {m.supports_image == null ? (
                          <span className="text-slate-400 dark:text-zinc-500">—</span>
                        ) : m.supports_image ? (
                          <span className="text-emerald-600 dark:text-emerald-400">✓</span>
                        ) : (
                          <span className="text-slate-400 dark:text-zinc-500">✗</span>
                        )}
                      </td>
                      <td className="px-2 py-2">
                        <div className="flex flex-wrap gap-1">
                          {visibleSources.map((s) => (
                            <Badge key={s.pool} tone={s.enabled && !s.disabled ? 'slate' : 'amber'}>
                              {POOL_LABELS[s.pool] ?? s.pool}
                            </Badge>
                          ))}
                          {!enabled && <Badge tone="amber">未启用</Badge>}
                        </div>
                      </td>
                    </tr>
                  );
                })}
                {filtered.length === 0 && (
                  <tr>
                    <td colSpan={7} className="px-2 py-6 text-center text-slate-400 dark:text-zinc-500">
                      {models.length === 0
                        ? '模型目录加载中或为空'
                        : visibleModels.length === 0
                          ? '当前可见应用下无支持模型'
                          : '无匹配模型'}
                    </td>
                  </tr>
                )}
              </tbody>
            </table>
          </div>
        </div>
      </div>
    </Modal>
  );
}

/** 帮助图标按钮（启动/停止网关按钮右侧），GatewayHeader 使用 */
export function GatewayHelpButton({ onClick }: { onClick: () => void }) {
  return (
    <button
      className="btn-ghost flex h-8 w-8 shrink-0 items-center justify-center !p-0 text-slate-400 hover:text-brand-500 dark:text-zinc-500"
      onClick={onClick}
      title="API 使用帮助"
      aria-label="API 使用帮助"
    >
      <CircleHelp size={16} />
    </button>
  );
}
