/**
 * 全局 API 管理 · 指纹清洗（wb_template_map 规则表维护）
 *
 * 转发前将第三方客户端身份声明（Claude Code / Codex / Cline / Roo / OpenCode /
 * DeepSeek Harness 等）按规则改写为中性表述，规避上游渠道级指纹风控
 * （cat-and-mouse：上游黑名单逐字匹配，任何一字改动即绕过）。
 *
 * 数据流：wb_template_map KV（整体替换语义）→ load_templates 每请求读取，
 * 保存即对下一请求生效，无需重启网关。
 * 恢复内置默认 = 保存空 templates 数组（into_rules 全空 → 回退 default_table）。
 * builtin=true 表示当前生效内置默认规则、尚无自定义副本（后端 get 回显）。
 */
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Plus, RotateCcw, Save, ShieldCheck } from 'lucide-react';
import { Badge, Spinner } from '../ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import type { SanitizeRule } from '../../types';

/** 时间戳 → 本地时间展示；null/0 = 从未自定义 */
const fmtTs = (ts: number | null) =>
  ts && ts > 0 ? new Date(ts * 1000).toLocaleString() : null;

export default function FingerprintSanitizePanel() {
  const toast = useAppStore((s) => s.pushToast);
  const [rules, setRules] = useState<SanitizeRule[]>([]);
  const [savedRules, setSavedRules] = useState<SanitizeRule[]>([]);
  const [builtin, setBuiltin] = useState(true);
  const [savedAt, setSavedAt] = useState<number | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [confirmReset, setConfirmReset] = useState(false);
  // 「恢复内置默认」二次确认倒计时句柄：重复点击需先撤销旧定时器，卸载必须清理
  const confirmTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(
    () => () => {
      if (confirmTimer.current) clearTimeout(confirmTimer.current);
    },
    [],
  );

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const view = await api.apiServer.templateMapGet();
      setRules(view.templates);
      setSavedRules(view.templates);
      setBuiltin(view.builtin ?? false);
      setSavedAt(view.updated_at);
    } catch (e) {
      toast('error', `加载清洗规则失败：${e}`);
    } finally {
      setLoading(false);
    }
  }, [toast]);

  useEffect(() => {
    void load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const dirty = useMemo(
    () => JSON.stringify(rules) !== JSON.stringify(savedRules),
    [rules, savedRules],
  );

  /** 保存前置校验：from 必填且全局唯一（from 是改写锚点，重复会导致不可预期次序） */
  const validate = (): string | null => {
    const seen = new Set<string>();
    for (let i = 0; i < rules.length; i++) {
      const from = rules[i].from.trim();
      if (!from) return `第 ${i + 1} 条规则的「原文指纹」不能为空`;
      if (seen.has(from)) return `「原文指纹」重复：${from.slice(0, 40)}…`;
      seen.add(from);
    }
    return null;
  };

  const save = async () => {
    const err = validate();
    if (err) {
      toast('warn', err);
      return;
    }
    setSaving(true);
    try {
      await api.apiServer.templateMapSet(rules.map((r) => ({ from: r.from.trim(), to: r.to.trim() })));
      await load();
      toast('success', '清洗规则已保存，下一条请求即生效');
    } catch (e) {
      toast('error', `保存失败：${e}`);
    } finally {
      setSaving(false);
    }
  };

  /** 恢复内置默认：保存空数组（into_rules 全空 → 回退 default_template_map） */
  const resetToBuiltin = async () => {
    setConfirmReset(false);
    setSaving(true);
    try {
      await api.apiServer.templateMapSet([]);
      await load();
      toast('success', '已恢复内置默认规则，下一条请求即生效');
    } catch (e) {
      toast('error', `恢复默认失败：${e}`);
    } finally {
      setSaving(false);
    }
  };

  const updateRow = (i: number, patch: Partial<SanitizeRule>) => {
    setRules((rs) => rs.map((r, k) => (k === i ? { ...r, ...patch } : r)));
  };

  if (loading) {
    return (
      <div className="flex h-40 items-center justify-center">
        <Spinner />
      </div>
    );
  }

  return (
    <div className="space-y-3">
      {/* 说明条：功能定位 + 生效语义 */}
      <div className="flex items-start gap-2 rounded-lg border border-brand-300/60 bg-brand-50/70 px-3 py-2 text-xs text-brand-800 dark:border-brand-700/40 dark:bg-brand-500/10 dark:text-brand-200">
        <ShieldCheck size={16} className="mt-0.5 shrink-0" />
        <div>
          <p>
            转发前将第三方客户端身份声明（Claude Code / Codex / Cline / DeepSeek Harness
            等）改写为中性表述，规避上游渠道级指纹风控。规则为「原文 → 改写」子串替换
            （逐字匹配，无正则），按列表顺序应用。
          </p>
          <p className="mt-1 opacity-80">
            保存即对下一条请求生效，无需重启网关；
            {builtin ? '当前生效内置默认规则，保存后成为独立自定义副本。' : '当前为自定义规则副本。'}
            {savedAt && !builtin && ` 最近保存：${fmtTs(savedAt)}`}
          </p>
        </div>
      </div>

      {/* 操作条 */}
      <div className="flex items-center gap-2">
        <span className="text-sm font-medium">规则列表</span>
        <Badge tone={builtin ? 'slate' : 'blue'}>{builtin ? '内置默认' : '自定义'}</Badge>
        <span className="text-xs text-slate-400">{rules.length} 条</span>
        <div className="ml-auto flex items-center gap-2">
          <button
            onClick={() => {
              if (confirmTimer.current) clearTimeout(confirmTimer.current);
              if (confirmReset) {
                confirmTimer.current = null;
                void resetToBuiltin();
              } else {
                setConfirmReset(true);
                confirmTimer.current = setTimeout(() => {
                  setConfirmReset(false);
                  confirmTimer.current = null;
                }, 3000);
              }
            }}
            disabled={saving || (builtin && !dirty)}
            className="flex items-center gap-1 rounded-md border border-slate-200 px-2.5 py-1.5 text-xs text-slate-600 transition hover:bg-slate-50 disabled:opacity-40 dark:border-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-800"
          >
            <RotateCcw size={13} />
            {confirmReset ? '确认恢复？' : '恢复内置默认'}
          </button>
          <button
            onClick={() => void save()}
            disabled={saving || !dirty}
            className="flex items-center gap-1 rounded-md bg-brand-500 px-3 py-1.5 text-xs font-medium text-white transition hover:bg-brand-600 disabled:opacity-40"
          >
            {saving ? <Spinner className="h-3.5 w-3.5" /> : <Save size={13} />}
            保存
          </button>
        </div>
      </div>

      {/* 规则行 */}
      <div className="space-y-1.5">
        {rules.map((r, i) => (
          <div key={i} className="flex items-center gap-2">
            <span className="w-6 shrink-0 text-right text-xs tabular-nums text-slate-400">{i + 1}</span>
            <input
              value={r.from}
              onChange={(e) => updateRow(i, { from: e.target.value })}
              placeholder="原文指纹（逐字子串，如 You are …）"
              spellCheck={false}
              className="min-w-0 flex-[2] rounded-md border border-slate-200 bg-white px-2 py-1.5 font-mono text-xs text-slate-700 outline-none focus:border-brand-400 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
            />
            <span className="shrink-0 text-xs text-slate-400">→</span>
            <input
              value={r.to}
              onChange={(e) => updateRow(i, { to: e.target.value })}
              placeholder="改写为（留空 = 删除该指纹）"
              spellCheck={false}
              className="min-w-0 flex-[2] rounded-md border border-slate-200 bg-white px-2 py-1.5 font-mono text-xs text-slate-700 outline-none focus:border-brand-400 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
            />
            <button
              onClick={() => setRules((rs) => rs.filter((_, k) => k !== i))}
              className="shrink-0 rounded-md px-2 py-1 text-xs text-rose-500 transition hover:bg-rose-50 dark:hover:bg-rose-900/20"
            >
              删除
            </button>
          </div>
        ))}
        <button
          onClick={() => setRules((rs) => [...rs, { from: '', to: '' }])}
          className="flex w-full items-center justify-center gap-1 rounded-md border border-dashed border-slate-300 py-2 text-xs text-slate-500 transition hover:border-brand-400 hover:text-brand-500 dark:border-zinc-700 dark:text-zinc-400"
        >
          <Plus size={13} />
          添加规则
        </button>
      </div>
    </div>
  );
}
