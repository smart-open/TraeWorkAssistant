/**
 * 全局 API 管理 · 自定义模型（custom_models.json）
 * 列表方式维护 OpenAI 兼容自定义模型：名称（请求模型名，路由键）/ API 地址 / Key /
 * 其他字段（上下文长度 / 最大输出 / 图片支持 / 倍率 / 备注 / 启用开关）。
 * 调度语义：请求模型名 canonical 命中 enabled 条目即直达该上游（用户显式配置优先），
 * 未命中回落 Trae/Buddy 统一管线；数据源：custom_models_list / save / remove。
 * 子弹框（编辑/删除确认）沿用 Modal 叠加，onSubModalChange 上报供主弹窗屏蔽 ESC 双关。
 */
import { useCallback, useEffect, useState } from 'react';
import { Blocks, Pencil, Plus, Power, RefreshCw, Trash2 } from 'lucide-react';
import { Badge, Modal } from '../ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { maskApiKey } from '../../lib/format';
import type { CustomModel } from '../../types';

/** 新建/编辑表单初值（新增时 id 留空由后端生成 cm- id） */
const EMPTY_FORM: CustomModel = {
  id: '',
  name: '',
  base_url: '',
  api_key: '',
  vendor: '',
  enabled: true,
  context_length: 0,
  max_tokens: 0,
  supports_image: false,
  rate: 0,
  note: '',
  updated_at: 0,
};

/** 常用供应商预设（datalist 建议，可自由输入其他值） */
const VENDOR_PRESETS = [
  'OpenAI',
  'Anthropic',
  'Google',
  'DeepSeek',
  '智谱AI',
  'Moonshot（月之暗面）',
  '阿里云百炼（通义）',
  '火山方舟（豆包）',
  'MiniMax',
  '腾讯混元',
  '百度千帆',
  'SiliconFlow 硅基流动',
  'OpenRouter',
  'xAI',
  'Ollama',
  'Groq',
];

export default function CustomModelsPanel({
  onSubModalChange,
}: {
  /** 子弹框（编辑/删除确认）开关状态上报（主弹窗据此屏蔽 ESC 双关） */
  onSubModalChange?: (open: boolean) => void;
}) {
  const toast = useAppStore((s) => s.pushToast);
  const [models, setModels] = useState<CustomModel[]>([]);
  const [loading, setLoading] = useState(false);
  const [saving, setSaving] = useState(false);
  // 编辑弹框（新增与编辑共用；form.id 为空 = 新增）
  const [form, setForm] = useState<CustomModel | null>(null);
  const [deleteFor, setDeleteFor] = useState<CustomModel | null>(null);
  // 必填项内联校验错误（输入即清除）
  const [errors, setErrors] = useState<{ name?: string; base_url?: string }>({});
  // 连通性测试（「测试连接」按钮与「保存前强制测试」共用同一入口）
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<{ ok: boolean; msg: string } | null>(null);
  // 倍率输入框文本态（避免受控数字输入打断小数点输入）
  const [rateText, setRateText] = useState('');

  const load = useCallback(async () => {
    setLoading(true);
    try {
      setModels(await api.apiServer.customModelsList());
    } catch {
      /* 保留空列表 */
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // 子弹框开关状态上报（供主弹窗屏蔽 ESC 双关）
  useEffect(() => {
    onSubModalChange?.(form != null || deleteFor != null);
  }, [form, deleteFor, onSubModalChange]);

  /** 打开/关闭编辑弹框（同时清内联错误与测试结果） */
  const openForm = (f: CustomModel) => {
    setForm(f);
    setErrors({});
    setTestResult(null);
    setRateText(f.rate > 0 ? String(f.rate) : f.id ? '0' : '');
  };
  const closeForm = () => {
    setForm(null);
    setErrors({});
    setTestResult(null);
  };

  /** 必填项内联校验（空对象 = 通过） */
  const validate = (f: CustomModel) => ({
    name: f.name.trim() ? undefined : '请填写模型名称（请求模型名）',
    base_url: /^https?:\/\//.test(f.base_url.trim())
      ? undefined
      : 'API 地址必须以 http:// 或 https:// 开头',
  });

  /** 连通性测试：向上游发一条最小 chat 请求；保存前强制调用同一入口 */
  const runTest = async (): Promise<boolean> => {
    if (!form) return false;
    const errs = validate(form);
    if (errs.name || errs.base_url) {
      setErrors(errs);
      toast('error', '请先完善必填项再测试');
      return false;
    }
    setTesting(true);
    setTestResult(null);
    try {
      const msg = await api.apiServer.customModelTest({
        ...form,
        name: form.name.trim(),
        base_url: form.base_url.trim(),
      });
      setTestResult({ ok: true, msg });
      return true;
    } catch (e) {
      const msg = String(e).slice(0, 160);
      setTestResult({ ok: false, msg });
      toast('error', `测试不通过：${msg}`);
      return false;
    } finally {
      setTesting(false);
    }
  };

  /** 保存（新增/编辑共用）：必填校验 → 强制连通性测试（不通过无法保存）→ upsert */
  const save = async () => {
    if (!form) return;
    const errs = validate(form);
    if (errs.name || errs.base_url) {
      setErrors(errs);
      toast('error', '请完善必填项');
      return;
    }
    const ok = await runTest();
    if (!ok) return;
    setSaving(true);
    try {
      const next = await api.apiServer.customModelsSave({
        ...form,
        name: form.name.trim(),
        base_url: form.base_url.trim(),
        vendor: form.vendor.trim(),
      });
      setModels(next);
      closeForm();
      toast('success', form.id ? `模型「${form.name}」已更新` : `模型「${form.name}」已添加`);
    } catch (e) {
      toast('error', `保存失败：${String(e).slice(0, 120)}`);
    } finally {
      setSaving(false);
    }
  };

  /** 启用/禁用开关（整条 upsert 保存） */
  const toggleEnabled = async (m: CustomModel) => {
    try {
      const next = await api.apiServer.customModelsSave({ ...m, enabled: !m.enabled });
      setModels(next);
      toast('success', `模型「${m.name}」已${m.enabled ? '禁用' : '启用'}`);
    } catch (e) {
      toast('error', `状态更新失败：${String(e).slice(0, 120)}`);
    }
  };

  const confirmDelete = async () => {
    if (!deleteFor) return;
    const m = deleteFor;
    setDeleteFor(null);
    try {
      await api.apiServer.customModelsRemove(m.id);
      setModels((prev) => prev.filter((x) => x.id !== m.id));
      toast('success', `模型「${m.name}」已删除`);
    } catch (e) {
      toast('error', `删除失败：${String(e).slice(0, 120)}`);
    }
  };

  const set = <K extends keyof CustomModel>(k: K, v: CustomModel[K]) =>
    setForm((f) => (f ? { ...f, [k]: v } : f));

  return (
    <div className="card p-4">
      <div className="mb-3 flex flex-wrap items-center justify-between gap-2">
        <div className="flex items-center gap-2">
          <Blocks size={16} className="text-brand-500" />
          <h3 className="text-sm font-semibold text-slate-800 dark:text-zinc-100">自定义模型</h3>
          <span className="hidden text-xs text-slate-400 sm:inline">
            OpenAI 兼容上游直通 · 模型名命中即直达，未命中回落 Trae/Buddy 调度
          </span>
        </div>
        <div className="flex items-center gap-1">
          <button className="btn-ghost flex items-center gap-1 text-xs" onClick={() => void load()} disabled={loading}>
            <RefreshCw size={13} className={loading ? 'animate-spin' : ''} />
            刷新
          </button>
          <button className="btn-outline flex items-center gap-1 !px-3 text-xs" onClick={() => openForm({ ...EMPTY_FORM })}>
            <Plus size={14} />
            新建模型
          </button>
        </div>
      </div>

      {models.length === 0 ? (
        <p className="py-6 text-center text-sm text-slate-400">
          暂无自定义模型 — 点击「新建模型」接入任意 OpenAI 兼容 API（中转站 / 自建服务）
        </p>
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full text-sm">
            <thead>
              <tr className="border-b border-slate-200 text-left text-xs text-slate-500 dark:border-zinc-700 dark:text-zinc-400">
                <th className="pb-2 pr-4 font-medium">模型名称</th>
                <th className="pb-2 pr-4 font-medium">API 地址</th>
                <th className="pb-2 pr-4 font-medium">Key</th>
                <th className="pb-2 pr-4 font-medium">上下文 / 最大输出</th>
                <th className="pb-2 pr-4 font-medium">倍率</th>
                <th className="pb-2 pr-4 font-medium">状态</th>
                <th className="pb-2 font-medium">操作</th>
              </tr>
            </thead>
            <tbody>
              {models.map((m) => (
                <tr
                  key={m.id}
                  className="row-hover border-b border-slate-100 last:border-0 dark:border-zinc-800"
                >
                  <td className="py-2 pr-4">
                    <div className="font-mono text-xs font-medium text-slate-700 dark:text-zinc-200">
                      {m.name}
                      {m.vendor && <Badge tone="slate" className="ml-1.5 !px-1.5 !text-[10px]">{m.vendor}</Badge>}
                    </div>
                    {m.note && <div className="mt-0.5 max-w-48 truncate text-[11px] text-slate-400" title={m.note}>{m.note}</div>}
                  </td>
                  <td className="max-w-56 truncate py-2 pr-4 font-mono text-xs text-slate-500 dark:text-zinc-400" title={m.base_url}>
                    {m.base_url}
                  </td>
                  <td className="py-2 pr-4 font-mono text-xs text-slate-500 dark:text-zinc-400">
                    {m.api_key ? maskApiKey(m.api_key) : '—'}
                  </td>
                  <td className="py-2 pr-4 tabular-nums text-xs text-slate-500 dark:text-zinc-400">
                    {m.context_length > 0 ? fmtK(m.context_length) : '—'}
                    {' / '}
                    {m.max_tokens > 0 ? fmtK(m.max_tokens) : '—'}
                    {m.supports_image && (
                      <Badge tone="blue" className="ml-1.5 !px-1.5 !text-[10px]">图片</Badge>
                    )}
                  </td>
                  <td className="py-2 pr-4 text-xs text-slate-500 dark:text-zinc-400">
                    {m.rate === 0 ? <Badge tone="green">免费</Badge> : `×${m.rate.toFixed(2)}`}
                  </td>
                  <td className="py-2 pr-4">
                    {m.enabled ? <Badge tone="green">启用中</Badge> : <Badge tone="slate">已禁用</Badge>}
                  </td>
                  <td className="py-2">
                    <div className="flex items-center gap-1">
                      <button
                        className="btn-ghost !p-1.5"
                        title={m.enabled ? '禁用（禁用后该模型名回落 Trae/Buddy 调度）' : '启用'}
                        onClick={() => void toggleEnabled(m)}
                      >
                        <Power size={14} className={m.enabled ? 'text-emerald-500' : 'text-slate-400'} />
                      </button>
                      <button className="btn-ghost !p-1.5" title="编辑" onClick={() => openForm({ ...m })}>
                        <Pencil size={14} />
                      </button>
                      <button className="btn-ghost !p-1.5" title="删除" onClick={() => setDeleteFor(m)}>
                        <Trash2 size={14} className="text-rose-500" />
                      </button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {/* 新建/编辑弹框（共用；form.id 为空 = 新增） */}
      <Modal
        open={form != null}
        onClose={closeForm}
        title={form?.id ? `编辑自定义模型 · ${form.name}` : '新建自定义模型'}
        footer={
          <>
            <button className="btn-outline" onClick={closeForm}>取消</button>
            <button className="btn-outline" onClick={() => void runTest()} disabled={testing || saving}>
              {testing ? '测试中…' : '测试连接'}
            </button>
            <button className="btn-primary" onClick={() => void save()} disabled={saving || testing}>
              {saving ? '保存中…' : '保存'}
            </button>
          </>
        }
      >
        {form && (
          <div className="grid grid-cols-1 gap-3 text-sm sm:grid-cols-2">
            <label className="block sm:col-span-1">
              <span className="mb-1 block text-xs font-medium text-slate-500">
                模型名称 <span className="text-rose-500">*</span>
              </span>
              <input
                className={`input font-mono text-xs ${errors.name ? '!border-rose-400' : ''}`}
                placeholder="如 gpt-4o / deepseek-v3（客户端请求模型名）"
                value={form.name}
                onChange={(e) => {
                  set('name', e.target.value);
                  if (errors.name) setErrors((p) => ({ ...p, name: undefined }));
                }}
              />
              {errors.name ? (
                <span className="mt-1 block text-[11px] text-rose-500">{errors.name}</span>
              ) : (
                <span className="mt-1 block text-[11px] text-slate-400">路由键：匹配时忽略大小写与首尾空格</span>
              )}
            </label>
            <label className="block sm:col-span-1">
              <span className="mb-1 block text-xs font-medium text-slate-500">
                API 地址 <span className="text-rose-500">*</span>
              </span>
              <input
                className={`input font-mono text-xs ${errors.base_url ? '!border-rose-400' : ''}`}
                placeholder="https://api.openai.com（含 /v1 亦可）"
                value={form.base_url}
                onChange={(e) => {
                  set('base_url', e.target.value);
                  if (errors.base_url) setErrors((p) => ({ ...p, base_url: undefined }));
                }}
              />
              {errors.base_url ? (
                <span className="mt-1 block text-[11px] text-rose-500">{errors.base_url}</span>
              ) : (
                <span className="mt-1 block text-[11px] text-slate-400">自动拼接 /v1/chat/completions</span>
              )}
            </label>
            <label className="block sm:col-span-2">
              <span className="mb-1 block text-xs font-medium text-slate-500">API Key（Bearer）</span>
              <input
                className="input font-mono text-xs"
                placeholder="sk-…（留空 = 不携带鉴权头）"
                value={form.api_key}
                onChange={(e) => set('api_key', e.target.value)}
              />
            </label>
            <label className="block">
              <span className="mb-1 block text-xs font-medium text-slate-500">上下文长度（0 = 未知）</span>
              <input
                type="number"
                min={0}
                className="input"
                value={form.context_length || ''}
                placeholder="如 128000"
                onChange={(e) => set('context_length', Math.max(0, parseInt(e.target.value) || 0))}
              />
            </label>
            <label className="block">
              <span className="mb-1 block text-xs font-medium text-slate-500">最大输出 tokens（0 = 未知）</span>
              <input
                type="number"
                min={0}
                className="input"
                value={form.max_tokens || ''}
                placeholder="如 16384"
                onChange={(e) => set('max_tokens', Math.max(0, parseInt(e.target.value) || 0))}
              />
            </label>
            <label className="block">
              <span className="mb-1 block text-xs font-medium text-slate-500">展示倍率（0 = 免费）</span>
              <input
                type="number"
                min={0}
                step="0.01"
                className="input"
                value={rateText}
                placeholder="如 0.78；0 标识免费"
                onChange={(e) => {
                  setRateText(e.target.value);
                  set('rate', Math.max(0, parseFloat(e.target.value) || 0));
                }}
              />
              <span className="mt-1 block text-[11px] text-slate-400">支持两位小数（如 0.01）；留空或 0 标识免费</span>
            </label>
            <label className="block">
              <span className="mb-1 block text-xs font-medium text-slate-500">供应商</span>
              <input
                className="input"
                list="vendor-presets"
                placeholder="选择或输入，如 DeepSeek"
                value={form.vendor}
                onChange={(e) => set('vendor', e.target.value)}
              />
              <datalist id="vendor-presets">
                {VENDOR_PRESETS.map((v) => (
                  <option key={v} value={v} />
                ))}
              </datalist>
              <span className="mt-1 block text-[11px] text-slate-400">展示用，可选预设或自由输入</span>
            </label>
            <label className="block sm:col-span-2">
              <span className="mb-1 block text-xs font-medium text-slate-500">备注</span>
              <input
                className="input"
                placeholder="如 中转站 A"
                value={form.note}
                onChange={(e) => set('note', e.target.value)}
              />
            </label>
            <div className="flex items-center gap-4 sm:col-span-2">
              <label className="flex items-center gap-2 text-xs text-slate-600 dark:text-zinc-300">
                <input
                  type="checkbox"
                  checked={form.enabled}
                  onChange={(e) => set('enabled', e.target.checked)}
                />
                启用（禁用后该模型名回落 Trae/Buddy 调度）
              </label>
              <label className="flex items-center gap-2 text-xs text-slate-600 dark:text-zinc-300">
                <input
                  type="checkbox"
                  checked={form.supports_image}
                  onChange={(e) => set('supports_image', e.target.checked)}
                />
                支持图片输入
              </label>
            </div>
            {testResult && (
              <div
                className={`rounded-md px-3 py-2 text-xs sm:col-span-2 ${
                  testResult.ok
                    ? 'bg-emerald-50 text-emerald-700 dark:bg-emerald-900/30 dark:text-emerald-300'
                    : 'bg-rose-50 text-rose-700 dark:bg-rose-900/30 dark:text-rose-300'
                }`}
              >
                {testResult.ok ? '✓ ' : '✗ '}
                {testResult.msg}
              </div>
            )}
          </div>
        )}
      </Modal>

      {/* 删除确认弹框（禁 window.confirm，红线） */}
      <Modal
        open={deleteFor != null}
        onClose={() => setDeleteFor(null)}
        title="删除自定义模型"
        footer={
          <>
            <button className="btn-outline" onClick={() => setDeleteFor(null)}>取消</button>
            <button className="btn-primary !bg-rose-600 hover:!bg-rose-500" onClick={() => void confirmDelete()}>
              确认删除
            </button>
          </>
        }
      >
        <div className="text-sm">
          确认删除模型「{deleteFor?.name}」？
          <div className="mt-1 text-xs text-slate-400">删除后该模型名将回落 Trae/Buddy 统一调度管线（如内置目录存在同名模型）。</div>
        </div>
      </Modal>
    </div>
  );
}

/** token 数缩写（128000 → 128K） */
function fmtK(n: number): string {
  return n >= 1000 ? `${Math.round((n / 1000) * 10) / 10}K` : String(n);
}
