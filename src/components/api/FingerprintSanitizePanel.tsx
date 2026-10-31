/**
 * 全局 API 管理 · 指纹清洗（wb_template_map 规则表维护）
 *
 * 转发前将第三方客户端身份声明（Claude Code / Codex / Cline / Roo / OpenCode /
 * DeepSeek Harness / TraeCode 等）按规则改写为中性表述，规避上游渠道级指纹风控
 * （cat-and-mouse：上游黑名单逐字匹配，任何一字改动即绕过）。
 *
 * 数据流：wb_template_map KV（合并语义：内置默认表始终生效，本表为增量覆盖/
 * 追加）→ load_templates 每请求读取，保存即对下一请求生效，无需重启网关。
 * 保存空 templates 数组 = 恢复内置默认；to=null（停用）= 从生效表移除该规则；
 * to=""（留空）= 剥离（改写为空串，删除指纹原文）。
 * builtin=true 表示尚无自定义副本、当前生效内置默认规则（后端 get 回显）。
 *
 * issue #129：档位分区——wb 池与 Trae 池共用 wb_template_map 键，Buddy-ai
 * 国际档读独立键 buddy_ai_template_map（§4.3 分区清洗）；内置默认表两档共享，
 * 自定义覆盖表相互隔离。指纹样本按来源池自动归档写键。
 */
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Plus, RotateCcw, Save, ShieldCheck, Wand2 } from 'lucide-react';
import { Badge, Spinner } from '../ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import type {
  FingerprintSample,
  SanitizeRule,
  SanitizeSwitches,
  SanitizeTier,
} from '../../types';
import { extractFingerprintFrom, neutralRewrite } from './fingerprintSamples';

/** 时间戳 → 本地时间展示；null/0 = 从未自定义 */
const fmtTs = (ts: number | null) =>
  ts && ts > 0 ? new Date(ts * 1000).toLocaleString() : null;

/** 档位切换项：值 = SanitizeTier，label = UI 展示 */
const TIER_OPTIONS: Array<[SanitizeTier, string]> = [
  ['wb', 'Buddy / Trae 池'],
  ['buddy-ai', 'Buddy-ai 国际档'],
];

/** 规则行（稳定 key 包装）：key 自增分配、与规则内容无关——编辑 from 或删除
 * 中间行时 React 按 key 复用 DOM，受控输入焦点不串行 */
type RuleRow = { key: number; rule: SanitizeRule };

export default function FingerprintSanitizePanel() {
  const toast = useAppStore((s) => s.pushToast);
  // 清洗规则档位（issue #129）：wb 池与 Trae 池共用键 | buddy-ai 国际档独立键
  const [tier, setTier] = useState<SanitizeTier>('wb');
  // 规则行（稳定 key 包装，见 RuleRow 注记）
  const [rows, setRows] = useState<RuleRow[]>([]);
  const [savedRules, setSavedRules] = useState<SanitizeRule[]>([]);
  const [builtin, setBuiltin] = useState(true);
  const [savedAt, setSavedAt] = useState<number | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [confirmReset, setConfirmReset] = useState(false);
  // 「清空样本」二次确认（同 confirmReset 内联两段式模式，清样本属破坏性操作）
  const [confirmClear, setConfirmClear] = useState(false);
  // 指纹样本（issue #125 增强）：网关命中（11128 拦截 / 空完成影子风控）自动采集，
  // 内存环形缓冲（最新在前），重启清空——持久反查靠 app.log
  const [samples, setSamples] = useState<FingerprintSample[]>([]);
  // 池级清洗开关（issue #158 P1-4）：wb_sanitize（wb+Trae）与 qoder_sanitize
  // 独立热切换；null = 加载失败（服务未运行/后端不可达），开关置灰按默认开展示
  const [switches, setSwitches] = useState<SanitizeSwitches | null>(null);
  // 「恢复内置默认」二次确认倒计时句柄：重复点击需先撤销旧定时器，卸载必须清理
  const confirmTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  // 「清空样本」二次确认倒计时句柄（独立定时器，避免与 confirmReset 互相干扰）
  const confirmClearTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  // applySample 切档后需读最新规则（setRows 异步、闭包持旧值），ref 镜像最新值；
  // 除 effect 同步外，load() 内也会显式回写（审查问题1：await load 返回与 effect
  // 提交时序无保证，切档后立即读取不能依赖 effect 已执行）
  const rulesRef = useRef<SanitizeRule[]>([]);
  // 行 key 自增序列（跨 load/添加唯一，不随规则内容变化）
  const rowSeq = useRef(0);

  useEffect(
    () => () => {
      if (confirmTimer.current) clearTimeout(confirmTimer.current);
      if (confirmClearTimer.current) clearTimeout(confirmClearTimer.current);
    },
    [],
  );

  const load = useCallback(async (t: SanitizeTier) => {
    setLoading(true);
    try {
      const view = await api.apiServer.templateMapGet(t);
      setRows(view.templates.map((rule) => ({ key: ++rowSeq.current, rule })));
      // 显式同步（不等 effect）：applySample 切档后立即读 ref 做重复校验
      rulesRef.current = view.templates;
      setSavedRules(view.templates);
      setBuiltin(view.builtin ?? false);
      setSavedAt(view.updated_at);
    } catch (e) {
      toast('error', `加载清洗规则失败：${e}`);
    } finally {
      setLoading(false);
    }
  }, [toast]);

  const loadSamples = useCallback(async () => {
    try {
      setSamples(await api.apiServer.fingerprintSamplesGet());
    } catch {
      // 样本为诊断辅助信息，加载失败不打扰规则主流程
    }
  }, []);

  const loadSwitches = useCallback(async () => {
    try {
      setSwitches(await api.apiServer.sanitizeSwitchesGet());
    } catch {
      setSwitches(null);
    }
  }, []);

  /** 开关热切换（issue #158 P1-4）：乐观更新 + 失败回滚；即时生效无保存按钮 */
  const toggleSwitch = async (pool: 'wb' | 'qoder', enabled: boolean) => {
    const prev = switches;
    setSwitches((s) => (s ? { ...s, [pool]: enabled } : s));
    try {
      await api.apiServer.sanitizeSwitchSet(pool, enabled);
      toast(
        'success',
        `已${enabled ? '开启' : '关闭'}${pool === 'qoder' ? ' Qoder' : ' Buddy/Trae'}池指纹清洗，下一条请求即生效`,
      );
    } catch (e) {
      setSwitches(prev);
      toast('error', `切换清洗开关失败：${e}`);
    }
  };

  useEffect(() => {
    void load('wb');
    void loadSamples();
    void loadSwitches();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const clearSamples = async () => {
    try {
      await api.apiServer.fingerprintSamplesClear();
      setSamples([]);
    } catch (e) {
      toast('error', `清空样本失败：${e}`);
    }
  };

  const dirty = useMemo(
    () => JSON.stringify(rows.map((r) => r.rule)) !== JSON.stringify(savedRules),
    [rows, savedRules],
  );

  // rulesRef 声明在组件顶部（load 内也回写，见审查问题1）；此处兜底同步
  // applySample 切档外路径的 rules 变化（增删行、编辑等）
  useEffect(() => {
    rulesRef.current = rows.map((r) => r.rule);
  }, [rows]);

  /** 切换档位（issue #129）：dirty 时阻断切换防丢改动，切换后重载对应档位规则；
   * 返回是否就绪（applySample 依赖此前置） */
  const switchTier = async (next: SanitizeTier) => {
    if (next === tier) return true;
    if (dirty) {
      toast('warn', '当前档位有未保存改动，请先保存或还原后再切换');
      return false;
    }
    setTier(next);
    await load(next);
    return true;
  };

  /** 一键从样本生成规则：from 预填样本首句（harness 身份声明句，可编辑），
   * to 走中性改写（未命中回退剥离）。from 必须逐字匹配原文，多行长句
   * （样本经空白压缩）可能失配——预填后用户可校对编辑。
   * issue #129：按样本来源池归档——buddy-ai 池样本写国际档独立键并自动切档，
   * 其余池样本写 wb/Trae 共用键 */
  const applySample = async (s: FingerprintSample) => {
    const from = extractFingerprintFrom(s.sample);
    if (!from) {
      toast('warn', '样本不可解析或不含 system 文本，无法生成规则');
      return;
    }
    const target: SanitizeTier = s.pool === 'buddy-ai' ? 'buddy-ai' : 'wb';
    if (!(await switchTier(target))) return;
    if (rulesRef.current.some((r) => r.from.trim() === from)) {
      toast('warn', `规则已存在，无需重复添加：${from.slice(0, 40)}…`);
      return;
    }
    const to = neutralRewrite(from);
    setRows((rs) => [...rs, { key: ++rowSeq.current, rule: { from, to } }]);
    if (to) {
      toast(
        'success',
        `已按样本预填${target === 'buddy-ai' ? '国际档' : ''}规则（from 可编辑校对），保存后生效`,
      );
    } else {
      toast('warn', '未命中已知句式，已预填剥离规则（to 为空=整句删除），请确认后再保存');
    }
  };

  /** 保存前置校验：from 必填且全局唯一（from 是改写锚点，重复会导致不可预期次序） */
  const validate = (): string | null => {
    const seen = new Set<string>();
    for (let i = 0; i < rows.length; i++) {
      const from = rows[i].rule.from.trim();
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
      // to=null（停用）原样落库；to=""（剥离）保留空串，两者不可混淆
      await api.apiServer.templateMapSet(
        rows.map((r) => ({ from: r.rule.from.trim(), to: r.rule.to === null ? null : r.rule.to.trim() })),
        tier,
      );
      await load(tier);
      toast('success', '清洗规则已保存，下一条请求即生效');
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
      await api.apiServer.templateMapSet([], tier);
      await load(tier);
      toast('success', '已恢复内置默认规则，下一条请求即生效');
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
        <ShieldCheck size={16} className="mt-0.5 shrink-0" />
        <div>
          <p>
            转发前将第三方客户端身份声明（Claude Code / Codex / Cline / DeepSeek Harness
            等）改写为中性表述，规避上游渠道级指纹风控。规则为「原文 → 改写」子串替换
            （逐字匹配，无正则），按列表顺序应用。
          </p>
          <p className="mt-1 opacity-80">
            wb/Trae 池共用一份规则表，Buddy-ai 国际档独立一份（内置默认表两档共享，自定义相互隔离）。
            规则表为增量覆盖：内置默认表始终生效，此处可覆盖、追加或停用（停用 =
            从生效表移除该规则，原句不再改写）。保存即对下一条请求生效，无需重启网关；
            {builtin ? '当前生效内置默认规则，保存后成为自定义覆盖副本。' : '当前为自定义覆盖副本（默认表仍在其下生效）。'}
            {savedAt && !builtin && ` 最近保存：${fmtTs(savedAt)}`}
          </p>
        </div>
      </div>

      {/* 池级清洗开关（issue #158 P1-4）：按池独立启停，热切换下一请求即生效；
          switches=null（加载失败/服务未运行）置灰按默认开展示 */}
      <div className="rounded-lg border border-slate-200 px-3 py-2 dark:border-zinc-700">
        <div className="flex items-center gap-2">
          <span className="text-sm font-medium">池级开关</span>
          <span className="truncate text-xs text-slate-400">
            按池独立启停指纹清洗与工具别名改写，热切换、下一请求即生效；重启网关复位为开启
          </span>
          {/* 网关可能在面板挂载后才启动：给一个刷新入口，避免开关一直置灰（置灰原因见下方提示） */}
          <button
            onClick={() => void loadSwitches()}
            className="ml-auto shrink-0 rounded-md border border-slate-200 px-2 py-1 text-xs text-slate-600 transition hover:bg-slate-50 dark:border-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-800"
          >
            刷新
          </button>
        </div>
        {switches === null && (
          <p className="mt-1 text-[11px] leading-4 text-amber-600 dark:text-amber-400">
            网关未运行或开关读取失败：已按默认（开启）展示并置灰，启动 API 服务后点「刷新」即可操作
          </p>
        )}
        <div className="mt-1.5 flex flex-col gap-0.5">
          <label
            className={`flex items-start gap-2.5 rounded-md px-1.5 py-1 transition hover:bg-slate-100/60 dark:hover:bg-zinc-800/60 ${switches ? 'cursor-pointer' : 'cursor-not-allowed opacity-60'}`}
          >
            <input
              type="checkbox"
              className="mt-0.5 h-3.5 w-3.5 rounded border-slate-300 text-brand-600 focus:ring-brand-500"
              checked={switches?.wb ?? true}
              disabled={!switches}
              onChange={(e) => void toggleSwitch('wb', e.target.checked)}
            />
            <span className="min-w-0">
              <span className="block text-xs text-slate-700 dark:text-zinc-200">
                Buddy / Trae 池
              </span>
              <span className="block text-[11px] leading-4 text-slate-400 dark:text-zinc-500">
                共享开关（wb_sanitize）：身份声明改写 + 工具别名；关闭后原名透传（排障用）
              </span>
            </span>
          </label>
          <label
            className={`flex items-start gap-2.5 rounded-md px-1.5 py-1 transition hover:bg-slate-100/60 dark:hover:bg-zinc-800/60 ${switches ? 'cursor-pointer' : 'cursor-not-allowed opacity-60'}`}
          >
            <input
              type="checkbox"
              className="mt-0.5 h-3.5 w-3.5 rounded border-slate-300 text-brand-600 focus:ring-brand-500"
              checked={switches?.qoder ?? true}
              disabled={!switches}
              onChange={(e) => void toggleSwitch('qoder', e.target.checked)}
            />
            <span className="min-w-0">
              <span className="block text-xs text-slate-700 dark:text-zinc-200">Qoder 池</span>
              <span className="block text-[11px] leading-4 text-slate-400 dark:text-zinc-500">
                独立开关（qoder_sanitize，issue #158）：空完成影子风控治理管线总闸，与上方开关互不影响
              </span>
            </span>
          </label>
        </div>
      </div>

      {/* 指纹样本（issue #125 增强）：命中自动采集，一键预填规则 */}
      <div className="rounded-lg border border-slate-200 px-3 py-2 dark:border-zinc-700">
        <div className="flex items-center gap-2">
          <span className="text-sm font-medium">指纹样本</span>
          <Badge tone={samples.length ? 'violet' : 'slate'}>{samples.length} 条</Badge>
          <span className="truncate text-xs text-slate-400">
            网关命中（11128 拦截 / 空完成影子风控）自动采集，仅内存保存、重启清空
          </span>
          <div className="ml-auto flex shrink-0 items-center gap-2">
            <button
              onClick={() => void loadSamples()}
              className="rounded-md border border-slate-200 px-2 py-1 text-xs text-slate-600 transition hover:bg-slate-50 dark:border-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-800"
            >
              刷新
            </button>
            <button
              onClick={() => {
                if (confirmClearTimer.current) clearTimeout(confirmClearTimer.current);
                if (confirmClear) {
                  confirmClearTimer.current = null;
                  setConfirmClear(false);
                  void clearSamples();
                } else {
                  setConfirmClear(true);
                  confirmClearTimer.current = setTimeout(() => {
                    setConfirmClear(false);
                    confirmClearTimer.current = null;
                  }, 3000);
                }
              }}
              disabled={!samples.length}
              className="rounded-md px-2 py-1 text-xs text-rose-500 transition hover:bg-rose-50 disabled:opacity-40 dark:hover:bg-rose-900/20"
            >
              {confirmClear ? '确认清空？' : '清空'}
            </button>
          </div>
        </div>
        {samples.length === 0 ? (
          <p className="mt-1.5 text-xs text-slate-400">
            暂无样本。触发风控后自动落下，点「生成规则」一键预填清洗规则（from 为样本首句，可编辑校对）。
          </p>
        ) : (
          <div className="mt-1.5 space-y-1">
            {samples.map((s, i) => (
              <div key={`${s.kind}-${s.ts}-${i}`} className="flex items-center gap-2 text-xs">
                <Badge tone={s.kind === '11128' ? 'red' : 'amber'}>
                  {s.kind === '11128' ? '11128 拦截' : '空完成'}
                </Badge>
                <Badge tone="slate">{s.pool}</Badge>
                <span className="shrink-0 tabular-nums text-slate-400">{fmtTs(s.ts)}</span>
                <code
                  className="min-w-0 flex-1 truncate font-mono text-slate-600 dark:text-zinc-300"
                  title={s.note ? `${s.sample}\n${s.note}` : s.sample}
                >
                  {s.sample}
                </code>
                {s.note && <Badge tone="blue">已命中</Badge>}
                <button
                  onClick={() => void applySample(s)}
                  className="flex shrink-0 items-center gap-1 rounded-md border border-violet-300 px-2 py-1 text-violet-600 transition hover:bg-violet-50 dark:border-violet-700/50 dark:text-violet-300 dark:hover:bg-violet-900/20"
                >
                  <Wand2 size={12} />
                  生成规则
                </button>
              </div>
            ))}
          </div>
        )}
      </div>

      {/* 操作条 */}
      <div className="flex items-center gap-2">
        <span className="text-sm font-medium">规则列表</span>
        {/* 档位切换（issue #129）：wb/Trae 池共用一份规则，buddy-ai 国际档独立键 */}
        <div className="flex overflow-hidden rounded-md border border-slate-200 text-xs dark:border-zinc-700">
          {TIER_OPTIONS.map(([t, label]) => (
            <button
              key={t}
              onClick={() => void switchTier(t)}
              disabled={saving}
              className={`px-2.5 py-1.5 transition disabled:opacity-40 ${
                tier === t
                  ? 'bg-brand-500 font-medium text-white'
                  : 'text-slate-600 hover:bg-slate-50 dark:text-zinc-300 dark:hover:bg-zinc-800'
              }`}
            >
              {label}
            </button>
          ))}
        </div>
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
                placeholder="原文指纹（逐字子串，如 You are …）"
                spellCheck={false}
                className="min-w-0 flex-[2] rounded-md border border-slate-200 bg-white px-2 py-1.5 font-mono text-xs text-slate-700 outline-none focus:border-brand-400 disabled:bg-slate-50 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200 dark:disabled:bg-zinc-800"
              />
              <span className="shrink-0 text-xs text-slate-400">→</span>
              <input
                value={disabled ? '' : (r.to ?? '')}
                onChange={(e) => updateRow(i, { to: e.target.value })}
                placeholder={disabled ? '已停用（该规则不参与生效表）' : '改写为（留空 = 剥离指纹原文）'}
                disabled={disabled}
                spellCheck={false}
                className="min-w-0 flex-[2] rounded-md border border-slate-200 bg-white px-2 py-1.5 font-mono text-xs text-slate-700 outline-none focus:border-brand-400 disabled:bg-slate-50 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200 dark:disabled:bg-zinc-800"
              />
              <label
                className="flex shrink-0 cursor-pointer select-none items-center gap-1 text-xs text-slate-500 dark:text-zinc-400"
                title={disabled ? '停用：从生效表移除该规则（原句透传）' : '停用该规则'}
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
          添加规则
        </button>
      </div>
    </div>
  );
}
