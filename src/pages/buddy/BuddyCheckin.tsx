import { useCallback, useEffect, useRef, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import { PlayCircle, Sparkles } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { Badge } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import type { WorkBuddyAccountView, WorkBuddySettings } from '../../types';

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
  energy?: number | string;
  streak?: number | string;
}

type ParsedEvent =
  | { type: 'start'; total: number; mode?: string }
  | { type: 'done'; ok: number; already: number; failed: number; mode?: string }
  | { type: 'exit' }
  | WbAccountLine
  | WbGrowthLine;

function parseLine(raw: string): ParsedEvent | null {
  try {
    return JSON.parse(raw);
  } catch {
    return null;
  }
}

const statusTone: Record<string, 'green' | 'blue' | 'red'> = {
  success: 'green',
  already: 'blue',
  fail: 'red',
};
const statusText: Record<string, string> = {
  success: '成功',
  already: '已签',
  fail: '失败',
};

export default function BuddyCheckin() {
  const pushToast = useAppStore((s) => s.pushToast);
  const [accounts, setAccounts] = useState<WorkBuddyAccountView[]>([]);
  const [running, setRunning] = useState(false);
  const [lines, setLines] = useState<WbAccountLine[]>([]);
  const [summary, setSummary] = useState<string | null>(null);
  const [settings, setSettings] = useState<WorkBuddySettings | null>(null);
  const [growthRunning, setGrowthRunning] = useState(false);
  const [growthLines, setGrowthLines] = useState<WbGrowthLine[]>([]);
  const [growthSummary, setGrowthSummary] = useState<string | null>(null);
  const unlistenRef = useRef<(() => void) | null>(null);

  const refresh = useCallback(async () => {
    try {
      const [accs, st] = await Promise.all([
        api.workbuddy.accountsList().catch(() => [] as WorkBuddyAccountView[]),
        api.workbuddy.settingsGet().catch(() => null),
      ]);
      setAccounts(accs);
      setSettings(st);
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
          setSummary(null);
        }
      } else if ('type' in parsed && parsed.type === 'done') {
        if (parsed.mode === 'growth') {
          setGrowthSummary('成长中心执行完成');
          setGrowthRunning(false);
          void refresh();
          pushToast('success', '成长中心执行完成（旅行/盲盒/任务结果见下方明细）');
        } else {
          setSummary(`成功 ${parsed.ok} · 已签 ${parsed.already} · 失败 ${parsed.failed}`);
          setRunning(false);
          void refresh();
          pushToast(parsed.failed > 0 ? 'warn' : 'success', `WorkBuddy 签到完成：成功 ${parsed.ok}，已签 ${parsed.already}，失败 ${parsed.failed}`);
        }
      } else if ('type' in parsed && parsed.type === 'growth') {
        const line = parsed as WbGrowthLine;
        setGrowthLines((prev) => {
          const next = prev.slice();
          next[line.index - 1] = line;
          return next;
        });
      } else if ('index' in parsed && parsed.index != null) {
        const line = parsed as WbAccountLine;
        setLines((prev) => {
          const next = prev.slice();
          next[line.index - 1] = line;
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
    setSummary(null);
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

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="Buddy · 签到与成长"
        desc="一键签到 · 成长中心 · 定时任务见环境配置"
        actions={
          <button className="btn-primary" onClick={() => void startCheckin()} disabled={running}>
            <PlayCircle size={15} /> {running ? '签到中…' : '立即签到'}
          </button>
        }
      />

      {/* 签到控制卡 */}
      <div className="card p-4">
        <div className="mb-3 flex items-center justify-between">
          <span className="text-sm font-medium">签到进度</span>
          {running && <Badge tone="blue">执行中</Badge>}
          {summary && !running && <span className="text-xs text-slate-400">{summary}</span>}
        </div>
        {lines.length === 0 && !running ? (
          <p className="py-4 text-center text-xs text-slate-400">
            {accounts.length > 0 ? `共 ${accounts.length} 个账号待签：点击「立即签到」开始（已签账号自动跳过）` : '账号池为空：请先在「账号管理」导入账号'}
          </p>
        ) : (
          <div className="space-y-1.5">
            {lines.map((l, i) => (
              <div key={i} className="flex items-center gap-3 rounded-lg border border-slate-100 px-3 py-2 text-sm dark:border-zinc-800">
                <Badge tone={statusTone[l.status] ?? 'slate'}>{statusText[l.status] ?? l.status}</Badge>
                <span className="min-w-0 flex-1 truncate font-medium">{l.name || l.user_id}</span>
                <span className="max-w-[50%] truncate text-xs text-slate-400">{l.message}</span>
              </div>
            ))}
          </div>
        )}
      </div>

      {/* 成长中心卡（F-17：三开关 + 立即执行，T2.5 执行器） */}
      <div className="mt-4 card p-4">
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
    </div>
  );
}
