/**
 * 全局 API 管理 · 接口配置（unified-api-gateway-design §5.2/§5.3）
 * 读写 api_gateway_settings.json（gateway_settings_get/set，Phase 1 §8.1）；
 * 端口改动下次启动 API 服务后生效；含使用方式与配置示例。
 */
import { useEffect, useState } from 'react';
import { Copy, Globe, Save } from 'lucide-react';
import { api } from '../../lib/tauri';
import { withMinDelay } from '../../lib/delay';
import { useAppStore } from '../../store';
import type { GatewaySettings, UnifiedModel } from '../../types';

export default function InterfaceConfig() {
  const toast = useAppStore((s) => s.pushToast);
  const [gw, setGw] = useState<GatewaySettings | null>(null);
  const [port, setPort] = useState(7864);
  const [model, setModel] = useState('glm-5.3');
  const [models, setModels] = useState<UnifiedModel[]>([]);
  const [saving, setSaving] = useState(false);
  const [copying, setCopying] = useState(false);

  useEffect(() => {
    api.apiServer
      .gatewaySettingsGet()
      .then((s) => {
        setGw(s);
        setPort(s.port);
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
      });
  }, []);

  const save = async () => {
    const p = Math.floor(port);
    if (!Number.isFinite(p) || p < 1 || p > 65535) {
      toast('error', '端口需为 1-65535 的整数');
      return;
    }
    setSaving(true);
    try {
      // 后端会规范化（空模型名回退默认值），前端展示以返回值为准（§5.3）
      const next = await withMinDelay(
        api.apiServer.gatewaySettingsSet({
          port: p,
          default_model: model.trim(),
          updated_at: gw?.updated_at ?? 0,
        }),
      );
      setGw(next);
      setPort(next.port);
      setModel(next.default_model);
      toast('success', '网关设置已保存；端口改动将在下次启动 API 服务后生效');
    } catch (e) {
      toast('error', `保存失败：${String(e).slice(0, 120)}`);
    } finally {
      setSaving(false);
    }
  };

  const copyConfigExample = async () => {
    const p = gw?.port ?? 7864;
    const m = gw?.default_model ?? 'glm-5.3';
    const example = `# 客户端配置示例（OpenAI 兼容格式）
接口地址: http://127.0.0.1:${p}/v1
API Key:  <在「API Keys 管理」中创建并复制>
模型 ID:  ${m}（统一目录内任一模型均可，请求按模型 ID 匹配资源池）

# Anthropic 兼容端点（Claude Code 等工具直连）
POST http://127.0.0.1:${p}/v1/messages
鉴权头: x-api-key: your-api-key 或 Authorization: Bearer

# cURL 测试（请将 API Key 替换为列表中的完整值）
curl -X POST http://127.0.0.1:${p}/v1/chat/completions \\
  -H "Content-Type: application/json" \\
  -H "Authorization: Bearer your-api-key" \\
  -d '{
    "model": "${m}",
    "messages": [{"role": "user", "content": "你好"}],
    "stream": true
  }'`;
    setCopying(true);
    try {
      await withMinDelay(navigator.clipboard.writeText(example));
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
      : [{ id: model, display: model, rate: null, efforts: [], context_length: null, max_tokens: null, supports_image: null, manual: false, sources: [] }, ...models];

  return (
    <div className="card p-4">
      <div className="mb-3 flex items-center gap-2">
        <Globe size={16} className="text-brand-500" />
        <h3 className="text-sm font-semibold text-slate-800 dark:text-zinc-100">接口配置</h3>
        <span className="text-xs text-slate-400">读写 api_gateway_settings.json</span>
      </div>

      <div className="space-y-4">
        <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
          <div>
            <label className="mb-1 block text-xs font-medium text-slate-500 dark:text-zinc-400">
              监听端口
            </label>
            <input
              type="number"
              className="input"
              value={port}
              onChange={(e) => setPort(parseInt(e.target.value) || 0)}
            />
            <p className="mt-1 text-xs text-slate-400">改动将在下次启动 API 服务后生效</p>
          </div>
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
              <code className="break-all text-[11px]">http://127.0.0.1:{gw?.port ?? 7864}/v1</code>
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
              POST http://127.0.0.1:{gw?.port ?? 7864}/v1/messages（Anthropic 兼容，x-api-key 鉴权）
            </code>
            <code className="block break-all text-[11px]">
              GET http://127.0.0.1:{gw?.port ?? 7864}/v1/models（统一模型目录）
            </code>
            <code className="block break-all text-[11px]">
              GET http://127.0.0.1:{gw?.port ?? 7864}/health
            </code>
          </div>
        </div>
      </div>
    </div>
  );
}
