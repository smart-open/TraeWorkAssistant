import { useCallback, useEffect, useMemo, useState } from 'react';
import { CheckCircle2, Gift, PlayCircle, RefreshCw, XCircle } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { Badge } from '../../components/ui';
import CheckinCalendarCard from '../../components/CheckinCalendar';
import { daysFromWbRecords } from '../../components/checkinCalendarModel';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import type { CodeartsAccountView, CodeartsCheckinLine, CodeartsCheckinRecord } from '../../types';

/**
 * codearts-checkin 每日签到：签到控制卡（NDJSON 进度）+ 档期日历（复用通用日历组件）。
 * 签到进行态由 store 单例持有（codearts-checkin-progress 监听也在 store 层 setupListeners
 * 注册），页面切走/卸载不丢事件，切回时进度/汇总完好。
 */

export default function CodeArtsCheckin() {
  const pushToast = useAppStore((s) => s.pushToast);
  const startCodeartsCheckin = useAppStore((s) => s.startCodeartsCheckin);
  const { running, lines, done: doneInfo, doneRev } = useAppStore((s) => s.codeartsCheckin);
  const [accounts, setAccounts] = useState<CodeartsAccountView[]>([]);
  const [checkinMap, setCheckinMap] = useState<Map<string, CodeartsCheckinRecord[]>>(new Map());
  const [history, setHistory] = useState<CodeartsCheckinRecord[]>([]);
  const [refreshing, setRefreshing] = useState(false);

  const refresh = useCallback(async () => {
    setRefreshing(true);
    try {
      const [accs, recs] = await Promise.all([
        api.codearts.accountsList(),
        api.codearts.checkinResults(90),
      ]);
      setAccounts(accs);
      setHistory(recs);
      const today = new Date();
      const todayStr = `${today.getFullYear()}-${String(today.getMonth() + 1).padStart(2, '0')}-${String(today.getDate()).padStart(2, '0')}`;
      const m = new Map<string, CodeartsCheckinRecord[]>();
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

  // doneRev 变化（签到轮次完成）后联动刷新账号列表/签到记录
  useEffect(() => {
    if (doneRev > 0) void refresh();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [doneRev]);

  const startCheckin = async () => {
    if (accounts.length === 0) {
      pushToast('warn', '账号池为空：请先在「账号管理」录入 AK/SK');
      return;
    }
    await startCodeartsCheckin({ skip_checked_in: true });
  };

  const checkinStatusOf = (uid: string): CodeartsCheckinLine['status'] | null => {
    const live = lines.find((l) => l.user_id === uid);
    if (live) return live.status;
    const recs = checkinMap.get(uid) ?? [];
    if (recs.some((r) => r.status === 'success' || r.status === 'already')) {
      return recs.some((r) => r.status === 'success') ? 'success' : 'already';
    }
    const failed = recs.find((r) => r.status === 'fail');
    return failed ? 'fail' : null;
  };

  const earned = Math.round(lines.reduce((s, l) => s + (l.reward ?? 0), 0) * 100) / 100;
  const calDays = useMemo(() => daysFromWbRecords(history), [history]);

  return (
    <div className="animate-fade-in">
      <PageHeader title="CodeArts · 每日签到" desc="账号池每日签到 · 已领账号自动跳过（幂等）" />

      {/* 签到说明卡 */}
      <div className="card p-4">
        <div className="mb-2 flex items-center gap-2">
          <Gift size={16} className="text-violet-500" />
          <span className="text-sm font-medium">签到说明</span>
        </div>
        <p className="text-xs leading-relaxed text-slate-400">
          每日签到收益为账号级积分，重复执行不会重复领取。支持应用内定时与 Windows 计划任务双轨，
          签到时刻可在「环境配置」修改。已领过的账号自动跳过。
        </p>
      </div>

      {/* 一键签到卡 */}
      <div className="mt-4 card p-4">
        <div className="mb-3 flex items-center justify-between">
          <div className="flex items-center gap-2">
            <span className="text-sm font-medium">一键签到</span>
            {running && <Badge tone="blue">执行中</Badge>}
          </div>
          {accounts.length > 0 && !running && (
            <span className="text-xs text-slate-400">已领账号自动跳过（幂等）</span>
          )}
        </div>
        <div className="rounded-lg border border-slate-200 dark:border-zinc-700">
          <table className="w-full text-sm">
            <thead className="bg-slate-50 text-xs text-slate-500 dark:bg-zinc-900">
              <tr>
                <th className="px-3 py-1.5 text-left">账号</th>
                <th className="px-3 py-1.5 text-left">凭证</th>
                <th className="px-3 py-1.5 text-left">签到状态</th>
                <th className="px-3 py-1.5 text-right">今日获得</th>
                <th className="px-3 py-1.5 text-right">可用积分</th>
              </tr>
            </thead>
            <tbody>
              {accounts.length === 0 ? (
                <tr>
                  <td colSpan={5} className="px-3 py-4 text-center text-xs text-slate-400">
                    暂无账号：请先在「账号管理」录入 AK/SK
                  </td>
                </tr>
              ) : (
                accounts.map((a) => {
                  const st = checkinStatusOf(a.id);
                  const earn = lines.find((l) => l.user_id === a.id)?.reward;
                  return (
                    <tr key={a.id} className="border-t border-slate-100 dark:border-zinc-800">
                      <td className="px-3 py-1.5">
                        <div className="font-medium">{a.name || a.id}</div>
                        <div className="text-xs text-slate-400">{a.id}</div>
                      </td>
                      <td className="px-3 py-1.5">
                        <Badge tone={a.has_credential ? 'green' : 'red'}>
                          {a.has_credential ? '已录入' : '无凭证'}
                        </Badge>
                      </td>
                      <td className="px-3 py-1.5">
                        {st == null ? (
                          <Badge tone="slate">未签</Badge>
                        ) : st === 'success' ? (
                          <Badge tone="green">已领</Badge>
                        ) : st === 'already' ? (
                          <Badge tone="blue">已领（此前已领）</Badge>
                        ) : st === 'skip' ? (
                          <Badge tone="amber">跳过</Badge>
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

      {/* 实时进度卡 */}
      {(running || lines.length > 0) && (
        <div className="mt-4 card p-4">
          <div className="mb-3 flex items-center justify-between">
            <h3 className="font-medium">实时进度</h3>
            {running ? (
              <Badge tone="blue">运行中</Badge>
            ) : doneInfo ? (
              <Badge tone={doneInfo.failed > 0 ? 'amber' : 'green'}>
                完成：成功 {doneInfo.ok} · 已签 {doneInfo.already} · 失败 {doneInfo.failed}
                {earned > 0 && ` · 获得 ${earned} 积分`}
              </Badge>
            ) : null}
          </div>
          <div className="space-y-1">
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
                  <div
                    key={l.index}
                    className="flex items-center gap-2 rounded border border-slate-200 px-3 py-2 text-sm dark:border-zinc-700"
                  >
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

      {/* 档期日历 */}
      <div className="mt-4">
        <CheckinCalendarCard
          title="签到档期日历"
          days={calDays}
          footnote="档期：每日签到 · 应用内调度到点自动执行"
        />
      </div>
    </div>
  );
}
