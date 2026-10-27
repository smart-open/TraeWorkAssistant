/**
 * 全局 API 管理 · 工具别名（tool_alias_map 规则表维护，issue #150）
 *
 * 上游风控升级为按请求 `tools` 工具名集合识别 agent 客户端——system 句表清洗
 * 后样本 system 已空仍被拦，换号无效（拦「客户端形态」不拦账号）。工具名是
 * function calling 协议定义，客户端按名匹配响应 tool_calls，单向改名即废；
 * 本表在请求侧把特征工具名改写为语义保持的通用别名（模型仍可凭描述选对工具），
 * 响应侧按逆映射自动回改（协议对客户端透明）。
 *
 * 数据流：tool_alias_map KV（合并语义：内置默认表始终生效，本表为增量覆盖/
 * 追加）→ load_tool_alias_rules 每请求读取，保存即对下一请求生效，无需重启
 * 网关。保存空 rules 数组 = 恢复内置默认；to=null（停用）= 从生效表移除该规则，
 * 原名透传。注意与「指纹清洗」不同：别名不支持留空剥离（to=""），别名必须
 * 非空且请求/响应双向一致，不支持多候选。
 */
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { ArrowLeftRight, Plus, RotateCcw, Save } from 'lucide-react';
import { Badge, Spinner } from '../ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import type { SanitizeRule } from '../../types';

/** 时间戳 → 本地时间展示；null/0 = 从未自定义 */
const fmtTs = (ts: number | null) =>
  ts && ts > 0 ? new Date(ts * 1000).toLocaleString() : null;

/** 规则行（稳定 key 包装）：key 自增分配、与规则内容无关——编辑 from 或删除
 * 中间行时 React 按 key 复用 DOM，受控输入焦点不串行（同指纹清洗面板口径） */
type RuleRow = { key: number; rule: SanitizeRule };

export default function ToolAliasPanel() {
  const toast = useAppStore((s) => s.pushToast);
  // 规则行（稳定 key 包装，见 RuleRow 注记）
  const [rows, setRows] = useState<RuleRow[]>([]);
  const rowSeq = useRef(0);
  const [savedRules, setSavedRules] = useState<SanitizeRule[]>([]);
  const [builtin, setBuiltin] = useState(true);
  const [savedAt, setSavedAt] = useState<number | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  // 「恢复内置默认」两段式确认（同指纹清洗面板内联模式）
  const [confirmReset, setConfirmReset] = useState(false);
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
      const view = await api.apiServer.toolAliasMapGet();
      setRows(view.rules.map((rule) => ({ key: ++rowSeq.current, rule })));
      setSavedRules(view.rules);
      setBuiltin(view.builtin ?? false);
      setSavedAt(view.updated_at);
    } catch (e) {
      toast('error', `加载工具别名失败：${e}`);
    } finally {
      setLoading(false);
    }
  }, [toast]);

  useEffect(() => {
    void load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const dirty = useMemo(
    () => JSON.stringify(rows.map((r) => r.rule)) !== JSON.stringify(savedRules),
    [rows, savedRules],
  );

  /** 保存前置校验：from 必填且唯一；未停用行 to 必填非空（别名不可为空串，
   * 后端落库层同样拒绝——工具名改写为空会破坏 function calling 协议） */
  const validate = (): string | null => {
    const seen = new Set<string>();
    for (let i = 0; i < rows.length; i++) {
      const r = rows[i].rule;
      const from = r.from.trim();
      if (!from) return `第 ${i + 1} 条规则的「原工具名」不能为空`;
      if (seen.has(from)) return `「原工具名」重复：${from}`;
      seen.add(from);
      if (r.to !== null && !r.to.trim()) return `「${from}」的别名不能为空（停用请勾选停用）`;
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
      await api.apiServer.toolAliasMapSet(
        rows.map((r) => ({
          from: r.rule.from.trim(),
          to: r.rule.to === null ? null : r.rule.to.trim(),
        })),
      );
      await load();
      toast('success', '工具别名已保存，下一条请求即生效');
    } catch (e) {
      toast('error', `保存失败：${e}`);
    } finally {
      setSaving(false);
    }
  };

  /** 恢复内置默认：保存空数组（into_rules 全空 → 合并走纯默认表） */
  const resetToBuiltin = async () => {
    setConfirmReset(false);
    setSaving(true);
    try {
      await api.apiServer.toolAliasMapSet([]);
      await load();
      toast('success', '已恢复内置默认别名，下一条请求即生效');
    } catch (e) {
      toast('error', `恢复默认失败：${e}`);
    } finally {
      setSaving(false);
    }
  };

  const updateRow = (i: number, patch: Partial<SanitizeRule>) => {
    setRows((rs) =>
      rs.map((row, k) => (k === i ? { ...row, rule: { ...row.rule, ...patch } } : row)),
    );
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
        <ArrowLeftRight size={16} className="mt-0.5 shrink-0" />
        <div>
          <p>
            上游风控会按请求 tools 中的工具名集合识别 agent 客户端（换账号无效）。此处把特征工具名
            （如 ask_user_question）改写为通用别名（如 ask_user），响应侧自动回改原名，对客户端透明。
          </p>
          <p className="mt-1 opacity-80">
            仅收特定客户端独家命名，glob/grep 等通用名不改。规则为逐字精确匹配（无正则）；
            别名必须非空且请求/响应双向一致，不支持留空剥离与多候选；别名不得与请求中
            其他真实工具名重名（重名时该规则自动跳过、原名透传）。当前仅对 Trae 池
            （SOLO 管线）请求生效。规则表为增量覆盖：
            内置默认表始终生效，此处可覆盖、追加或停用（停用 = 原名透传）。保存即对下一条请求生效，
            无需重启网关；
            {builtin ? '当前生效内置默认表，保存后成为自定义覆盖副本。' : '当前为自定义覆盖副本（默认表仍在其下生效）。'}
            {savedAt && !builtin && ` 最近保存：${fmtTs(savedAt)}`}
          </p>
        </div>
      </div>

      {/* 操作条 */}
      <div className="flex items-center gap-2">
        <span className="text-sm font-medium">别名规则</span>
        <Badge tone={builtin ? 'slate' : 'blue'}>{builtin ? '内置默认' : '自定义'}</Badge>
        <span className="text-xs text-slate-400">{rows.length} 条</span>
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
        {rows.map(({ key, rule: r }, i) => {
          const disabled = r.to === null;
          return (
            <div key={key} className={`flex items-center gap-2 ${disabled ? 'opacity-60' : ''}`}>
              <span className="w-6 shrink-0 text-right text-xs tabular-nums text-slate-400">{i + 1}</span>
              <input
                value={r.from}
                onChange={(e) => updateRow(i, { from: e.target.value })}
                placeholder="原工具名（逐字匹配请求 tools 定义，如 ask_user_question）"
                spellCheck={false}
                className="min-w-0 flex-[2] rounded-md border border-slate-200 bg-white px-2 py-1.5 font-mono text-xs text-slate-700 outline-none focus:border-brand-400 disabled:bg-slate-50 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200 dark:disabled:bg-zinc-800"
              />
              <span className="shrink-0 text-xs text-slate-400">→</span>
              <input
                value={disabled ? '' : (r.to ?? '')}
                onChange={(e) => updateRow(i, { to: e.target.value })}
                placeholder={disabled ? '已停用（原名透传，不做改写）' : '通用别名（如 ask_user）'}
                disabled={disabled}
                spellCheck={false}
                className="min-w-0 flex-1 rounded-md border border-slate-200 bg-white px-2 py-1.5 font-mono text-xs text-slate-700 outline-none focus:border-brand-400 disabled:bg-slate-50 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200 dark:disabled:bg-zinc-800"
              />
              <label
                className="flex shrink-0 cursor-pointer select-none items-center gap-1 text-xs text-slate-500 dark:text-zinc-400"
                title={disabled ? '停用：从生效表移除该规则（原名透传）' : '停用该规则'}
              >
                <input
                  type="checkbox"
                  checked={disabled}
                  onChange={(e) => updateRow(i, { to: e.target.checked ? null : '' })}
                  className="accent-rose-500"
                />
                停用
              </label>
              <button
                onClick={() => setRows((rs) => rs.filter((_, k) => k !== i))}
                className="shrink-0 rounded-md px-2 py-1 text-xs text-rose-500 transition hover:bg-rose-50 dark:hover:bg-rose-900/20"
              >
                删除
              </button>
            </div>
          );
        })}
        <button
          onClick={() =>
            setRows((rs) => [...rs, { key: ++rowSeq.current, rule: { from: '', to: '' } }])
          }
          className="flex w-full items-center justify-center gap-1 rounded-md border border-dashed border-slate-300 py-2 text-xs text-slate-500 transition hover:border-brand-400 hover:text-brand-500 dark:border-zinc-700 dark:text-zinc-400"
        >
          <Plus size={13} />
          添加别名
        </button>
      </div>
    </div>
  );
}
