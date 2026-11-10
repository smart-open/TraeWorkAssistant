/**
 * 全局 API 管理 · 指纹清洗（wb_template_map 规则表维护）
 *
 * 转发前将第三方客户端身份声明（Claude Code / Codex / Cline / Roo / OpenCode /
 * DeepSeek Harness 等）按规则改写为中性表述，规避上游渠道级指纹风控
 * （cat-and-mouse：上游黑名单逐字匹配，任何一字改动即绕过）。
 *
 * 数据流：wb_template_map KV（合并语义：内置默认表始终生效，本表为增量覆盖/
 * 追加）→ load_templates 每请求读取，保存即对下一请求生效，无需重启网关。
 * 保存空 templates 数组 = 恢复内置默认；to=null（停用）= 从生效表移除该规则；
 * to=""（留空）= 剥离（改写为空串，删除指纹原文）。
 * builtin=true 表示尚无自定义副本、当前生效内置默认规则（后端 get 回显）。
 *
 * 档位分区（issue #129：§4.3 分区清洗）：WB 池与 buddy-ai 国际档各自独立一张
 * 自定义表（内置默认表共享），切档必须重新拉取——此前只操作 wb 键导致国际档
 * 完全没生效。
 */
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Eraser, FileSignature, Plus, RotateCcw, Save, ShieldCheck, Wand2 } from 'lucide-react';
import { Badge, Spinner } from '../ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { parsePrefix, parseSystemText, sampleTier } from './fingerprintSamples';
import type {
  FingerprintSample,
  SanitizeRule,
  SanitizeSwitches,
  SanitizeTier,
} from '../../types';

/** 时间戳 → 本地时间展示；null/0 = 从未自定义 */
const fmtTs = (ts: number | null) =>
  ts && ts > 0 ? new Date(ts * 1000).toLocaleString() : null;

/** 规则行（稳定 key 包装，issue #150 引入）：key 自增分配、与规则内容无关——
 * 编辑 from 或删除中间行时 React 按 key 复用 DOM，受控输入焦点不串行 */
type RuleRow = { key: number; rule: SanitizeRule };

export default function FingerprintSanitizePanel() {
  const toast = useAppStore((s) => s.pushToast);
  // 规则行（稳定 key 包装，见 RuleRow 注记）
  const [rows, setRows] = useState<RuleRow[]>([]);
  // 行 key 自增序列（跨 load/添加唯一，不随规则内容变化）
  const rowSeq = useRef(0);
  const [savedRules, setSavedRules] = useState<SanitizeRule[]>([]);
  const [builtin, setBuiltin] = useState(true);
  const [savedAt, setSavedAt] = useState<number | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [confirmReset, setConfirmReset] = useState(false);
  // 档位分区（issue #129 §4.3）：默认 wb 档；切换档位重新拉取对应档的自定
  // 义表（后端按档位键隔离存储）
  const [tier, setTier] = useState<SanitizeTier>('wb');
  // 指纹样本（网关命中采样）与「一键生成规则」的整句/前缀两档策略
  const [samples, setSamples] = useState<FingerprintSample[]>([]);
  // 池级清洗开关（issue #158 P1-4）：wb_sanitize（wb+Trae）与 qoder_sanitize
  // 独立热切换；null = 加载失败（服务未运行/后端不可达），开关置灰按默认开展示
  const [switches, setSwitches] = useState<SanitizeSwitches | null>(null);
  const [samplesOpen, setSamplesOpen] = useState(false);
  const [sampleErr, setSampleErr] = useState<string | null>(null);
  // 二次确认态：恢复默认 / 切换档位（丢弃未保存编辑）/ 清空样本（删除类）
  // 三个动作共用 3s 内再点一次才生效的口径，避免误触不可逆操作
  const [confirmClear, setConfirmClear] = useState(false);
  const [pendingTier, setPendingTier] = useState<SanitizeTier | null>(null);
  // 二次确认倒计时句柄：重复点击需先撤销旧定时器，卸载必须清理
  const confirmTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const clearTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const switchTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(
    () => () => {
      if (confirmTimer.current) clearTimeout(confirmTimer.current);
      if (clearTimer.current) clearTimeout(clearTimer.current);
      if (switchTimer.current) clearTimeout(switchTimer.current);
    },
    [],
  );

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const view = await api.wbConfig.templateMapGet(tier);
      setRows(view.templates.map((rule) => ({ key: ++rowSeq.current, rule })));
      setSavedRules(view.templates);
      setBuiltin(view.builtin ?? false);
      setSavedAt(view.updated_at);
    } catch (e) {
      toast('error', `加载清洗规则失败：${e}`);
    } finally {
      setLoading(false);
    }
  }, [toast, tier]);

  useEffect(() => {
    void load();
  }, [load]);

  /** 指纹样本：从网关命中采样拉取（内存环形缓冲，重启即空） */
  const loadSamples = useCallback(async () => {
    try {
      setSamples(await api.wbConfig.fingerprintSamplesGet());
      setSampleErr(null);
    } catch (e) {
      const msg = `加载指纹样本失败：${e}`;
      setSampleErr(msg);
      // 面板收起时内联错误不可见，写操作反馈必须直达用户
      toast('error', msg);
    }
  }, [toast]);

  useEffect(() => {
    void loadSamples();
  }, [loadSamples]);

  const loadSwitches = useCallback(async () => {
    try {
      setSwitches(await api.wbConfig.sanitizeSwitchesGet());
    } catch {
      setSwitches(null);
    }
  }, []);

  /** 开关热切换（issue #158 P1-4）：乐观更新 + 失败回滚；即时生效无保存按钮 */
  const toggleSwitch = async (pool: 'wb' | 'qoder', enabled: boolean) => {
    const prev = switches;
    setSwitches((s) => (s ? { ...s, [pool]: enabled } : s));
    try {
      await api.wbConfig.sanitizeSwitchSet(pool, enabled);
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
    void loadSwitches();
  }, [loadSwitches]);

  const dirty = useMemo(
    () => JSON.stringify(rows.map((r) => r.rule)) !== JSON.stringify(savedRules),
    [rows, savedRules],
  );

  /** 当前档位可用的样本数（样本按来源池归属档位，跨档位生成规则永不命中） */
  const samplesUsableCount = useMemo(
    () => samples.filter((s) => sampleTier(s.pool) === tier).length,
    [samples, tier],
  );

  /**
   * 档位切换：切档会重新拉取该档规则表，未保存的编辑必然被丢弃——脏状态下
   * 需二次确认（否则用户随手切档即丢一屏编辑，且丢的是另一档的数据）。
   */
  const clickTier = (t: SanitizeTier) => {
    if (t === tier) return;
    if (switchTimer.current) clearTimeout(switchTimer.current);
    if (dirty && pendingTier !== t) {
      setPendingTier(t);
      switchTimer.current = setTimeout(() => {
        setPendingTier(null);
        switchTimer.current = null;
      }, 3000);
      return;
    }
    switchTimer.current = null;
    setPendingTier(null);
    setTier(t);
  };

  /** 清空样本（删除类）：3s 内再点一次才真正执行 */
  const clickClearSamples = () => {
    if (clearTimer.current) clearTimeout(clearTimer.current);
    if (confirmClear) {
      clearTimer.current = null;
      setConfirmClear(false);
      void clearSamples();
      return;
    }
    setConfirmClear(true);
    clearTimer.current = setTimeout(() => {
      setConfirmClear(false);
      clearTimer.current = null;
    }, 3000);
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
      await api.wbConfig.templateMapSet(
        tier,
        rows.map((r) => ({ from: r.rule.from.trim(), to: r.rule.to === null ? null : r.rule.to.trim() })),
      );
      await load();
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
      await api.wbConfig.templateMapSet(tier, []);
      await load();
      toast('success', '已恢复内置默认规则，下一条请求即生效');
    } catch (e) {
      toast('error', `恢复默认失败：${e}`);
    } finally {
      setSaving(false);
    }
  };

  /** 清空样本缓冲：仅清内存环形缓冲，不影响 app.log 已落盘的样本行 */
  const clearSamples = async () => {
    try {
      await api.wbConfig.fingerprintSamplesClear();
      setSamples([]);
      setSampleErr(null);
      toast('success', '指纹样本已清空（仅内存缓冲，日志样本行不受影响）');
    } catch (e) {
      setSampleErr(`清空样本失败：${e}`);
      toast('error', `清空指纹样本失败：${e}`);
    }
  };

  /**
   * 从样本一键生成规则草案：整句录入（from = 首条 system 完整文本，to = 空串即
   * 剥离），或前缀模式（from = 首英文逗号/句号前整段）——上游逐字匹配的是整句
   * （含工具清单），只截取一部分会让改写落在半句上，规则命中不可预期。
   */
  const addFromSample = (sample: string, whole: boolean) => {
    try {
      const from = whole ? parseSystemText(sample) : parsePrefix(sample);
      // 整句档空 system 样本（system="" 或仅省略号截断）解析得空串：生成 from=""
      // 草稿规则会被后端装配层 fail-safe 剔除，这里前置拦截并给出明确提示
      if (!from) {
        setSampleErr('样本未包含系统提示内容，无法生成规则草案');
        return;
      }
      if (rows.some((row) => row.rule.from.trim() === from)) {
        setSampleErr('该指纹的规则已存在，未重复添加');
        return;
      }
      setSampleErr(null);
      setRows((rs) => [...rs, { key: ++rowSeq.current, rule: { from, to: '' } }]);
      toast('success', '已生成规则草案：请填写改写文本后保存');
    } catch (e) {
      setSampleErr(e instanceof Error ? e.message : String(e));
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

      {/* 档位切换（issue #129 §4.3 分区清洗：WB 池与 buddy-ai 国际档规则表独立） */}
      <div className="flex items-center gap-1 rounded-md border border-slate-200 p-0.5 text-xs dark:border-zinc-700">
        {(['wb', 'buddy-ai'] as SanitizeTier[]).map((t) => (
          <button
            key={t}
            onClick={() => clickTier(t)}
            title={pendingTier === t ? '当前档位有未保存的规则，再点一次将丢弃' : undefined}
            className={`flex-1 rounded px-2 py-1 transition ${
              tier === t
                ? 'bg-brand-500 font-medium text-white'
                : pendingTier === t
                  ? 'bg-amber-100 font-medium text-amber-700 dark:bg-amber-500/20 dark:text-amber-300'
                  : 'text-slate-500 hover:bg-slate-50 dark:text-zinc-400 dark:hover:bg-zinc-800'
            }`}
          >
            {pendingTier === t
              ? '确认切换？'
              : t === 'wb'
                ? 'WB 池档位'
                : 'buddy-ai 国际档'}
          </button>
        ))}
      </div>

      {/* 操作条 */}
      <div className="flex items-center gap-2">
        <span className="text-sm font-medium">规则列表</span>
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
                  onChange={(e) => {
                    if (e.target.checked) {
                      updateRow(i, { to: null });
                    } else {
                      // 取消停用：恢复保存时的原改写值（停用瞬间 to 已置 null，直接置 '' 会抹掉原值）；
                      // 无存档可依（新增行 / from 已改动 / 保存时即为停用）则回退剥离语义
                      const saved = savedRules.find((s) => s.from === r.from);
                      updateRow(i, { to: saved?.to ?? '' });
                    }
                  }}
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

      {/* 指纹样本：网关命中采样 → 一键生成规则草案（闭环最省力的一环） */}
      <div className="rounded-lg border border-slate-200 dark:border-zinc-700">
        <button
          onClick={() => setSamplesOpen((v) => !v)}
          className="flex w-full items-center gap-2 px-3 py-2 text-left text-xs text-slate-600 dark:text-zinc-300"
        >
          <FileSignature size={14} className="shrink-0 text-brand-500" />
          <span className="font-medium">指纹样本</span>
          <Badge tone="slate">{samples.length}</Badge>
          <span className="ml-auto text-slate-400">{samplesOpen ? '收起' : '展开'}</span>
        </button>
        {samplesOpen && (
          <div className="space-y-1.5 border-t border-slate-200 px-3 py-2 dark:border-zinc-700">
            {samples.length === 0 ? (
              <p className="text-xs text-slate-400">
                暂无样本。上游命中指纹拦截（11128）或空完成影子风控后，网关会自动采样入内存缓冲（上限
                50 条，重启清空）。
              </p>
            ) : samplesUsableCount === 0 ? (
              <p className="text-[11px] text-amber-600 dark:text-amber-400">
                缓冲内 {samples.length} 条样本均不属于当前档位（或该池不做模板清洗），
                暂不可生成规则——切到对应档位，或等待本档命中后刷新。
              </p>
            ) : null}
            {samples.length > 0 && (
              samples.map((s, idx) => {
                const ts = s.ts > 0 ? new Date(s.ts * 1000).toLocaleTimeString() : '';
                // 样本归属档位：规则表按档位隔离，跨档位生成的规则永不命中
                const sTier = sampleTier(s.pool);
                const usable = sTier === tier;
                const blockReason =
                  sTier === null
                    ? '该池请求不做模板清洗，样本仅供诊断'
                    : `该样本属 ${sTier === 'wb' ? 'WB 池档位' : 'buddy-ai 国际档'}，与当前档位规则表不匹配`;
                return (
                  <div
                    key={`${s.pool}-${s.ts}-${idx}`}
                    className="rounded-md border border-slate-200 px-2 py-1.5 dark:border-zinc-700"
                  >
                    <div className="flex items-center gap-1.5 text-[11px] text-slate-400">
                      {/* kind 三分支（issue #195 第二轮）：11128 显式拦截 / 11128_cleaned 清洗后样本
                          （进出对比，生成规则按钮隐藏——仅供诊断对照）/ empty 空完成影子风控 */}
                      <Badge tone={s.kind === '11128' ? 'red' : s.kind === '11128_cleaned' ? 'violet' : 'amber'}>
                        {s.kind === '11128' ? '11128' : s.kind === '11128_cleaned' ? '11128 清洗后' : '空完成'}
                      </Badge>
                      <span>{s.pool}</span>
                      {ts && <span>{ts}</span>}
                      {s.note && <span className="truncate">{s.note}</span>}
                      <span className="ml-auto flex shrink-0 gap-1">
                        {s.kind !== '11128_cleaned' && (
                          <>
                            <button
                              onClick={() => addFromSample(s.sample, false)}
                              disabled={!usable}
                              title={
                                usable ? '以首英文逗号/句号前的整段作为原文指纹' : blockReason
                              }
                              className="rounded p-1 text-brand-500 transition hover:bg-brand-50 disabled:cursor-not-allowed disabled:text-slate-300 disabled:hover:bg-transparent dark:hover:bg-brand-900/20 dark:disabled:text-zinc-600"
                            >
                              <Wand2 size={13} />
                            </button>
                            <button
                              onClick={() => addFromSample(s.sample, true)}
                              disabled={!usable}
                              title={usable ? '以整条 system 文本作为原文指纹' : blockReason}
                              className="rounded p-1 text-brand-500 transition hover:bg-brand-50 disabled:cursor-not-allowed disabled:text-slate-300 disabled:hover:bg-transparent dark:hover:bg-brand-900/20 dark:disabled:text-zinc-600"
                            >
                              <FileSignature size={13} />
                            </button>
                          </>
                        )}
                      </span>
                    </div>
                    <p className="mt-1 break-all font-mono text-[11px] text-slate-500 dark:text-zinc-400">
                      {s.sample}
                    </p>
                  </div>
                );
              })
            )}
            {sampleErr && <p className="text-xs text-rose-500">{sampleErr}</p>}
            <div className="flex items-center gap-2 pt-1">
              <button
                onClick={() => void loadSamples()}
                className="flex items-center gap-1 rounded-md border border-slate-200 px-2 py-1 text-xs text-slate-600 transition hover:bg-slate-50 dark:border-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-800"
              >
                刷新
              </button>
              <button
                onClick={clickClearSamples}
                disabled={samples.length === 0}
                title={confirmClear ? '再点一次确认清空（仅内存缓冲）' : '清空内存样本缓冲'}
                className={`flex items-center gap-1 rounded-md border px-2 py-1 text-xs transition disabled:opacity-40 ${
                  confirmClear
                    ? 'border-rose-400 bg-rose-50 font-medium text-rose-600 dark:border-rose-500/50 dark:bg-rose-500/10 dark:text-rose-300'
                    : 'border-slate-200 text-slate-600 hover:bg-slate-50 dark:border-zinc-700 dark:text-zinc-300 dark:hover:bg-zinc-800'
                }`}
              >
                <Eraser size={13} />
                {confirmClear ? '确认清空？' : '清空'}
              </button>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
