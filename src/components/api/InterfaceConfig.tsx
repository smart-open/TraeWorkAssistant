/**
 * 全局 API 管理 · 接口配置（unified-api-gateway-design §5.2/§5.3）
 * 默认模型配置（gateway_settings_get/set，Phase 1 §8.1）；
 * Web 版单端口架构：/v1/* 网关与管理面同源，无独立端口配置。
 * 接口地址展示走 gatewayBaseUrl()：默认跟随当前访问地址 origin，构建时可注入
 * VITE_GATEWAY_BASE_URL 覆盖（不再固定 127.0.0.1）。
 * issue #26 全局模型白名单：白名单非空 = 仅放行名单内模型（/v1/models 过滤 + 推理端点准入），
 * 空 = 不限；编辑弹框为三源全量多选，管理端目录（unifiedModels）不过滤白名单。
 */
import { useEffect, useState } from 'react';
import { Copy, Globe, ListFilter, Pencil, Save, Search, X } from 'lucide-react';
import { api } from '../../lib/tauri';
import { gatewayBaseUrl } from '../../lib/gateway';
import { withMinDelay } from '../../lib/delay';
import { copyText } from '../../lib/clipboard';
import { useAppStore } from '../../store';
import { Badge, Modal } from '../ui';
import type { GatewaySettings, LanIfaceIp, UnifiedModel } from '../../types';

/** 与后端 canonical_id 一致：trim + 小写（§3.3 #1） */
const canonical = (id: string) => id.trim().toLowerCase();

/** 来源池徽章配色（与 API Keys 管理的池徽标一致） */
function PoolBadges({ pools }: { pools: string[] }) {
  return (
    <>
      {pools.map((p) =>
        p === 'trae' ? (
          <Badge key={p} tone="blue">Trae</Badge>
        ) : p === 'buddy' ? (
          <Badge key={p} tone="violet">Buddy</Badge>
        ) : (
          <Badge key={p} tone="amber">自定义</Badge>
        ),
      )}
    </>
  );
}

export default function InterfaceConfig() {
  const toast = useAppStore((s) => s.pushToast);
  const [gw, setGw] = useState<GatewaySettings | null>(null);
  const [model, setModel] = useState('glm-5.3');
  const [models, setModels] = useState<UnifiedModel[]>([]);
  const [modelsLoaded, setModelsLoaded] = useState(false);
  const [saving, setSaving] = useState(false);
  const [copying, setCopying] = useState(false);
  // 局域网网卡 IPv4（issue #34：网关 0.0.0.0 监听后的接入地址展示）
  const [lanIps, setLanIps] = useState<LanIfaceIp[]>([]);

  // ---- issue #26 模型白名单 ----
  const [whitelist, setWhitelist] = useState<string[]>([]);
  const [savingWl, setSavingWl] = useState(false);
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState<Set<string>>(new Set());
  const [draftEnabled, setDraftEnabled] = useState(false);
  const [wlSearch, setWlSearch] = useState('');

  useEffect(() => {
    api.apiServer
      .gatewaySettingsGet()
      .then((s) => {
        setGw(s);
        setModel(s.default_model);
      })
      .catch(() => {
        /* 保留默认值 */
      });
    api.apiServer
      .unifiedModels()
      .then(setModels)
      .catch(() => {
        /* 保留空列表 */
      })
      .finally(() => setModelsLoaded(true));
    api.apiServer
      .modelWhitelistGet()
      .then(setWhitelist)
      .catch(() => {
        /* 保留空名单（= 不限） */
      });
    api.apiServer
      .lanIfaceIps()
      .then(setLanIps)
      .catch(() => {
        /* 保留空列表（展示回退「仅本机可访问」） */
      });
  }, []);

  const save = async () => {
    setSaving(true);
    try {
      // 后端契约保留 port 字段（原值透传，Web 版无 UI 入口）；模型名由后端规范化（空回退默认）
      const next = await withMinDelay(
        api.apiServer.gatewaySettingsSet({
          port: gw?.port ?? 7864,
          default_model: model.trim(),
          updated_at: gw?.updated_at ?? 0,
        }),
      );
      setGw(next);
      setModel(next.default_model);
      toast('success', '网关设置已保存');
    } catch (e) {
      toast('error', `保存失败：${String(e).slice(0, 120)}`);
    } finally {
      setSaving(false);
    }
  };

  /** 白名单保存（后端归一/去重，返回值为准）；空列表 = 关闭（不限） */
  const saveWhitelist = async (list: string[]) => {
    setSavingWl(true);
    try {
      const next = await withMinDelay(api.apiServer.modelWhitelistSet(list));
      setWhitelist(next);
      toast('success', next.length ? `白名单已保存（${next.length} 个模型）` : '白名单已关闭（不限模型）');
      return true;
    } catch (e) {
      toast('error', `白名单保存失败：${String(e).slice(0, 120)}`);
      return false;
    } finally {
      setSavingWl(false);
    }
  };

  const openEditing = () => {
    setDraft(new Set(whitelist));
    setDraftEnabled(whitelist.length > 0);
    setWlSearch('');
    setEditing(true);
  };

  const saveEditing = async () => {
    if (draftEnabled && draft.size === 0) {
      toast('error', '启用白名单时至少选择 1 个模型');
      return;
    }
    const ok = await saveWhitelist(draftEnabled ? [...draft] : []);
    if (ok) setEditing(false);
  };

  const toggleDraft = (c: string) => {
    setDraft((prev) => {
      const next = new Set(prev);
      if (next.has(c)) next.delete(c);
      else next.add(c);
      return next;
    });
  };

  const copyConfigExample = async () => {
    const m = gw?.default_model ?? 'glm-5.3';
    const base = gatewayBaseUrl();
    // 局域网接入地址（issue #34：网关 0.0.0.0 监听，内网设备按局域网 IP 访问）
    const lanLines = lanIps
      .map((e) => `接口地址(局域网): http://${e.ip}:${gw?.port ?? 7864}/v1  # ${e.name}`)
      .join('\n');
    const example = `# 客户端配置示例（OpenAI 兼容格式）
接口地址: ${base}/v1
${lanLines ? lanLines + '\n' : ''}API Key:  <在「API Keys 管理」中创建并复制>
模型 ID:  ${m}（统一目录内任一模型均可，请求按模型 ID 匹配资源池）

# Anthropic 兼容端点（Claude Code 等工具直连）
POST ${base}/v1/messages
鉴权头: x-api-key: your-api-key 或 Authorization: Bearer

# cURL 测试（请将 API Key 替换为列表中的完整值）
curl -X POST ${base}/v1/chat/completions \\
  -H "Content-Type: application/json" \\
  -H "Authorization: Bearer your-api-key" \\
  -d '{
    "model": "${m}",
    "messages": [{"role": "user", "content": "你好"}],
    "stream": true
  }'`;
    setCopying(true);
    try {
      await withMinDelay(Promise.resolve(copyText(example)));
      toast('success', '配置示例已复制到剪贴板');
    } catch {
      toast('error', '复制失败');
    } finally {
      setCopying(false);
    }
  };

  // 默认模型不在目录中时（如目录尚未聚合），前置占位避免显示错位
  const modelOptions =
    models.some((m) => m.id === model) || !model
      ? models
      : [{ id: model, display: model, rate: null, efforts: [], max_mode: false, context_length: null, max_tokens: null, supports_image: null, manual: false, sources: [] }, ...models];

  // ---- 白名单派生数据 ----
  const catalogByCanonical = new Map(models.map((m) => [canonical(m.id), m]));
  const wlEnabled = whitelist.length > 0;
  // 已失效判定须以目录加载完成为前提：加载中/失败时 catalogByCanonical 为空，
  // 会把全部条目误标失效（「清除失效」将误清所有勾选）
  const draftStale = modelsLoaded ? [...draft].filter((c) => !catalogByCanonical.has(c)) : [];
  const searchLc = wlSearch.trim().toLowerCase();
  const catalogList = models.filter(
    (m) =>
      !searchLc ||
      m.id.toLowerCase().includes(searchLc) ||
      m.display.toLowerCase().includes(searchLc) ||
      m.vendor.toLowerCase().includes(searchLc),
  );

  // 展示用网关地址（跟随当前访问地址 origin；构建时 VITE_GATEWAY_BASE_URL 可覆盖）
  const displayBase = gatewayBaseUrl();

  return (
    <div className="card p-4">
      <div className="mb-3 flex items-center gap-2">
        <Globe size={16} className="text-brand-500" />
        <h3 className="text-sm font-semibold text-slate-800 dark:text-zinc-100">接口配置</h3>
      </div>

      <div className="space-y-4">
        <div>
          <label className="mb-1 block text-xs font-medium text-slate-500 dark:text-zinc-400">
            默认模型
          </label>
          <select
            className="input"
            value={model}
            onChange={(e) => setModel(e.target.value)}
          >
            {modelOptions.map((m) => (
              <option key={m.id} value={m.id}>
                {m.display || m.id}
                {m.rate != null ? `（${m.rate.toFixed(2)}x）` : ''}
              </option>
            ))}
          </select>
          <p className="mt-1 text-xs text-slate-400">
            统一目录（Trae / Buddy 聚合）；用于 CC Switch 注册与未指定 model 的请求
          </p>
        </div>

        {/* issue #26 模型白名单 */}
        <div className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700/60">
          <div className="flex items-center justify-between gap-2">
            <div className="flex items-center gap-2">
              <ListFilter size={14} className={wlEnabled ? 'text-amber-500' : 'text-slate-400'} />
              <span className="text-xs font-medium text-slate-600 dark:text-zinc-300">模型白名单</span>
              {wlEnabled ? (
                <Badge tone="amber">已启用 · {whitelist.length} 个</Badge>
              ) : (
                <Badge tone="slate">未启用 · 不限</Badge>
              )}
            </div>
            <button
              className="btn-ghost flex items-center gap-1 !p-1 text-xs"
              onClick={openEditing}
              disabled={savingWl}
              title="编辑白名单"
            >
              <Pencil size={12} />
              编辑
            </button>
          </div>
          {wlEnabled ? (
            <div className="mt-2 flex flex-wrap items-center gap-1.5">
              {whitelist.map((c) => (
                <span
                  key={c}
                  className="flex items-center gap-1 rounded-full bg-amber-100 px-2 py-0.5 text-[11px] text-amber-700 dark:bg-amber-500/15 dark:text-amber-300"
                >
                  {c}
                  <button
                    className="opacity-60 hover:opacity-100"
                    disabled={savingWl}
                    onClick={() => void saveWhitelist(whitelist.filter((w) => w !== c))}
                    aria-label={`移除 ${c}`}
                  >
                    <X size={11} />
                  </button>
                </span>
              ))}
            </div>
          ) : (
            <p className="mt-1.5 text-xs text-slate-400">
              未启用：目录内全部模型均可访问。启用后 /v1/models 与推理端点仅放行名单内模型（自定义模型同受管辖）。
            </p>
          )}
        </div>

        <button
          className="btn-secondary flex items-center gap-2"
          onClick={() => void save()}
          disabled={saving}
        >
          <Save size={15} />
          {saving ? '保存中…' : '保存配置'}
        </button>

        <div className="rounded-lg bg-slate-50 p-3 text-xs text-slate-500 dark:bg-zinc-800/50 dark:text-zinc-400">
          <div className="mb-2 flex items-center justify-between">
            <p className="font-medium">使用方式 & 配置示例</p>
            <button
              className="btn-ghost flex items-center gap-1 !p-1 text-xs"
              onClick={() => void copyConfigExample()}
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
              <code className="break-all text-[11px]">{displayBase}/v1</code>
            </div>
            <div>
              <span className="text-slate-400">局域网接入：</span>
              {lanIps.length ? (
                lanIps.map((e) => (
                  <code
                    key={e.ip}
                    className="block break-all text-[11px]"
                    title={`网卡：${e.name}`}
                  >
                    http://{e.ip}:{gw?.port ?? 7864}/v1（{e.name}）
                  </code>
                ))
              ) : (
                <code className="text-[11px]">未检测到局域网地址（已排除回环/虚拟网卡）</code>
              )}
            </div>
            <div>
              <span className="text-slate-400">安全提示：</span>
              <code className="text-[11px]">
                网关监听 0.0.0.0，局域网内设备可访问；未启用任何 API Key 时匿名放行，建议创建并启用 Key
              </code>
            </div>
            <div>
              <span className="text-slate-400">API Key：</span>
              <code className="text-[11px]">在「API Keys 管理」中创建并复制（网关共享）</code>
            </div>
            <div>
              <span className="text-slate-400">默认模型：</span>
              <code className="text-[11px]">{gw?.default_model ?? '—'}</code>
            </div>
            <div className="pt-1">
              <span className="text-slate-400">其他端点：</span>
            </div>
            <code className="block break-all text-[11px]">
              POST {displayBase}/v1/messages（Anthropic 兼容，x-api-key 鉴权）
            </code>
            <code className="block break-all text-[11px]">
              GET {displayBase}/v1/models（统一模型目录）
            </code>
            <code className="block break-all text-[11px]">
              GET {displayBase}/health
            </code>
            <div className="border-t border-slate-200 pt-1.5 dark:border-zinc-700/60">
              <span className="text-slate-400">模型档位：</span>
              <code className="text-[11px]">
                reasoning_effort（OpenAI）/ thinking（Anthropic），统一六档 minimal/low/medium/high/xhigh/max，按池自动映射；Trae 未实证模型显式请求时填充默认映射
              </code>
            </div>
            <div>
              <span className="text-slate-400">Max Mode：</span>
              <code className="text-[11px]">
                支持模型（Max 列 ✓）ID 加 -max 后缀启用 1M 上下文，如 glm-5.3-max；qwen3.8-max 需写 qwen3.8-max-max
              </code>
            </div>
          </div>
        </div>
      </div>

      {/* 白名单编辑弹框：三源全量多选 + 来源徽章 + 搜索 + 已失效清除 */}
      <Modal
        open={editing}
        onClose={() => setEditing(false)}
        title="编辑模型白名单"
        size="lg"
        bodyClass="max-h-[calc(100vh-560px)] min-h-[240px] overflow-y-auto"
      >
        <div className="space-y-3">
          <label className="flex items-center gap-2">
            <input
              type="checkbox"
              className="accent-amber-500"
              checked={draftEnabled}
              onChange={(e) => setDraftEnabled(e.target.checked)}
            />
            <span className="text-xs font-medium text-slate-700 dark:text-zinc-200">启用白名单</span>
            <span className="text-xs text-slate-400">关闭 = 目录内全部模型均可访问</span>
          </label>

          <div className="relative">
            <Search size={13} className="absolute left-2.5 top-1/2 -translate-y-1/2 text-slate-400" />
            <input
              className="input !py-1.5 pl-7 text-xs"
              placeholder="搜索模型 ID / 名称 / 供应商"
              value={wlSearch}
              onChange={(e) => setWlSearch(e.target.value)}
              disabled={!draftEnabled}
            />
          </div>

          <div className="max-h-72 overflow-y-auto rounded-lg border border-slate-200 dark:border-zinc-700/60">
            {catalogList.length === 0 && (
              <p className="p-3 text-xs text-slate-400">目录为空或无匹配项；可先在目录页同步官网模型。</p>
            )}
            {catalogList.map((m) => {
              const c = canonical(m.id);
              const checked = draft.has(c);
              return (
                <label
                  key={m.id}
                  className={`flex cursor-pointer items-center gap-2 border-b border-slate-100 px-3 py-2 text-xs last:border-b-0 dark:border-zinc-800 ${
                    checked ? 'bg-amber-50/60 dark:bg-amber-500/5' : ''
                  } ${!draftEnabled ? 'cursor-not-allowed opacity-50' : ''}`}
                >
                  <input
                    type="checkbox"
                    className="accent-amber-500"
                    checked={checked}
                    disabled={!draftEnabled}
                    onChange={() => toggleDraft(c)}
                  />
                  <span className="min-w-0 flex-1 truncate font-medium text-slate-700 dark:text-zinc-200">
                    {m.display || m.id}
                    {m.display !== m.id && (
                      <span className="ml-1.5 font-normal text-slate-400">{m.id}</span>
                    )}
                  </span>
                  <PoolBadges pools={m.sources.map((s) => s.pool)} />
                  {m.rate != null && (
                    <span className="tabular-nums text-slate-400">{m.rate.toFixed(2)}x</span>
                  )}
                </label>
              );
            })}
          </div>

          {draftEnabled && draftStale.length > 0 && (
            <div className="rounded-lg border border-rose-200 bg-rose-50/60 p-2 text-xs dark:border-rose-500/30 dark:bg-rose-500/5">
              <div className="mb-1 flex items-center justify-between">
                <span className="font-medium text-rose-600 dark:text-rose-300">
                  已失效（目录中不存在，仍会按名单拒绝）
                </span>
                <button
                  className="btn-ghost !p-1 text-xs text-rose-500"
                  onClick={() => setDraft((prev) => new Set([...prev].filter((c) => catalogByCanonical.has(c))))}
                >
                  清除失效
                </button>
              </div>
              <div className="flex flex-wrap gap-1">
                {draftStale.map((c) => (
                  <span
                    key={c}
                    className="flex items-center gap-1 rounded-full bg-rose-100 px-2 py-0.5 text-[11px] text-rose-700 dark:bg-rose-500/15 dark:text-rose-300"
                  >
                    {c}
                    <button className="opacity-60 hover:opacity-100" onClick={() => toggleDraft(c)} aria-label={`移除 ${c}`}>
                      <X size={11} />
                    </button>
                  </span>
                ))}
              </div>
            </div>
          )}

          <div className="flex items-center justify-between">
            <span className="text-xs text-slate-400">
              {draftEnabled ? `已选 ${draft.size} 个模型` : '白名单未启用'}
            </span>
            <div className="flex gap-2">
              <button className="btn-ghost text-xs" onClick={() => setEditing(false)} disabled={savingWl}>
                取消
              </button>
              <button
                className="btn-secondary flex items-center gap-1.5 text-xs"
                onClick={() => void saveEditing()}
                disabled={savingWl}
              >
                <Save size={13} />
                {savingWl ? '保存中…' : '保存'}
              </button>
            </div>
          </div>
        </div>
      </Modal>
    </div>
  );
}
