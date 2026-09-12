/**
 * 全局 API 管理 · 网关使用帮助弹窗（GatewayHeader 帮助图标入口）
 * 左侧使用说明（端点 / 鉴权 / cURL 示例 / 资源调度说明，与 InterfaceConfig 文案一致）+
 * 右侧当前支持模型列表（供应商 / 模型名称 / 倍率 / 支持图片 / 来源池，带搜索过滤）。
 * 资源调度说明覆盖三层语义：自定义直达 → 池间选池（smart/priority+回退）→ 池内取号，
 * 并指向「资源总览 → 调度策略中心」调整入口（任务7）。
 * 数据源：unified_models（实时聚合 Trae / Buddy / 自定义 三池）+ gateway_settings_get（端口）。
 */
import { useEffect, useMemo, useState } from 'react';
import { CircleHelp, Copy, Search } from 'lucide-react';
import { Badge, Modal } from '../ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import type { GatewaySettings, UnifiedModel } from '../../types';

const POOL_LABELS: Record<string, string> = {
  trae: 'Trae',
  buddy: 'Buddy',
  custom: '自定义',
};

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
  const [gw, setGw] = useState<GatewaySettings | null>(null);
  const [models, setModels] = useState<UnifiedModel[]>([]);
  const [query, setQuery] = useState('');
  const [copied, setCopied] = useState(false);

  const port = gw?.port ?? 7864;
  const base = `http://127.0.0.1:${port}`;

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

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return models;
    return models.filter(
      (m) =>
        m.id.toLowerCase().includes(q) ||
        m.display.toLowerCase().includes(q) ||
        m.vendor.toLowerCase().includes(q),
    );
  }, [models, query]);

  const copyCurl = async () => {
    const m = gw?.default_model || models[0]?.id || 'glm-5.3';
    const example = `curl -X POST ${base}/v1/chat/completions \\
  -H "Content-Type: application/json" \\
  -H "Authorization: Bearer your-api-key" \\
  -d '{
    "model": "${m}",
    "messages": [{"role": "user", "content": "你好"}],
    "stream": true
  }'`;
    setCopied(true);
    try {
      await navigator.clipboard.writeText(example);
      toast('success', 'cURL 示例已复制到剪贴板');
    } catch {
      toast('error', '复制失败');
    } finally {
      setCopied(false);
    }
  };

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
                  请求按模型 ID 匹配资源池；未指定时用默认模型 <code>{gw?.default_model || '—'}</code>
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
                <Copy size={12} className={copied ? 'animate-pulse' : ''} />
                {copied ? '复制中…' : '复制'}
              </button>
            </div>
            <code className="block break-all text-[11px] leading-relaxed text-slate-500 dark:text-zinc-400">
              curl -X POST {base}/v1/chat/completions \<br />
              &nbsp;&nbsp;-H &quot;Authorization: Bearer your-api-key&quot; \<br />
              &nbsp;&nbsp;-d &apos;&#123;&quot;model&quot;: &quot;{gw?.default_model || '模型 ID'}&quot;, &quot;messages&quot;:
              [...&#125;&apos;
            </code>
          </div>

          <div className="rounded-lg bg-slate-50 p-3 text-xs dark:bg-zinc-800/50">
            <p className="mb-2 font-medium text-slate-700 dark:text-zinc-200">资源调度说明</p>
            <div className="space-y-1.5 leading-relaxed text-slate-500 dark:text-zinc-400">
              <p>
                <span className="font-medium text-slate-600 dark:text-zinc-300">自定义模型：</span>
                模型名命中启用条目即直达该上游（上游自有计费），不参与跨池回退。
              </p>
              <p>
                <span className="font-medium text-slate-600 dark:text-zinc-300">双源模型（Trae/Buddy 同名）：</span>
                按「池间调度策略」选池——智能调度（默认）按 积分先到期 → 免费/低倍率 → 积分多 排序，全并列时按池优先级序；
                固定优先级按序取首选可用池；可选「首选池不可用时跨池回退」。仅单源可用时直接路由该池。
              </p>
              <p>
                <span className="font-medium text-slate-600 dark:text-zinc-300">池内取号：</span>
                Trae 池 / Buddy 池各自独立配置策略（默认积分先过期优先，可选 余额多优先 / 随机 / 三因子加权 / P2C）。
              </p>
              <p className="text-slate-400 dark:text-zinc-500">
                调整入口：「资源总览」Tab → 调度策略中心（内置最佳组合预设一键应用，运行中网关即时生效，无需重启）。
              </p>
            </div>
          </div>
        </div>

        {/* 右：支持模型列表 */}
        <div className="lg:col-span-3">
          <div className="mb-2 flex items-center gap-2">
            <p className="text-xs font-medium text-slate-700 dark:text-zinc-200">
              支持模型<span className="ml-1 text-slate-400">（{filtered.length}/{models.length}）</span>
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
            <table className="w-full text-left text-xs">
              <thead>
                <tr className="border-b border-slate-200 bg-slate-50 text-[11px] text-slate-400 dark:border-zinc-700 dark:bg-zinc-800/50 dark:text-zinc-500">
                  <th className="px-2.5 py-2 font-medium">供应商</th>
                  <th className="px-2.5 py-2 font-medium">模型名称</th>
                  <th className="px-2.5 py-2 text-right font-medium">倍率</th>
                  <th className="px-2.5 py-2 text-center font-medium">图片</th>
                  <th className="px-2.5 py-2 font-medium">来源</th>
                </tr>
              </thead>
              <tbody>
                {filtered.map((m) => {
                  const enabled = m.sources.some((s) => s.enabled);
                  return (
                    <tr
                      key={m.id}
                      className="border-b border-slate-100 text-slate-700 last:border-0 dark:border-zinc-800 dark:text-zinc-200"
                    >
                      <td className="max-w-[8rem] truncate px-2.5 py-2" title={m.vendor || undefined}>
                        {m.vendor || '—'}
                      </td>
                      <td className="px-2.5 py-2">
                        <span className="block max-w-[14rem] truncate" title={m.id}>
                          {m.display || m.id}
                        </span>
                      </td>
                      <td className="px-2.5 py-2 text-right tabular-nums">
                        {m.rate == null ? (
                          <span className="text-slate-400 dark:text-zinc-500">—</span>
                        ) : m.rate === 0 ? (
                          <Badge tone="green">免费</Badge>
                        ) : (
                          `${m.rate}x`
                        )}
                      </td>
                      <td className="px-2.5 py-2 text-center">
                        {m.supports_image == null ? (
                          <span className="text-slate-400 dark:text-zinc-500">—</span>
                        ) : m.supports_image ? (
                          <span className="text-emerald-600 dark:text-emerald-400">✓</span>
                        ) : (
                          <span className="text-slate-400 dark:text-zinc-500">✗</span>
                        )}
                      </td>
                      <td className="px-2.5 py-2">
                        <div className="flex flex-wrap gap-1">
                          {m.sources.map((s) => (
                            <Badge key={s.pool} tone={s.enabled ? 'slate' : 'amber'}>
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
                    <td colSpan={5} className="px-2.5 py-6 text-center text-slate-400 dark:text-zinc-500">
                      {models.length === 0 ? '模型目录加载中或为空' : '无匹配模型'}
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
