/**
 * Qoder · 单模型上下文长度设置弹框（kv qoder_context_length，仅 Qoder 池）。
 *
 * 填写 → 该模型经 Qoder 池的请求体下发 `parameters.context_length`；留空 = 跟随上游
 * （不下发字段，请求与未设置时完全一致）。与 Trae 页「长上下文」开关分表存储、互不影响。
 * Qoder 未公开可接受的取值，上游不认时返回 400——规则与恢复方式必须写在弹框里。
 */
import { useEffect, useState } from 'react';
import { RotateCcw, Save } from 'lucide-react';
import { Modal, Spinner } from '../ui';
import { api } from '../../lib/tauri';
import { withMinDelay } from '../../lib/delay';
import { useAppStore } from '../../store';

/** 对齐后端 context_window::QODER_MIN_TOKENS / QODER_MAX_TOKENS（视图未加载时的兜底） */
const DEFAULT_MIN = 1_024;
const DEFAULT_MAX = 10_000_000;

export default function QoderContextLengthModal({
  model,
  current,
  catalogContext,
  min = DEFAULT_MIN,
  max = DEFAULT_MAX,
  onClose,
  onSaved,
}: {
  /** null = 弹框关闭 */
  model: string | null;
  /** 已保存值（null = 跟随上游） */
  current: number | null;
  /** 目录声明上下文（仅作参考展示） */
  catalogContext: number | null;
  min?: number;
  max?: number;
  onClose: () => void;
  /** 保存成功：tokens 为生效值（null = 已恢复跟随上游） */
  onSaved: (model: string, tokens: number | null) => void;
}) {
  const toast = useAppStore((s) => s.pushToast);
  const [draft, setDraft] = useState('');
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    if (model != null) setDraft(current != null ? String(current) : '');
  }, [model, current]);

  const trimmed = draft.trim();
  const parsed = trimmed === '' ? null : Number(trimmed);
  const error =
    parsed == null
      ? null
      : !Number.isInteger(parsed)
        ? '请输入整数 token 数'
        : parsed < min || parsed > max
          ? `超出合法范围 ${min.toLocaleString()} ~ ${max.toLocaleString()}`
          : null;
  const unchanged = parsed === current;

  const save = async (tokens: number | null) => {
    if (model == null || saving) return;
    setSaving(true);
    try {
      const got = await withMinDelay(api.apiServer.qoderContextLengthSet(model, tokens), 300);
      toast(
        'success',
        got != null
          ? `${model} 上下文长度已设为 ${got.toLocaleString()}，下一请求生效`
          : `${model} 已恢复跟随上游`,
      );
      onSaved(model, got);
    } catch (e) {
      toast('error', `保存失败：${String(e)}`);
    } finally {
      setSaving(false);
    }
  };

  return (
    <Modal
      open={model != null}
      onClose={() => {
        if (!saving) onClose();
      }}
      title={`上下文长度 · ${model ?? ''}`}
      footer={
        <>
          {current != null && (
            <button
              className="btn-ghost mr-auto inline-flex items-center gap-1"
              disabled={saving}
              onClick={() => void save(null)}
            >
              <RotateCcw size={14} />
              恢复跟随上游
            </button>
          )}
          <button className="btn-outline" disabled={saving} onClick={onClose}>
            取消
          </button>
          <button
            className="btn-primary inline-flex items-center gap-1"
            disabled={saving || error != null || unchanged}
            onClick={() => void save(parsed)}
          >
            {saving ? <Spinner className="text-white" /> : <Save size={14} />}
            保存
          </button>
        </>
      }
    >
      <label className="mb-1 block text-xs text-slate-500 dark:text-zinc-400">
        上下文长度（token）
      </label>
      <input
        className="input w-full tabular-nums"
        type="number"
        min={min}
        max={max}
        step={1}
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === 'Enter' && error == null && !unchanged) void save(parsed);
        }}
        placeholder="留空 = 跟随上游"
        disabled={saving}
        autoFocus
      />
      <p
        className={`mt-1 text-[11px] leading-4 ${error ? 'text-red-500' : 'text-slate-400 dark:text-zinc-500'}`}
      >
        {error ??
          (parsed == null
            ? '当前输入为空：保存后跟随上游（请求不带 context_length）'
            : `将下发 parameters.context_length = ${parsed.toLocaleString()}`)}
        {catalogContext != null && !error && `；目录声明上下文 ${catalogContext.toLocaleString()}`}
      </p>

      <ul className="mt-3 list-disc space-y-1 rounded-lg border border-slate-200 bg-slate-50/80 py-2 pl-7 pr-3 text-xs leading-5 text-slate-500 dark:border-zinc-800 dark:bg-zinc-800/40 dark:text-zinc-400">
        <li>
          <span className="font-medium text-slate-600 dark:text-zinc-300">合法范围：</span>
          {min.toLocaleString()} ~ {max.toLocaleString()} 的整数（单位 token，例如 1M 填 1000000），范围外的值不会保存。
        </li>
        <li>
          <span className="font-medium text-slate-600 dark:text-zinc-300">不填 = 跟随上游：</span>
          请求不带该字段，由 Qoder 按模型默认处理，与从未设置时完全一致。
        </li>
        <li>
          <span className="font-medium text-slate-600 dark:text-zinc-300">生效范围：</span>
          只作用于经 Qoder 池的该模型请求（请求体 <code>parameters.context_length</code>），保存即对下一请求生效；
          不影响 Trae 等其他池。
        </li>
        <li>
          <span className="font-medium text-slate-600 dark:text-zinc-300">上游返回 400 时：</span>
          Qoder 未公开可接受的取值，填写的值可能被拒绝，此时该模型请求会失败并返回 400（错误信息会注明本设置）。
          恢复方法：回到这里点「恢复跟随上游」（或清空输入后保存），下一请求即恢复正常；也可以换一个更常见的值（如 200000）再试。
        </li>
      </ul>
    </Modal>
  );
}
