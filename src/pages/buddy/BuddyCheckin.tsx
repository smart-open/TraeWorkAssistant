import { useCallback, useEffect, useRef, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import { CheckCircle2, PlayCircle, Sparkles, XCircle } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { Badge } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import type { WbCheckinRecord, WorkBuddyAccountView, WorkBuddySettings } from '../../types';

/**
 * buddy-checkin 签到与成长（§3.7.3，F-15/F-16/F-55）：
 * 签到控制卡（NDJSON 进度）+ 成长中心卡。
 * 近 30 天签到日志在概述页；定时任务与坐标点击兜底在环境配置「签到配置」面板。
 */

interface WbAccountLine {
  index: number;
  user_id: string;
  name: string;
  status: 'success' | 'already' | 'fail';
  message?: string;
  reward?: number;
}

interface WbGrowthLine {
  type: 'growth';
  index: number;
  user_id: string;
  name: string;
  status: 'ok' | 'fail';
  message?: string;
  travel?: string;
  lottery?: string;
  tasks?: string;
  energy?: number;
  streak?: number;
}

type ParsedEvent =
  | { type: 'start'; total: number; mode?: string }
  | { type: 'done'; ok: number; already: number; failed: number; mode?: string }
  | { type: 'exit' }
  | WbAccountLine
  | WbGrowthLine;

/** 能量/连签标量归一：接口可能返回 {current:..} 等嵌套对象（修复「连签 [object Object] 天」展示） */
function scalarNum(v: unknown, depth = 0): number | null {
  if (typeof v === 'number') return isFinite(v) ? v : null;
  if (typeof v === 'string' && v.trim() !== '') {
    const n = Number(v);
    return isNaN(n) ? null : n;
  }
  if (v && typeof v === 'object' && depth < 3) {
    for (const k of ['current', 'days', 'count', 'value', 'num', 'total', 'streak', 'energy']) {
      const n = scalarNum((v as Record<string, unknown>)[k], depth + 1);
      if (n != null) return n;
    }
  }
  return null;
}

function parseLine(raw: string): ParsedEvent | null {
  try {
    return JSON.parse(raw);
  } catch {
    return null;
  }
}

const statusText: Record<string, string> = {
  success: '成功',
  already: '已签',
  fail: '失败',
};

/** 登录态徽标：needs_relogin 优先，其次按 access_token 剩余时长分色（对齐 Trae MiniJwtBadge 形态） */
function MiniTokenBadge({ a }: { a: WorkBuddyAccountView }) {
  if (a.needs_relogin)
    return <span className="text-xs text-rose-500"><XCircle size={11} className="inline" /> 需重新登录</span>;
  const exp = a.access_token_expires_at;
  if (!exp) return <span className="text-xs text-slate-400">未知</span>;
  const hours = (exp - Math.floor(Date.now() / 1000)) / 3600;
  if (hours <= 0) return <span className="text-xs text-rose-500"><XCircle size={11} className="inline" /> 已过期</span>;
  if (hours <= 24) return <span className="text-xs text-amber-500">{hours.toFixed(1)}h</span>;
  return <span className="text-xs text-emerald-500">{hours.toFixed(0)}h</span>;
}

export default function BuddyCheckin() {
  const pushToast = useAppStore((s) => s.pushToast);
  const [accounts, setAccounts] = useState<WorkBuddyAccountView[]>([]);
  const [running, setRunning] = useState(false);
  const [lines, setLines] = useState<WbAccountLine[]>([]);
  const [doneInfo, setDoneInfo] = useState<{ ok: number; already: number; failed: number } | null>(null);
  const [settings, setSettings] = useState<WorkBuddySettings | null>(null);
  // 今日签到记录（user_id → 记录数组，供列表签到状态/获取积分列展示）
  const [checkinMap, setCheckinMap] = useState<Map<string, WbCheckinRecord[]>>(new Map());
  const [growthRunning, setGrowthRunning] = useState(false);
  const [growthLines, setGrowthLines] = useState<WbGrowthLine[]>([]);
  const [growthSummary, setGrowthSummary] = useState<string | null>(null);
  const unlistenRef = useRef<(() => void) | null>(null);

  const refresh = useCallback(async () => {
    try {
      const [accs, st, recs] = await Promise.all([
        api.workbuddy.accountsList().catch(() => [] as WorkBuddyAccountView[]),
        api.workbuddy.settingsGet().catch(() => null),
        api.workbuddy
          .checkinResults(1)
          .catch(() => [] as WbCheckinRecord[]),
      ]);
      setAccounts(accs);
      setSettings(st);
      // 仅保留今天的记录（days=1 会含昨日）
      const today = new Date();
      const todayStr = `${today.getFullYear()}-${String(today.getMonth() + 1).padStart(2, '0')}-${String(today.getDate()).padStart(2, '0')}`;
      const m = new Map<string, WbCheckinRecord[]>();
      for (const r of recs) {
        if (r.date !== todayStr) continue;
        const arr = m.get(r.user_id) ?? [];
        arr.push(r);
        m.set(r.user_id, arr);
      }
      setCheckinMap(m);
    } catch (err) {
      pushToast('error', `读取签到数据失败：${String(err)}`);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    void refresh();
    let disposed = false;
    void listen<string>('wb-checkin-progress', (ev) => {
      const parsed = parseLine(ev.payload);
      if (!parsed || typeof parsed !== 'object') return;
      if ('type' in parsed && parsed.type === 'start') {
        // 成长中心与签到共用管线：按 mode 分流，互不干扰
        if (parsed.mode === 'growth') {
          setGrowthLines([]);
          setGrowthSummary(null);
          setGrowthRunning(true);
        } else {
          setLines([]);
          setDoneInfo(null);
        }
      } else if ('type' in parsed && parsed.type === 'done') {
        if (parsed.mode === 'growth') {
          setGrowthSummary('成长中心执行完成');
          setGrowthRunning(false);
          void refresh();
          pushToast('success', '成长中心执行完成（旅行/盲盒/任务结果见下方明细）');
        } else {
          setDoneInfo({ ok: parsed.ok, already: parsed.already, failed: parsed.failed });
          setRunning(false);
          void refresh();
          pushToast(parsed.failed > 0 ? 'warn' : 'success', `WorkBuddy 签到完成：成功 ${parsed.ok}，已签 ${parsed.already}，失败 ${parsed.failed}`);
        }
      } else if ('type' in parsed && parsed.type === 'growth') {
        const line = parsed as WbGrowthLine;
        // 能量/连签归一为标量（接口返回嵌套对象时防 [object Object] 展示）
        const energy = scalarNum(line.energy);
        const streak = scalarNum(line.streak);
        setGrowthLines((prev) => {
          const next = prev.slice();
          next[line.index - 1] = { ...line, energy: energy ?? undefined, streak: streak ?? undefined };
          return next;
        });
      } else if ('index' in parsed && parsed.index != null) {
        const line = parsed as WbAccountLine;
        const reward = scalarNum(line.reward);
        setLines((prev) => {
          const next = prev.slice();
          next[line.index - 1] = { ...line, reward: reward ?? undefined };
          return next;
        });
      } else if ('type' in parsed && parsed.type === 'exit') {
        setRunning(false);
        setGrowthRunning(false);
      }
    }).then((u) => {
      if (disposed) u();
      else unlistenRef.current = u;
    });
    return () => {
      disposed = true;
      unlistenRef.current?.();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const startCheckin = async () => {
    if (accounts.length === 0) {
      pushToast('warn', '账号池为空：请先在「账号管理」导入本机账号');
      return;
    }
    setRunning(true);
    setLines([]);
    setDoneInfo(null);
    try {
      await api.workbuddy.checkinStart({ skip_checked_in: true, skip_expired: false });
    } catch (err) {
      setRunning(false);
      pushToast('error', `发起签到失败：${String(err)}`);
    }
  };

  const runGrowth = async () => {
    if (accounts.length === 0) {
      pushToast('warn', '账号池为空：请先在「账号管理」导入本机账号');
      return;
    }
    setGrowthRunning(true);
    setGrowthLines([]);
    setGrowthSummary(null);
    try {
      await api.workbuddy.growthRun();
    } catch (err) {
      setGrowthRunning(false);
      pushToast('error', `发起成长任务失败：${String(err)}`);
    }
  };

  const saveSettings = async (patch: Partial<NonNullable<typeof settings>>) => {
    if (!settings) return;
    const next = { ...settings, ...patch };
    setSettings(next);
    try {
      await api.workbuddy.settingsSet(next);
    } catch (err) {
      pushToast('error', `保存设置失败：${String(err)}`);
    }
  };

  // 本轮累计获得积分（汇总徽标展示；防浮点求和误差）
  const earned = Math.round(lines.reduce((s, l) => s + (l.reward ?? 0), 0) * 100) / 100;

  // 实时结果按 user_id 索引（运行中覆盖列表列展示）
  const liveById = new Map(lines.map((l) => [l.user_id, l] as const));
  // 今日累计获取积分：成功记录求和（重试多轮累加；无成功记录时取最近一条含奖励记录）
  const todayEarnedOf = (uid: string): number | null => {
    const recs = checkinMap.get(uid) ?? [];
    const sum = recs.reduce((s, r) => s + (r.status === 'success' && r.reward != null ? r.reward : 0), 0);
    if (sum > 0) return Math.round(sum * 100) / 100;
    return recs.find((r) => r.reward != null)?.reward ?? null; // recs 新→旧序，find 即最新
  };
  // 今日签到状态（无实时行时按记录推导：任一成功/已签 → 已签；否则最近一条失败）
  const checkinStatusOf = (uid: string): 'success' | 'already' | 'fail' | null => {
    const live = liveById.get(uid);
    if (live) return live.status;
    const recs = checkinMap.get(uid) ?? [];
    if (recs.some((r) => r.status === 'success' || r.status === 'already')) {
      return recs.some((r) => r.status === 'success') ? 'success' : 'already';
    }
    return recs.find((r) => r.status === 'fail') ? 'fail' : null; // 新→旧序，find 即最近一次失败
  };

  return (
    <div className="animate-fade-in">
      <PageHeader title="Buddy · 签到与成长" desc="一键签到 · 成长中心 · 定时任务见环境配置" />

      {/* 成长中心卡（F-17：三开关 + 立即执行，T2.5 执行器）——与签到进度卡上下互换后置顶 */}
      <div className="card p-4">
        <div className="mb-3 flex items-center justify-between">
          <div className="flex items-center gap-2">
            <Sparkles size={16} className="text-violet-500" />
            <span className="text-sm font-medium">成长中心</span>
            {growthRunning && <Badge tone="blue">执行中</Badge>}
            {growthSummary && !growthRunning && <span className="text-xs text-slate-400">{growthSummary}</span>}
          </div>
          <button className="btn-outline !px-3 !py-1 text-xs" onClick={() => void runGrowth()} disabled={growthRunning}>
            <Sparkles size={13} /> {growthRunning ? '执行中…' : '立即执行成长任务'}
          </button>
        </div>
        <p className="mb-3 text-xs text-slate-400">
          Buddy 旅行 / 盲盒 / 任务领奖为纯增量积分自动化，按下方开关逐账号链式执行；奖励数额以接口返回为准。
        </p>
        <div className="grid gap-3 lg:grid-cols-3">
          <label className="flex items-start gap-2 rounded-lg border border-slate-100 p-3 dark:border-zinc-800">
            <input
              type="checkbox"
              className="mt-0.5"
              checked={settings?.growth_travel ?? true}
              onChange={(e) => void saveSettings({ growth_travel: e.target.checked })}
            />
            <span className="text-sm">
              Buddy 旅行
              <span className="block text-xs text-slate-400">到达自动领奖并再次出发</span>
            </span>
          </label>
          <label className="flex items-start gap-2 rounded-lg border border-slate-100 p-3 dark:border-zinc-800">
            <input
              type="checkbox"
              className="mt-0.5"
              checked={settings?.growth_lottery ?? true}
              onChange={(e) => void saveSettings({ growth_lottery: e.target.checked })}
            />
            <span className="text-sm">
              盲盒抽取
              <span className="block text-xs text-slate-400">消耗剩余次数自动抽取</span>
            </span>
          </label>
          <label className="flex items-start gap-2 rounded-lg border border-slate-100 p-3 dark:border-zinc-800">
            <input
              type="checkbox"
              className="mt-0.5"
              checked={settings?.growth_tasks ?? true}
              onChange={(e) => void saveSettings({ growth_tasks: e.target.checked })}
            />
            <span className="text-sm">
              任务领奖
              <span className="block text-xs text-slate-400">自动领取已完成任务奖励</span>
            </span>
          </label>
        </div>
        {/* 成长任务执行明细 */}
        {growthLines.length > 0 && (
          <div className="mt-3 space-y-1.5">
            {growthLines.map((g, i) => (
              <div key={i} className="rounded-lg border border-slate-100 px-3 py-2 text-sm dark:border-zinc-800">
                <div className="flex items-center gap-3">
                  <Badge tone={g.status === 'fail' ? 'red' : 'green'}>{g.status === 'fail' ? '失败' : '完成'}</Badge>
                  <span className="min-w-0 flex-1 truncate font-medium">{g.name || g.user_id}</span>
                  {(g.energy != null || g.streak != null) && (
                    <span className="text-xs text-violet-500">
                      {g.energy != null && `能量 ${g.energy}`}
                      {g.energy != null && g.streak != null && ' · '}
                      {g.streak != null && `连签 ${g.streak} 天`}
                    </span>
                  )}
                </div>
                <div className="mt-1 flex flex-wrap gap-x-4 gap-y-0.5 text-xs text-slate-400">
                  {g.travel && <span>旅行：{g.travel}</span>}
                  {g.lottery && <span>盲盒：{g.lottery}</span>}
                  {g.tasks && <span>任务：{g.tasks}</span>}
                  {g.status === 'fail' && g.message && <span className="text-red-500">{g.message}</span>}
                </div>
              </div>
            ))}
          </div>
        )}
      </div>

      {/* 一键签到卡（对齐 Trae 一键签到形态：账号列表表格 + 按钮右下角；实时进度移到底部独立卡） */}
      <div className="mt-4 card p-4">
        <div className="mb-3 flex items-center justify-between">
          <div className="flex items-center gap-2">
            <span className="text-sm font-medium">一键签到</span>
            {running && <Badge tone="blue">执行中</Badge>}
          </div>
          {accounts.length > 0 && !running && (
            <span className="text-xs text-slate-400">已签账号自动跳过</span>
          )}
        </div>

        {/* 参与签到的账号列表（账号 / 登录态 / 会员等级 / 签到状态 / 获取积分 / 可用总积分） */}
        <div className="rounded-lg border border-slate-200 dark:border-zinc-700">
          <table className="w-full text-sm">
            <thead className="bg-slate-50 text-xs text-slate-500 dark:bg-zinc-900">
              <tr>
                <th className="px-3 py-1.5 text-left">账号</th>
                <th className="px-3 py-1.5 text-left">登录态</th>
                <th className="px-3 py-1.5 text-left">会员等级</th>
                <th className="px-3 py-1.5 text-left">签到状态</th>
                <th className="px-3 py-1.5 text-right">获取积分</th>
                <th className="px-3 py-1.5 text-right">可用总积分</th>
              </tr>
            </thead>
            <tbody>
              {accounts.length === 0 ? (
                <tr>
                  <td colSpan={6} className="px-3 py-4 text-center text-xs text-slate-400">
                    暂无账号：请先在「账号管理」导入本机账号
                  </td>
                </tr>
              ) : (
                accounts.map((a) => {
                  const st = checkinStatusOf(a.id);
                  const earn = liveById.get(a.id)?.reward ?? todayEarnedOf(a.id);
                  return (
                    <tr key={a.id} className="border-t border-slate-100 dark:border-zinc-800">
                      <td className="px-3 py-1.5">
                        <div className="font-medium">{a.nickname || a.uid}</div>
                        <div className="text-xs text-slate-400">{a.phone_masked || a.id}</div>
                      </td>
                      <td className="px-3 py-1.5"><MiniTokenBadge a={a} /></td>
                      <td className="px-3 py-1.5 text-xs text-slate-500">
                        {a.edition_type ? (
                          a.edition_type.toLowerCase() === 'pro' ? (
                            <span className="font-medium text-sky-600 dark:text-sky-400">{a.edition_type}</span>
                          ) : (
                            a.edition_type
                          )
                        ) : (
                          <span className="text-slate-300 dark:text-zinc-600">—</span>
                        )}
                      </td>
                      <td className="px-3 py-1.5">
                        {st == null ? (
                          <Badge tone="slate">未签</Badge>
                        ) : st === 'success' ? (
                          <Badge tone="green">已签</Badge>
                        ) : st === 'already' ? (
                          <Badge tone="blue">已签（此前已签）</Badge>
                        ) : (
                          <Badge tone="red">失败</Badge>
                        )}
                      </td>
                      <td className="px-3 py-1.5 text-right tabular-nums text-xs">
                        {earn != null ? (
                          <span className="text-emerald-600 dark:text-emerald-400">+{earn}</span>
                        ) : (
                          <span className="text-slate-300 dark:text-zinc-600">—</span>
                        )}
                      </td>
                      <td className="px-3 py-1.5 text-right tabular-nums text-xs">
                        {a.credits_balance != null ? a.credits_balance.toLocaleString() : '-'}
                      </td>
                    </tr>
                  );
                })
              )}
            </tbody>
          </table>
        </div>

        {/* 操作区：按钮右下角（对齐 Trae） */}
        <div className="mt-3 flex items-center justify-between">
          <span className="text-xs text-slate-400">
            {running ? '签到进行中，逐账号结果见下方实时进度…' : '签到结果与获取积分将展示在下方实时进度卡'}
          </span>
          <button className="btn-outline" onClick={() => void startCheckin()} disabled={running}>
            <PlayCircle size={15} /> {running ? '签到中…' : '开始签到'}
          </button>
        </div>
      </div>

      {/* 实时进度卡（对齐 Trae：标题 + 汇总徽标 + 逐账号结果行；结束后保留避免提示一闪而过） */}
      {(running || lines.length > 0) && (
        <div className="mt-4 card p-4">
          <div className="mb-3 flex items-center justify-between">
            <h3 className="font-medium">实时进度</h3>
            <div className="flex items-center gap-3">
              {running ? (
                <Badge tone="blue">运行中</Badge>
              ) : doneInfo ? (
                <Badge tone={doneInfo.failed > 0 ? 'amber' : 'green'}>
                  完成：成功 {doneInfo.ok} · 已签 {doneInfo.already} · 失败 {doneInfo.failed}
                  {earned > 0 && ` · 获得 ${earned} 积分`}
                </Badge>
              ) : null}
            </div>
          </div>
          <div className="space-y-1">
            {lines.map((l, i) => {
              const tone =
                l.status === 'success'
                  ? 'text-emerald-600 dark:text-emerald-300'
                  : l.status === 'already'
                  ? 'text-sky-600 dark:text-sky-300'
                  : 'text-rose-600 dark:text-rose-300';
              const Icon = l.status === 'fail' ? XCircle : CheckCircle2;
              return (
                <div key={i} className="flex items-center gap-2 rounded border border-slate-200 px-3 py-2 text-sm dark:border-zinc-700">
                  <Icon size={14} className={tone} />
                  <span className="w-8 text-right text-xs text-slate-400">{l.index}</span>
                  <span className="flex-1 truncate">{l.name || l.user_id}</span>
                  <span className={`max-w-[50%] truncate text-xs ${tone}`} title={l.message}>
                    {l.status === 'success' ? (l.message || '签到成功') : (l.message || statusText[l.status])}
                  </span>
                  {l.reward != null && (
                    <span className="shrink-0 rounded bg-emerald-50 px-1.5 py-0.5 text-xs font-medium text-emerald-600 dark:bg-emerald-500/10 dark:text-emerald-300">
                      获取积分 +{l.reward}
                    </span>
                  )}
                </div>
              );
            })}
          </div>
        </div>
      )}
    </div>
  );
}
