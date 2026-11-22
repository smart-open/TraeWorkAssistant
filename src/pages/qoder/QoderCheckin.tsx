import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { CheckCircle2, Gift, PlayCircle, RefreshCw, XCircle } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { Badge, Modal } from '../../components/ui';
import CheckinCalendarCard from '../../components/CheckinCalendar';
import { daysFromQoderRecords } from '../../components/checkinCalendarModel';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import type { QoderAccountView, QoderCheckinRecord } from '../../types';

/**
 * qoder-checkin 每日签到（F-80 §5.8，对照 BuddyCheckin 裁剪复刻）：
 * 签到控制卡（NDJSON 进度）+ 双活动说明卡。
 * 双活动：0 点刷新的 QoderWork 签到（100 Credits/天）+ 10:00 开窗的每日登录奖励
 * （100 Add-on Credits/天）——接口层同源 sash campaigns，一次触发自然全覆盖。
 * 疑点⑦：签到进行态由 store 单例持有（qoder-checkin-progress 监听也在 store 层
 * setupListeners 注册），页面切走/卸载不丢事件，切回时进度/汇总完好。
 */

function MiniTokenBadge({ a }: { a: QoderAccountView }) {
  if (a.needs_relogin)
    return <span className="text-xs text-rose-500"><XCircle size={11} className="inline" /> 需重新登录</span>;
  const exp = a.token_expires_at;
  if (!exp) return <span className="text-xs text-emerald-500"><CheckCircle2 size={11} className="inline" /> 长期有效</span>;
  const hours = (exp - Math.floor(Date.now() / 1000)) / 3600;
  if (hours <= 0) return <span className="text-xs text-rose-500"><XCircle size={11} className="inline" /> 已过期</span>;
  if (hours <= 24) return <span className="text-xs text-amber-500">{hours.toFixed(1)}h</span>;
  return <span className="text-xs text-emerald-500">{hours.toFixed(0)}h</span>;
}

/** 凭证来源徽标（端类型标签：§3.3 页面内区分凭证来源） */
function SourceBadge({ a }: { a: QoderAccountView }) {
  const label = a.credential_source === 'pat'
    ? 'PAT'
    : a.credential_source === 'ide_store'
    ? 'IDE'
    : a.credential_source === 'qoderwork_store'
    ? 'Work'
    : a.credential_source === 'mitm'
    ? 'MITM'
    : a.credential_source === 'cli'
    ? 'CLI'
    : a.credential_source || '—';
  return <Badge tone={a.credential_source === 'pat' ? 'green' : 'slate'}>{label}</Badge>;
}

export default function QoderCheckin() {
  const pushToast = useAppStore((s) => s.pushToast);
  const startQoderCheckin = useAppStore((s) => s.startQoderCheckin);
  // 疑点⑦：签到进行态（running/lines/done）来自 store，页面卸载不丢、切回即恢复
  const { running, lines, done: doneInfo, doneRev } = useAppStore((s) => s.qoderCheckin);
  const [accounts, setAccounts] = useState<QoderAccountView[]>([]);
  const [checkinMap, setCheckinMap] = useState<Map<string, QoderCheckinRecord[]>>(new Map());
  const [refreshing, setRefreshing] = useState(false);
  // F-80-余 v2 档期日历：90 天签到结果（通用组件渲染；聚合在 checkinCalendarModel）
  const [history, setHistory] = useState<QoderCheckinRecord[]>([]);
  // 无签到任务弹框（2026-11-21）：人工点击「开始签到」且本轮存在 empty_campaigns
  // 跳过时弹出官网提示；静默补签/调度轮次不打扰（manualStartRef 区分入口）
  const [emptyNoticeOpen, setEmptyNoticeOpen] = useState(false);
  const manualStartRef = useRef(false);

  const refresh = useCallback(async () => {
    setRefreshing(true);
    try {
      // 不做内层吞错：任一源失败走外层 catch 统一 toast，避免「静默空态无反馈」
      const [accs, recs] = await Promise.all([
        api.qoder.accountsList(),
        api.qoder.checkinResults(90),
      ]);
      setAccounts(accs);
      setHistory(recs);
      const today = new Date();
      const todayStr = `${today.getFullYear()}-${String(today.getMonth() + 1).padStart(2, '0')}-${String(today.getDate()).padStart(2, '0')}`;
      const m = new Map<string, QoderCheckinRecord[]>();
      for (const r of recs) {
        if (r.date !== todayStr) continue;
        const arr = m.get(r.user_id) ?? [];
        arr.push(r);
        m.set(r.user_id, arr);
      }
      setCheckinMap(m);
    } catch (err) {
      pushToast('error', `读取签到数据失败：${String(err)}`);
    } finally {
      setRefreshing(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    void refresh();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // 疑点⑦：进度事件由 store 归约（applyQoderCheckinLine），页面只按 doneRev 联动刷新
  // 账号列表/签到记录（skipped_busy 整轮跳过不递增 doneRev，不触发刷新——对齐原页面语义）
  useEffect(() => {
    if (doneRev > 0) {
      void refresh();
      // 人工「开始签到」轮次含无签到任务跳过 → 弹框提示官网（done 与 doneRev 同批
      // 写入 store，本 effect 拿到的 doneInfo 即本轮终值；调度/静默轮次不打扰）。
      // 两类跳过都算「无签到任务」：empty=空活动表；skipped_ineligible=体验版不在
      // 投放范围（服务端同样不下发活动，issue #223）——只看 empty 会漏弹后者
      if (manualStartRef.current) {
        manualStartRef.current = false;
        if (doneInfo && (doneInfo.empty > 0 || doneInfo.skipped_ineligible > 0)) {
          setEmptyNoticeOpen(true);
        }
      }
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [doneRev]);

  // manualStartRef 悬挂防护（2026-11-21 审查）：轮次结束但 doneRev 未递增的两条路径
  //（invoke 发起失败 / skipped_busy 整轮跳过——均无 done 计数事件）不会走上一 effect，
  // 标记会悬挂到下一轮调度/静默 done 误弹「无签到任务」弹框。running 复位 false 时
  // 兜底清标记；正常 done 路径上一 effect（声明序在前）已先复位，本 effect 为 no-op
  useEffect(() => {
    if (!running) manualStartRef.current = false;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [running]);

  const startCheckin = async () => {
    if (accounts.length === 0) {
      pushToast('warn', '账号池为空：请先在「账号管理」导入 PAT 或客户端账号');
      return;
    }
    manualStartRef.current = true;
    // 连点自守/进度清理/失败置回均在 store 动作内（疑点⑦）
    await startQoderCheckin();
  };

  const todayEarnedOf = (uid: string): number | null => {
    const recs = checkinMap.get(uid) ?? [];
    // 真实入账合计：success/fail 的 reward 全计（双活动同额属真实两笔）；
    // already 的 reward（幂等回放 / 断网复查确认）同为当日真实入账，一并计入展示，
    // 但与 success/fail 同额时视为同活动重复回放，去重防双计（与顺序无关，两遍扫描）
    const real = recs.filter((r) => r.status !== 'already' && r.reward != null);
    const realAmts = new Set(real.map((r) => r.reward));
    let sum = real.reduce((s, r) => s + (r.reward ?? 0), 0);
    const seenAlready = new Set<number>();
    for (const r of recs) {
      if (r.status !== 'already' || r.reward == null) continue;
      if (realAmts.has(r.reward) || seenAlready.has(r.reward)) continue;
      seenAlready.add(r.reward);
      sum += r.reward;
    }
    return sum > 0 ? Math.round(sum * 100) / 100 : null;
  };
  const checkinStatusOf = (uid: string): 'success' | 'already' | 'fail' | 'skip' | null => {
    const live = lines.find((l) => l.user_id === uid);
    if (live) return live.status;
    const recs = checkinMap.get(uid) ?? [];
    if (recs.some((r) => r.status === 'success' || r.status === 'already')) {
      return recs.some((r) => r.status === 'success') ? 'success' : 'already';
    }
    if (recs.some((r) => r.status === 'fail')) return 'fail';
    // skip（体验版未满足活动资格，issue #223 / 无签到任务，2026-11-21）：确定性
    // 跳过非失败，中性展示（徽标文案按 fail_kind 细分，见 skipKindOf）
    return recs.some((r) => r.status === 'skip') ? 'skip' : null;
  };

  // skip 细分：empty=无签到任务（fail_kind=empty_campaigns）/ ineligible=体验版
  // 未满足资格。落库后读记录 fail_kind 字段（历史记录无该字段回退 ineligible 文案）。
  // P3-16 审查修复：进行中实时行不再硬编码后端标记字符串做前缀匹配——后端
  // （qoder_checkin.rs L2088-2142）对 skip 事件总是携带结构化 fail_kind
  //（empty_campaigns / experience_ineligible），直接消费该字段；字段缺失才回退
  // ineligible（与历史记录回退口径一致），后端标记/文案变更不再静默失配
  const skipKindOf = (uid: string): 'empty' | 'ineligible' => {
    const live = lines.find((l) => l.user_id === uid);
    if (live && live.status === 'skip') {
      // P3-16：消费后端结构化 fail_kind（store 已透传），字段缺失回退 ineligible
      //（与历史记录回退口径一致）
      return live.fail_kind === 'empty_campaigns' ? 'empty' : 'ineligible';
    }
    const recs = checkinMap.get(uid) ?? [];
    return recs.some((r) => r.status === 'skip' && r.fail_kind === 'empty_campaigns')
      ? 'empty'
      : 'ineligible';
  };

  const earned = Math.round(lines.reduce((s, l) => s + (l.reward ?? 0), 0) * 100) / 100;

  // ── 档期日历聚合（F-80-余 v2，通用组件）：逐活动状态点 / 奖励去重聚合 /
  // 逐账号明细均在 checkinCalendarModel.daysFromQoderRecords（含单测）
  const calDays = useMemo(() => daysFromQoderRecords(history), [history]);

  return (
    <div className="animate-fade-in">
      <PageHeader title="Qoder · 每日签到" desc="每天自动领取「签到」与「登录」两项奖励，重复执行不重复领" />

      {/* 双活动说明卡（§2.2） */}
      <div className="card p-4">
        <div className="mb-3 flex items-center gap-2">
          <Gift size={16} className="text-violet-500" />
          <span className="text-sm font-medium">每日双活动（一次执行，两项奖励同时领）</span>
        </div>
        <div className="grid gap-3 lg:grid-cols-2">
          <div className="rounded-lg border border-slate-100 p-3 text-sm dark:border-zinc-800">
            <div className="font-medium">活动一：每日签到</div>
            <div className="mt-1 text-xs text-slate-400">
              每天 0 点刷新，签到领 100 Credits（独立 30 天有效期包，优先消耗将过期的）；当天漏签不补发
            </div>
          </div>
          <div className="rounded-lg border border-slate-100 p-3 text-sm dark:border-zinc-800">
            <div className="font-medium">活动二：每日登录奖励</div>
            <div className="mt-1 text-xs text-slate-400">
              每天 10:00 起可领 100 通用 Add-on Credits（仅个人版），有效期至次日 10:00；结束时间以官方公告为准
            </div>
          </div>
        </div>
        <p className="mt-3 text-xs text-slate-400">
          调度默认每天 10:15 自动执行一次（此时两项活动都已开放）；应用内 Rust 调度器驱动，
          时刻可在环境配置修改。已领过的账号自动跳过，重复执行不会重复领取。
        </p>
      </div>

      {/* 一键签到卡 */}
      <div className="mt-4 card p-4">
        <div className="mb-3 flex items-center justify-between">
          <div className="flex items-center gap-2">
            <span className="text-sm font-medium">一键签到</span>
            {running && <Badge tone="blue">执行中</Badge>}
          </div>
          {accounts.length > 0 && !running && <span className="text-xs text-slate-400">已领账号自动跳过（幂等）</span>}
        </div>
        <div className="rounded-lg border border-slate-200 dark:border-zinc-700">
          <table className="w-full text-sm">
            <thead className="bg-slate-50 text-xs text-slate-500 dark:bg-zinc-900">
              <tr>
                <th className="px-3 py-1.5 text-left">账号</th>
                <th className="px-3 py-1.5 text-left">来源</th>
                <th className="px-3 py-1.5 text-left">登录态</th>
                <th className="px-3 py-1.5 text-left">签到状态</th>
                <th className="px-3 py-1.5 text-right">今日获得</th>
                <th className="px-3 py-1.5 text-right">可用总积分</th>
              </tr>
            </thead>
            <tbody>
              {accounts.length === 0 ? (
                <tr>
                  <td colSpan={6} className="px-3 py-4 text-center text-xs text-slate-400">
                    暂无账号：请先在「账号管理」导入 PAT
                  </td>
                </tr>
              ) : (
                accounts.map((a) => {
                  const st = checkinStatusOf(a.id);
                  const earn = lines.find((l) => l.user_id === a.id)?.reward ?? todayEarnedOf(a.id);
                  return (
                    <tr key={a.id} className="border-t border-slate-100 dark:border-zinc-800">
                      <td className="px-3 py-1.5">
                        <div className="font-medium">{a.nickname || a.uid || a.id}</div>
                        <div className="text-xs text-slate-400">{a.phone_masked || a.id}</div>
                      </td>
                      <td className="px-3 py-1.5"><SourceBadge a={a} /></td>
                      <td className="px-3 py-1.5"><MiniTokenBadge a={a} /></td>
                      <td className="px-3 py-1.5">
                        {st == null ? (
                          <Badge tone="slate">未签</Badge>
                        ) : st === 'success' ? (
                          <Badge tone="green">已领</Badge>
                        ) : st === 'already' ? (
                          <Badge tone="blue">已领（此前已领）</Badge>
                        ) : st === 'skip' ? (
                          <Badge tone="slate">{skipKindOf(a.id) === 'empty' ? '无签到任务' : '未满足资格'}</Badge>
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
        <div className="mt-3 flex items-center justify-between">
          <span className="text-xs text-slate-400">
            {running ? '签到进行中，逐账号结果见下方实时进度…' : '签到结果将展示在下方实时进度卡'}
          </span>
          <div className="flex items-center gap-2">
            <button className="btn-outline" onClick={() => void refresh()} disabled={refreshing}>
              <RefreshCw size={15} className={refreshing ? 'animate-spin' : ''} /> 刷新
            </button>
            <button className="btn-outline" onClick={() => void startCheckin()} disabled={running}>
              <PlayCircle size={15} /> {running ? '签到中…' : '开始签到'}
            </button>
          </div>
        </div>
      </div>

      {/* 实时进度卡（紧随一键签到：执行时逐账号结果即时可见，无需滚过日历） */}
      {(running || lines.length > 0) && (
        <div className="mt-4 card p-4">
          <div className="mb-3 flex items-center justify-between">
            <h3 className="font-medium">实时进度</h3>
            {running ? (
              <Badge tone="blue">运行中</Badge>
            ) : doneInfo ? (
              <Badge
                tone={doneInfo.failed > 0 || doneInfo.blocked > 0 ? 'amber' : 'green'}
                title={
                  doneInfo.blocked > 0
                    ? '「风控拦截」恢复需用户操作：在工具内切换到该账号后打开 Qoder 客户端使用一次建立信任，再手动重试（issue #190）'
                    : undefined
                }
              >
                完成：成功 {doneInfo.ok} · 已签 {doneInfo.already} · 失败 {doneInfo.failed}
                {doneInfo.empty > 0 && `（另有 ${doneInfo.empty} 项无签到任务，已跳过）`}
                {doneInfo.blocked > 0 && `（含 ${doneInfo.blocked} 项风控拦截）`}
                {earned > 0 && ` · 获得 ${earned} Credits`}
              </Badge>
            ) : null}
          </div>
          <div className="space-y-1">
            {/* 过滤稀疏数组空洞：乱序事件按 index 跳写产生 hole，直接 map 会在 hole 上取 status 崩溃 */}
            {lines
              .filter((l) => l && l.user_id)
              .map((l) => {
                const tone =
                  l.status === 'success'
                    ? 'text-emerald-600 dark:text-emerald-300'
                    : l.status === 'already'
                    ? 'text-sky-600 dark:text-sky-300'
                    : l.status === 'skip'
                    ? 'text-amber-600 dark:text-amber-300'
                    : 'text-rose-600 dark:text-rose-300';
                const Icon = l.status === 'fail' ? XCircle : CheckCircle2;
                return (
                  <div key={l.index} className="flex items-center gap-2 rounded border border-slate-200 px-3 py-2 text-sm dark:border-zinc-700">
                  <Icon size={14} className={tone} />
                  <span className="w-8 text-right text-xs text-slate-400">{l.index}</span>
                  <span className="flex-1 truncate">{l.name || l.user_id}</span>
                  <span className={`max-w-[50%] truncate text-xs ${tone}`} title={l.message}>
                    {l.message || l.status}
                  </span>
                  {l.reward != null && (
                    <span className="shrink-0 rounded bg-emerald-50 px-1.5 py-0.5 text-xs font-medium text-emerald-600 dark:bg-emerald-500/10 dark:text-emerald-300">
                      +{l.reward}
                    </span>
                  )}
                </div>
              );
            })}
          </div>
        </div>
      )}

      {/* 活动档期日历（F-80-余 v2，通用组件）：双活动领取结果按日可视化，辅助校验排期决策 */}
      <CheckinCalendarCard
        title="活动档期日历"
        days={calDays}
        footnote="档期：0:00 每日签到刷新 · 10:00 登录奖励开窗 · 10:15 应用内调度"
      />

      {/* 无签到任务弹框（2026-11-21）：仅人工「开始签到」轮次弹出（manualStartRef
          区分入口），静默补签/调度/CLI 轮次直接跳过不打扰 */}
      <Modal
        open={emptyNoticeOpen}
        onClose={() => setEmptyNoticeOpen(false)}
        title="未发现签到任务"
        footer={
          <button className="btn-outline" onClick={() => setEmptyNoticeOpen(false)}>
            知道了
          </button>
        }
      >
        <p>签到任务已经接入，当前未发现可执行的签到任务（活动未开放或账号不在投放范围），本轮已跳过。</p>
        <p className="mt-2">
          详情请查看官网：
          <a
            className="break-all text-sky-600 underline dark:text-sky-400"
            href="https://docs.qoder.cn/events/100credits"
            target="_blank"
            rel="noreferrer"
          >
            https://docs.qoder.cn/events/100credits
          </a>
        </p>
      </Modal>
    </div>
  );
}
