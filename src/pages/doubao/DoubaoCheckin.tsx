import { useEffect, useMemo, useRef, useState } from 'react';
import { PlayCircle, Users, CheckCircle2, CircleSlash, XCircle, CalendarDays } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { Badge, Progress } from '../../components/ui';
import CheckinCalendarCard from '../../components/CheckinCalendar';
import type { CheckinCalendarDay } from '../../components/checkinCalendarModel';
import { daysFromDoubaoDetail } from '../../components/checkinCalendarDoubao';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import type { DoubaoAccountView, DoubaoDayDetail, GroupView } from '../../types';

/** 今日日期键（本机时区 YYYY-MM-DD，与后端 doubao_checkin_results::today_key 口径一致） */
function todayKey(): string {
  const d = new Date();
  const p = (n: number) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
}

type Scope = 'all' | 'group' | 'selected';

/** 一键签到逐账号结果行 */
interface ResultLine {
  userId: string;
  name: string;
  status: 'success' | 'already' | 'fail';
  message?: string;
  reward?: number;
}

/** 豆包 · 一键签到（参考 Trae 一键签到页：范围选择 + 账号勾选 + 逐账号结果 + 活动档期日历）。
 *  与 Trae 的 NDJSON 事件管线不同，豆包签到为单次 invoke（无事件流），
 *  自选/分组范围由前端逐账号串行调用实现，天然带逐账号进度。
 *  账号表始终展示全部账号（无凭证账号灰显标注原因），候选集仅用于签到执行与计数。 */
export default function DoubaoCheckin() {
  const pushToast = useAppStore((s) => s.pushToast);
  const setView = useAppStore((s) => s.setView);

  const [accounts, setAccounts] = useState<DoubaoAccountView[]>([]);
  const [groups, setGroups] = useState<GroupView[]>([]);
  const [calDays, setCalDays] = useState<CheckinCalendarDay[]>([]);
  const [loading, setLoading] = useState(false);
  const [scope, setScope] = useState<Scope>('all');
  const [groupId, setGroupId] = useState('');
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [running, setRunning] = useState(false);
  const [lines, setLines] = useState<ResultLine[]>([]);
  const [summary, setSummary] = useState<{ ok: number; failed: number; reward: number } | null>(null);

  // 组件存活标记：切页卸载后终止批量签到循环（单账号 invoke 超时 600s，不守卫会后台继续发真实签到请求；
  // 重进页面是新实例、running 归零，不守卫还会造成并发双跑）
  const aliveRef = useRef(true);
  useEffect(() => {
    // setup 复位必须与 cleanup 配对（2026-11-06 修复）：StrictMode（dev）effect 会按
    // setup→cleanup→setup 双跑，cleanup 把守卫置 false 后若 setup 不复位，aliveRef
    // 恒为 false → run() 在第一个 invoke 前静默 return，而 running 已置 true——
    // 界面永远「运行中/正在签到第 1/3」、后端零请求零落库（生产构建 React 无双跑，不受影响）
    aliveRef.current = true;
    return () => {
      aliveRef.current = false;
    };
  }, []);

  const today = todayKey();

  const reload = async () => {
    setLoading(true);
    try {
      const [accs, gs, det] = await Promise.all([
        api.doubao.accountsList(),
        api.groups.sharedList().catch(() => [] as GroupView[]),
        // 活动档期日历数据源：近 90 天按日逐账号明细（与签到主流程解耦，失败静默）
        api.doubao.resultsDetail(90).catch(() => [] as DoubaoDayDetail[]),
      ]);
      // 卸载守卫（与 run() 同口径，审查 #12）：单账号 invoke 超时 600s，切页后
      // 迟到响应不得 setState / pushToast / setLoading
      if (!aliveRef.current) return;
      setAccounts(accs);
      setGroups(gs);
      setCalDays(daysFromDoubaoDetail(det));
    } catch (err) {
      if (!aliveRef.current) return;
      pushToast('error', `读取豆包账号失败：${String(err)}`);
    } finally {
      if (aliveRef.current) setLoading(false);
    }
  };

  useEffect(() => {
    void reload();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  /** 候选账号：有会话凭证才可在线签到（无凭证账号在表中灰显并说明原因） */
  const candidates = useMemo(() => {
    const eligible = accounts.filter((a) => a.session_state !== 'none');
    if (scope === 'group') return eligible.filter((a) => a.group_id === groupId);
    if (scope === 'selected') return eligible.filter((a) => selected.has(a.user_id));
    return eligible;
  }, [accounts, scope, groupId, selected]);

  /** 可勾选 = 有会话凭证的账号（自选范围用） */
  const selectable = useMemo(() => accounts.filter((a) => a.session_state !== 'none'), [accounts]);

  const toggle = (uid: string) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(uid)) next.delete(uid);
      else next.add(uid);
      return next;
    });
  };

  /** 开始签到：逐账号串行 + 账号间 800ms 间隔（与后端 run_batch ACCOUNT_GAP_MS 对齐，
   *  防连续请求触发频控；签到幂等：当天已签服务端直接返回已签） */
  const GAP_MS = 800;
  const run = async () => {
    if (running || candidates.length === 0) return;
    setRunning(true);
    setLines([]);
    setSummary(null);
    let ok = 0;
    let failed = 0;
    let reward = 0;
    // 失败首因 + probe 保活错误（失败排障透出，2026-11-07）：收尾 toast 不再只报计数——
    // 全失败时用户只看到「失败 N」无法定性（probe 未对齐风控拒 vs 凭证过期 vs 无资格）。
    // probe_keepalive_error 仅首个触发保活的账号响应携带（KEEPALIVE_GATE 串行保活）
    let firstFailMsg = '';
    let probeErr = '';
    for (let i = 0; i < candidates.length; i++) {
      const a = candidates[i];
      if (i > 0) await new Promise((r) => setTimeout(r, GAP_MS));
      // 卸载守卫：每轮 invoke 前校验，切页后立即停止，不再发起后续账号的真实签到请求
      if (!aliveRef.current) return;
      try {
        const r = (await api.doubao.checkinRun(a.user_id)) as {
          outcome?: { status?: string; message?: string; reward?: number };
          probe_keepalive_error?: string | null;
        };
        // 首因语义（审查 L-2）：首个非空值胜——probe_keepalive_error 仅首个触发
        // 保活的账号响应携带，覆盖语义（最后非空值胜）与注释口径矛盾
        if (r.probe_keepalive_error && !probeErr) probeErr = r.probe_keepalive_error;
        const o = r.outcome;
        const st = o?.status;
        if (st === 'fail') {
          failed += 1;
          if (!firstFailMsg) firstFailMsg = (o?.message ?? '').trim();
          setLines((prev) => [...prev, { userId: a.user_id, name: a.name, status: 'fail', message: o?.message }]);
        } else {
          // success / already 均视为当日已签（已签 = 之前已成功，标记成功）
          ok += 1;
          const rw = Number(o?.reward ?? 0) || 0;
          reward += rw;
          setLines((prev) => [
            ...prev,
            { userId: a.user_id, name: a.name, status: st === 'already' ? 'already' : 'success', message: o?.message, reward: rw },
          ]);
        }
      } catch (err) {
        failed += 1;
        if (!firstFailMsg) firstFailMsg = String(err);
        setLines((prev) => [...prev, { userId: a.user_id, name: a.name, status: 'fail', message: String(err) }]);
      }
    }
    // 卸载后跳过全部收尾（组件已销毁，running 无需复位；pushToast/reload 均为多余副作用）
    if (!aliveRef.current) return;
    setSummary({ ok, failed, reward });
    setRunning(false);
    // 失败时透出首因与 probe 保活状态（各自截断防 toast 过长；完整原因见结果列表行
    // 与按日落库 message，重开页面日历明细仍可查）
    const failHint = firstFailMsg ? `；首因：${firstFailMsg.slice(0, 80)}` : '';
    // 仅失败时拼接抓包提示（审查 L-3）：全成功 toast 带「易被风控拒绝请重试」与
    // 成功结果矛盾
    const probeHint = failed > 0 && probeErr ? `；抓包参数未对齐（${probeErr.slice(0, 60)}）——易被风控拒绝，请开启代理并保持豆包客户端打开后重试` : '';
    pushToast(
      failed > 0 ? 'warn' : 'success',
      `签到完成：成功 ${ok} · 失败 ${failed}${reward > 0 ? ` · 新增额度 +${reward.toLocaleString()}` : ''}${failHint}${probeHint}`,
    );
    await reload();
  };

  const lineIcon = (st: ResultLine['status']) => {
    if (st === 'success') return <CheckCircle2 size={14} className="text-emerald-500" />;
    if (st === 'already') return <CircleSlash size={14} className="text-sky-500" />;
    return <XCircle size={14} className="text-rose-500" />;
  };

  const todayBadge = (a: DoubaoAccountView) =>
    a.checkin_last_date === today ? <Badge tone="green">今日已签</Badge> : <Badge tone="slate">今日未签</Badge>;

  return (
    <div className="animate-fade-in">
      <PageHeader title="豆包 · 一键签到" desc="有会话凭证的账号批量完成每日签到 · 成功后自动回写总额度与连续签到天数" />

      {accounts.length === 0 ? (
        <div className="mt-5 card p-8 text-center">
          <p className="text-sm text-slate-500">暂无豆包账号，请先到「账号管理」保存登录态。</p>
          <button onClick={() => setView('doubao-accounts')} className="btn-outline mt-3">
            去账号管理
          </button>
        </div>
      ) : (
        <>
          {/* 风控验证状态横幅（2026-11-06）：显眼提示当前签到成功率仍在反复验证中，避免用户误判为稳定功能 */}
          <div className="mt-4 flex items-start gap-2 rounded-lg border-l-4 border-rose-400 bg-rose-50 p-3 text-xs font-medium text-rose-700 dark:bg-rose-500/10 dark:text-rose-300">
            ⚠ 风控验证中：豆包签到目前上游风控还未完全验证通过，签到可能失败，需要反复验证中。
            失败后可稍后重试，或直接在豆包客户端内完成签到（开启代理时结果会自动落库到本页日历）。
          </div>

          <div className="mt-2 flex items-start gap-2 rounded-lg border border-amber-300 bg-amber-50 p-3 text-xs text-amber-700 dark:border-amber-700 dark:bg-amber-500/10 dark:text-amber-300">
            ⚠ 每日签到走豆包官方 landing/checkin 接口：当天已签幂等跳过；请勿在一天内反复手动执行，避免触发风控。
            每日定时签到可在「环境配置 → 每日签到」开启。
          </div>

          {/* 签到流程依赖链说明（2026-11-06）：提交被上游风控强校验设备形态，
              参数齐备与否直接决定成败——把依赖与自愈路径讲清楚 */}
          <div className="mt-2 rounded-lg border border-slate-200 bg-slate-50 p-3 text-xs leading-relaxed text-slate-500 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-400">
            <span className="font-medium text-slate-600 dark:text-zinc-200">签到流程依赖：</span>
            ① 会话凭证（账号管理「保存当前登录态」或代理自动抓取）→
            ② 设备抓包参数（代理运行时「以账号打开」豆包自动抓取，<b>按账号绑定设备指纹，24 小时有效</b>）→
            ③ 提交签到（带本账号设备指纹 + 设备 Cookie 对齐客户端形态）。
            参数缺失/过期时，签到前会<b>自动保活</b>（设置页可关）：自动启动代理 → 逐账号切换豆包客户端抓取参数（每账号约 30-60 秒，
            客户端会被短暂切换登录态，结束停留在最后处理的账号），全程见日志 <code>[probe-keepalive]</code>。
            若参数齐备仍报 <code>710010702</code>，说明上游要求客户端内一次性反爬令牌（CLI 无法重放）——
            此时请直接在豆包客户端内完成签到（开启代理时结果会自动落库到本页日历）。
          </div>

          {/* 范围与账号选择 */}
          <div className="card mt-4 p-4">
            <div className="mb-3 flex flex-wrap items-center gap-2 text-sm">
              {(
                [
                  ['all', '全部账号'],
                  ['group', '按分组'],
                  ['selected', '自选账号'],
                ] as [Scope, string][]
              ).map(([k, label]) => (
                <button
                  key={k}
                  onClick={() => setScope(k)}
                  disabled={running}
                  className={`chip border disabled:opacity-50 ${scope === k ? 'border-brand-500 text-brand-600' : 'border-slate-300 text-slate-500'}`}
                >
                  {label}
                </button>
              ))}
              {scope === 'group' && (
                <select
                  value={groupId}
                  onChange={(e) => setGroupId(e.target.value)}
                  disabled={running}
                  className="input !w-44 disabled:opacity-50"
                >
                  <option value="">选择分组…</option>
                  {groups.map((g) => (
                    <option key={g.id} value={g.id}>
                      {g.name}
                    </option>
                  ))}
                </select>
              )}
              {scope === 'selected' && (
                <span className="text-xs text-slate-400">
                  已选 {selected.size} / {selectable.length} 个可签账号
                </span>
              )}
              <div className="ml-auto flex items-center gap-2">
                {scope === 'selected' && (
                  <button
                    onClick={() => setSelected(new Set(selectable.map((a) => a.user_id)))}
                    className="btn-outline !py-1 !text-xs"
                    disabled={running}
                  >
                    全选可签
                  </button>
                )}
              </div>
            </div>

            {/* 范围提示（不替代账号表——全部账号始终可见，无凭证灰显） */}
            {scope === 'group' && !groupId && (
              <div className="mb-3 rounded-lg border border-slate-200 bg-slate-50 px-3 py-2 text-xs text-slate-500 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-400">
                请先选择分组（无分组可在「账号管理 → 分组管理」创建）。
              </div>
            )}
            {scope === 'group' && groupId && candidates.length === 0 && (
              <div className="mb-3 rounded-lg border border-slate-200 bg-slate-50 px-3 py-2 text-xs text-slate-500 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-400">
                该分组内没有可签到账号（需要有会话凭证；凭证经代理自动抓取或在账号编辑弹框录入）。
              </div>
            )}
            {scope === 'selected' && selected.size === 0 && (
              <div className="mb-3 rounded-lg border border-slate-200 bg-slate-50 px-3 py-2 text-xs text-slate-500 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-400">
                请在下方勾选要签到的账号（无凭证账号不可勾选）。
              </div>
            )}
            {scope === 'all' && candidates.length === 0 && (
              <div className="mb-3 rounded-lg border border-slate-200 bg-slate-50 px-3 py-2 text-xs text-slate-500 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-400">
                没有可签到账号：全部账号均无会话凭证（凭证经代理自动抓取或在账号编辑弹框录入）。
              </div>
            )}

            <div className="overflow-x-auto">
              <table className="w-full text-sm">
                <thead className="bg-slate-50 text-xs uppercase text-slate-500 dark:bg-zinc-900">
                  <tr>
                    {scope === 'selected' && <th className="w-10 px-3 py-2" />}
                    <th className="whitespace-nowrap px-3 py-2 text-left">账号</th>
                    <th className="whitespace-nowrap px-3 py-2 text-left">会话</th>
                    <th className="whitespace-nowrap px-3 py-2 text-left">今日</th>
                    <th className="whitespace-nowrap px-3 py-2 text-left">额度</th>
                  </tr>
                </thead>
                <tbody>
                  {accounts.map((a) => {
                    const eligible = a.session_state !== 'none';
                    return (
                      <tr
                        key={a.user_id}
                        className={`row-hover border-t border-slate-200 dark:border-zinc-800 ${eligible ? '' : 'opacity-60'}`}
                      >
                        {scope === 'selected' && (
                          <td className="px-3 py-2">
                            <input
                              type="checkbox"
                              checked={selected.has(a.user_id)}
                              onChange={() => toggle(a.user_id)}
                              disabled={!eligible || running}
                              title={eligible ? undefined : '无会话凭证，无法在线签到'}
                              className="h-4 w-4 accent-brand-600"
                            />
                          </td>
                        )}
                        <td className="px-3 py-2">
                          <div className="flex items-center gap-2">
                            <span className="font-medium">{a.name}</span>
                            {a.is_current && <Badge tone="green">当前</Badge>}
                          </div>
                          <div className="font-mono text-xs text-slate-400">{a.user_id}</div>
                        </td>
                        <td className="whitespace-nowrap px-3 py-2 text-xs">
                          {a.session_state === 'none' ? (
                            <Badge tone="slate" title="无会话凭证，无法在线签到">
                              无凭证
                            </Badge>
                          ) : a.session_state === 'expired' ? (
                            <Badge tone="red">已过期</Badge>
                          ) : (
                            <Badge tone={a.session_state === 'ok' ? 'green' : 'amber'}>
                              {a.session_state === 'ok' ? '有效' : '未探活'}
                            </Badge>
                          )}
                        </td>
                        <td className="whitespace-nowrap px-3 py-2">{todayBadge(a)}</td>
                        <td className="whitespace-nowrap px-3 py-2 text-xs tabular-nums">
                          {a.quota_total != null ? (
                            <span className="font-medium text-amber-600 dark:text-amber-400">{a.quota_total.toLocaleString()}</span>
                          ) : (
                            <span className="text-slate-400">—</span>
                          )}
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>

            {/* 开始签到：账号表格右下方、签到进度上方 */}
            <div className="mt-3 flex items-center justify-end gap-3">
              <span className="text-xs text-slate-400">将对上方 {candidates.length} 个候选账号逐个执行</span>
              <button
                onClick={() => void run()}
                className="btn-primary"
                disabled={running || loading || candidates.length === 0}
              >
                {running ? null : <PlayCircle size={15} />}
                {running ? '签到进行中…' : `开始签到（${candidates.length}）`}
              </button>
            </div>
          </div>

          {/* 执行结果（逐账号串行进度 + 汇总） */}
          {(running || lines.length > 0 || summary) && (
            <div className="card mt-4 p-4">
              <div className="mb-2 flex items-center justify-between">
                <div className="flex items-center gap-2">
                  <Users size={16} className="text-brand-500" />
                  <span className="text-sm font-medium">签到进度</span>
                </div>
                {summary ? (
                  <Badge tone={summary.failed > 0 ? 'amber' : 'green'}>
                    完成：成功 {summary.ok} · 失败 {summary.failed}
                    {summary.reward > 0 ? ` · +${summary.reward.toLocaleString()}` : ''}
                  </Badge>
                ) : (
                  <Badge tone="blue">运行中</Badge>
                )}
              </div>
              <Progress value={lines.length} max={candidates.length || 1} className="mb-3" />
              <div className="max-h-64 space-y-1 overflow-auto">
                {lines.map((l, i) => (
                  <div key={`${l.userId}-${i}`} className="flex items-center gap-2 text-xs">
                    {lineIcon(l.status)}
                    <span className="font-medium">{l.name}</span>
                    <span className="text-slate-500">
                      {l.status === 'success'
                        ? `签到成功${l.reward ? ` · +${l.reward.toLocaleString()} 额度` : ''}`
                        : l.status === 'already'
                          ? '今天已签过（幂等跳过，计成功）'
                          : `失败：${l.message ?? '未知错误'}`}
                    </span>
                  </div>
                ))}
                {running && lines.length < candidates.length && (
                  <div className="flex items-center gap-2 text-xs text-slate-400">
                    正在签到第 {lines.length + 1}/{candidates.length} 个账号…
                  </div>
                )}
              </div>
            </div>
          )}

          {/* 活动档期日历（对齐 Trae 一键签到页）：近 90 天签到档期按月可视化 */}
          <div className="mt-1 flex items-center gap-2 px-1 pt-3 text-xs text-slate-400">
            <CalendarDays size={14} />
            档期：每日定时任务到点自动签到（时刻见环境配置）· 签到成功（含已签）计绿 · 失败计红
          </div>
          <CheckinCalendarCard
            title="活动档期日历"
            days={calDays}
            footnote="按日落库保证同账号同日仅一条（重试轮覆盖为最终态）· 点按日格查看逐账号明细"
          />
        </>
      )}
    </div>
  );
}
