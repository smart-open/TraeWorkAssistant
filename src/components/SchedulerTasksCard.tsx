import { useCallback, useEffect, useState } from 'react';
import { CalendarClock, RotateCcw, Loader2 } from 'lucide-react';
import { Badge, Spinner } from './ui';
import { api } from '../lib/tauri';
import { useAppStore } from '../store';
import type { SchedulerTaskView } from '../types';

/**
 * 定时任务配置卡（Trae / Buddy 环境配置共用）：
 * 按平台展示内置调度任务开关（kv scheduler_cfg.disabled_tasks，缺省全开 = 推荐配置）、
 * 可编辑的每日触发时刻（kv scheduler_cfg.task_times，空 = 默认时刻）与最近一次执行摘要；
 * wb-checkin 由外部 override 绑定 WorkBuddySettings.auto_checkin。
 */

/** 各任务一句话说明（key → 描述） */
const TASK_DESC: Record<string, string> = {
  'trae-jwt-renew': '临期 JWT 自动续期，剩余有效期 > 48h 时自动跳过',
  'models-sync': '同步官网模型列表（不消耗积分），供网关与模型选择器使用',
  'trae-checkin': '全账号每日自动签到，已签账号自动跳过（幂等）',
  'wb-checkin': '每日自动签到；服务启动后也会自动补签当日签到',
  'wb-growth': '每日成长任务（旅行/盲盒/任务三开关驱动，全关时空轮）',
  'wb-renew': 'Token 到期前 24h 内自动续期（兜底，每日检查一次）',
  'wb-credits-snapshot': '积分余额每日快照，补齐积分看板近 7 日消耗趋势',
  'trae-credits-snapshot': '积分余额每日快照，补齐积分看板消耗趋势',
};

/** 外部接管开关（wb-checkin → settings.auto_checkin）：checked + 切换回调 */
export interface TaskToggleOverride {
  checked: boolean;
  onToggle: (v: boolean) => void;
  /** 切换请求进行中（禁用开关防连点） */
  busy?: boolean;
}

export default function SchedulerTasksCard({
  taskKeys,
  title = '定时任务',
  desc = '由服务端内置调度器每日自动执行，失败 30 分钟后自动重试。',
  overrides,
  className,
}: {
  taskKeys: string[];
  title?: string;
  desc?: string;
  /** 需要（部分开关）绑定到其他配置源时提供，如 wb-checkin → auto_checkin */
  overrides?: Record<string, TaskToggleOverride>;
  className?: string;
}) {
  const pushToast = useAppStore((s) => s.pushToast);
  const [tasks, setTasks] = useState<SchedulerTaskView[] | null>(null);
  const [disabled, setDisabled] = useState<string[]>([]);
  /** 自定义触发时刻表（key → HH:MM；空 = 默认时刻，状态接口回显生效值） */
  const [times, setTimes] = useState<Record<string, string>>({});
  const [toggling, setToggling] = useState<string | null>(null);
  const [timeSaving, setTimeSaving] = useState<string | null>(null);
  const [resetting, setResetting] = useState(false);

  const reload = useCallback(() => {
    api.scheduler
      .status()
      .then((r) => setTasks(r.tasks.filter((t) => taskKeys.includes(t.key))))
      .catch(() => setTasks(null));
    api.scheduler
      .configGet()
      .then((c) => {
        setDisabled(c.disabled_tasks);
        setTimes(c.task_times ?? {});
      })
      .catch(() => setDisabled([]));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    reload();
  }, [reload]);

  /** 开关切换：乐观更新 → 服务端保存，失败回滚并提示 */
  const toggleTask = async (key: string, next: boolean) => {
    if (overrides?.[key]) {
      overrides[key].onToggle(next);
      return;
    }
    const prev = disabled;
    const nextList = next ? prev.filter((k) => k !== key) : [...prev, key];
    setDisabled(nextList);
    setToggling(key);
    try {
      // config_set 为整表替换：必须同时携带自定义时刻，否则会被清空
      const saved = await api.scheduler.configSet({ disabled_tasks: nextList, task_times: times });
      setDisabled(saved.disabled_tasks);
      setTimes(saved.task_times ?? {});
      pushToast('success', next ? '任务已启用，明日按计划执行' : '任务已停用');
      // enabled 合成了其他设置语义，重取状态保持展示准确
      api.scheduler
        .status()
        .then((r) => setTasks(r.tasks.filter((t) => taskKeys.includes(t.key))))
        .catch(() => {});
    } catch (e) {
      setDisabled(prev);
      pushToast('error', e instanceof Error ? e.message : '保存失败');
    } finally {
      setToggling(null);
    }
  };

  /** 修改执行时刻：清空 = 恢复默认；保存后重取状态回显生效时刻 */
  const saveTime = async (key: string, value: string) => {
    const prev = times;
    const next = { ...prev };
    if (value) next[key] = value;
    else delete next[key];
    setTimes(next);
    setTimeSaving(key);
    try {
      const saved = await api.scheduler.configSet({ disabled_tasks: disabled, task_times: next });
      setTimes(saved.task_times ?? {});
      pushToast('success', value ? `执行时间已改为 ${value}（当天已过新时刻会自动补跑）` : '已恢复默认执行时间');
      api.scheduler
        .status()
        .then((r) => setTasks(r.tasks.filter((t) => taskKeys.includes(t.key))))
        .catch(() => {});
    } catch (e) {
      setTimes(prev);
      pushToast('error', e instanceof Error ? e.message : '保存失败');
    } finally {
      setTimeSaving(null);
    }
  };

  /** 恢复推荐配置：清空停用名单与自定义时刻（全部启用 + 默认时刻） */
  const resetRecommended = async () => {
    setResetting(true);
    try {
      const saved = await api.scheduler.configSet({ disabled_tasks: [], task_times: {} });
      setDisabled(saved.disabled_tasks);
      setTimes(saved.task_times ?? {});
      pushToast('success', '已恢复推荐配置（全部任务启用 + 默认时刻）');
      api.scheduler
        .status()
        .then((r) => setTasks(r.tasks.filter((t) => taskKeys.includes(t.key))))
        .catch(() => {});
    } catch (e) {
      pushToast('error', e instanceof Error ? e.message : '恢复失败');
    } finally {
      setResetting(false);
    }
  };

  // 恢复推荐按钮只覆盖本卡内的 kv 配置任务；外部 override 绑定的开关由自身行内开关恢复
  const hasCustom = disabled.some((k) => taskKeys.includes(k)) || taskKeys.some((k) => times[k]);

  return (
    <section className={`card p-4 ${className ?? ''}`}>
      <div className="mb-1 flex flex-wrap items-center justify-between gap-2">
        <div className="flex items-center gap-2">
          <CalendarClock size={16} className="text-brand-500" />
          <h3 className="font-medium">{title}</h3>
        </div>
        {hasCustom && (
          <button onClick={resetRecommended} disabled={resetting} className="btn-outline text-xs">
            {resetting ? <Loader2 size={13} className="animate-spin" /> : <RotateCcw size={13} />} 恢复推荐配置
          </button>
        )}
      </div>
      <p className="mb-3 text-xs text-slate-400">{desc}</p>

      {!tasks ? (
        <div className="flex items-center gap-2 text-sm text-slate-400">
          <Spinner /> 加载中…
        </div>
      ) : (
        <div className="space-y-2">
          {taskKeys.map((key) => {
            const t = tasks.find((x) => x.key === key);
            const ov = overrides?.[key];
            const on = ov ? ov.checked : !disabled.includes(key);
            const busy = ov ? !!ov.busy : toggling === key;
            return (
              <div
                key={key}
                className={`flex items-start gap-3 rounded-lg bg-slate-50 px-3 py-2.5 transition-opacity dark:bg-zinc-900 ${
                  on ? '' : 'opacity-55'
                }`}
              >
                <input
                  type="checkbox"
                  className="mt-1"
                  checked={on}
                  disabled={busy}
                  onChange={(e) => void toggleTask(key, e.target.checked)}
                />
                <div className="min-w-0 flex-1">
                  <div className="flex flex-wrap items-center gap-2">
                    <span className="text-sm font-medium">{t?.name ?? key}</span>
                    {t && (
                      <input
                        type="time"
                        value={t.time}
                        disabled={timeSaving === key}
                        onChange={(e) => void saveTime(key, e.target.value)}
                        className="w-[92px] rounded-md border border-slate-200 bg-white px-1.5 py-0.5 text-xs tabular-nums text-slate-600 disabled:opacity-50 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-300"
                        title="每日触发时刻（修改即时保存；留空恢复默认）"
                      />
                    )}
                    {times[key] && t?.time !== undefined && (
                      <button
                        onClick={() => void saveTime(key, '')}
                        disabled={timeSaving === key}
                        className="text-xs text-slate-400 underline-offset-2 hover:text-brand-500 hover:underline"
                        title="恢复该任务的默认时刻"
                      >
                        默认
                      </button>
                    )}
                    {!on && <Badge tone="amber">已停用</Badge>}
                  </div>
                  <p className="mt-0.5 text-xs text-slate-400">{TASK_DESC[key] ?? ''}</p>
                  {t?.last_summary && (
                    <p
                      className={`mt-1 truncate text-xs ${
                        t.last_ok === false ? 'text-rose-500' : 'text-emerald-600 dark:text-emerald-400'
                      }`}
                      title={`${t.last_run_date ?? ''} ${t.last_summary}`}
                    >
                      {t.last_ok === false ? '上次失败：' : '上次执行：'}
                      {t.last_summary}
                      {t.last_run_date ? `（${t.last_run_date}）` : ''}
                    </p>
                  )}
                </div>
              </div>
            );
          })}
        </div>
      )}
    </section>
  );
}
