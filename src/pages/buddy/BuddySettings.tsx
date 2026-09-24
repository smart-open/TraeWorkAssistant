import { useCallback, useEffect, useRef, useState } from 'react';
import { Save, FolderOpen, RefreshCw, TerminalSquare, Play, MousePointerClick, Search, SlidersHorizontal, ListChecks } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { Badge, Spinner } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { withMinDelay } from '../../lib/delay';
import type { WorkBuddySettings, WorkBuddyEnvCheck, WbCliStatus, WbCliRotateLog, Settings, AppLocate } from '../../types';

/**
 * buddy-settings 环境配置（§3.7.5，F-55/F-59/F-13）：
 * 通用配置（应用环境 + 切换迁移会话）+ 任务配置（JWT 续期 / 自动签到 / 自动成长）+ 通知与 CLI 轮换。
 * 到期日历统一收敛在「积分看板」页。
 */

/** CLI 最近活动的人类可读文案 */
function fmtActivity(ms: number | null): string {
  if (ms == null) return '无会话记录';
  const diffMin = Math.floor((Date.now() - ms) / 60000);
  if (diffMin < 1) return '刚刚有会话写入';
  if (diffMin < 60) return `${diffMin} 分钟前有会话写入`;
  const diffH = Math.floor(diffMin / 60);
  if (diffH < 24) return `${diffH} 小时前有会话写入`;
  return `${Math.floor(diffH / 24)} 天前有会话写入`;
}

const ROTATE_PARAM_FIELDS: { key: keyof WorkBuddySettings; label: string; hint: string; min: number; unit?: string }[] = [
  { key: 'cli_rotate_interval_minutes', label: '检查间隔（分钟）', hint: '后台每 N 分钟检查一次是否需要轮换', min: 5 },
  { key: 'cli_cooldown_minutes', label: '冷却期（分钟）', hint: '① 切换后 N 分钟内不再切（防抖动）', min: 1 },
  { key: 'cli_min_gap_hours', label: '到期差异阈值（小时）', hint: '② 目标比当前早到期超过 N 小时才切（防横跳）', min: 0 },
  { key: 'cli_min_urgency_hours', label: '到期紧迫阈值（小时）', hint: '③ 目标剩余超过 N 小时 = 都还早，不切', min: 0 },
  { key: 'cli_active_guard_minutes', label: '活跃保护（分钟）', hint: '④ CLI 最近会话写入 N 分钟内不切（防打断）', min: 0 },
  { key: 'cli_min_remaining_credits', label: '最小剩余积分', hint: '⑤ 目标低于该值不切（0 = 关闭）', min: 0 },
];

/** CLI 五重防护自动轮换卡（F-59）：状态 + 配置 + 手动检查 + 轮换日志 */
function CliRotateCard({
  settings,
  patch,
}: {
  settings: WorkBuddySettings | null;
  patch: (p: Partial<WorkBuddySettings>) => void;
}) {
  const pushToast = useAppStore((s) => s.pushToast);
  const [status, setStatus] = useState<WbCliStatus | null>(null);
  const [logs, setLogs] = useState<WbCliRotateLog[]>([]);
  const [running, setRunning] = useState(false);

  const refreshStatus = useCallback(() => {
    api.workbuddy
      .cliStatus()
      .then(setStatus)
      .catch(() => setStatus(null));
    api.workbuddy
      .cliRotateLogs(8)
      .then(setLogs)
      .catch(() => setLogs([]));
  }, []);

  useEffect(() => {
    void refreshStatus();
  }, [refreshStatus]);

  const runRotate = async () => {
    setRunning(true);
    try {
      const r = await withMinDelay(api.workbuddy.cliRotateRun(), 600);
      if (r.status === 'switched' && r.to) {
        pushToast('success', `已切换 CLI 账号：${r.to.name}（当前会话不受影响，重启 CLI 后生效）`);
      } else if (r.status === 'skipped') {
        pushToast('info', `无需切换：${r.reason ?? ''}`);
      } else {
        pushToast('error', `轮换失败：${r.error ?? '未知错误'}`);
      }
      refreshStatus();
    } catch (err) {
      pushToast('error', `轮换执行失败：${String(err)}`);
    } finally {
      setRunning(false);
    }
  };

  return (
    <div className="card p-4">
      <div className="mb-3 flex items-center justify-between">
        <div className="flex items-center gap-2">
          <TerminalSquare size={16} className="text-sky-500" />
          <span className="text-sm font-medium">CLI 自动轮换（CodeBuddy · 五重防护）</span>
          <Badge tone={settings?.cli_rotate_enabled ? 'green' : 'slate'}>
            {settings?.cli_rotate_enabled ? '已启用' : '未启用'}
          </Badge>
        </div>
        <button className="btn-outline shrink-0" onClick={() => void runRotate()} disabled={running}>
          {running ? <Spinner /> : <Play size={14} />} 立即检查
        </button>
      </div>

      {/* 当前 CLI 账号状态 */}
      <div className="mb-3 rounded-lg border border-slate-100 p-3 text-xs dark:border-zinc-800">
        {status?.environment_override ? (
          <p className="mb-1 font-medium text-rose-500">
            检测到进程环境变量 CODEBUDDY_AUTH_TOKEN：它会覆盖 settings.json 配置，请删除该环境变量后重启 CLI
          </p>
        ) : null}
        <div className="flex flex-wrap items-center gap-2 text-slate-500">
          <Badge tone={status?.env_token_present ? 'green' : 'amber'}>
            {status?.env_token_present ? '已桥接' : '未桥接'}
          </Badge>
          <span>
            当前 CLI 账号：<b className="text-slate-700 dark:text-zinc-200">{status?.active_account_name ?? status?.active_account_id ?? '未知'}</b>
          </span>
          <span className="text-slate-400">· {fmtActivity(status?.recent_activity_ms ?? null)}</span>
        </div>
        <p className="mt-1 text-slate-400">
          桥接目标：~/.codebuddy/settings.json 的 env.CODEBUDDY_AUTH_TOKEN（在账号管理页对账号点「设为 CLI 账号」即可桥接）；
          切换只影响之后新开的 CLI 会话，当前运行中的会话不受影响。
        </p>
      </div>

      {/* 启用开关 + 五重防护参数 */}
      <label className="flex items-start gap-2">
        <input
          type="checkbox"
          className="mt-0.5"
          checked={settings?.cli_rotate_enabled ?? false}
          onChange={(e) => patch({ cli_rotate_enabled: e.target.checked })}
        />
        <span className="text-sm">
          启用自动轮换（防积分过期浪费）
          <span className="block text-xs text-slate-400">
            按「到期最早且仍有剩余」原则自动切换 CLI 账号，五重防护全部通过才执行
          </span>
        </span>
      </label>
      <div className="mt-3 grid gap-3 lg:grid-cols-3">
        {ROTATE_PARAM_FIELDS.map((f) => (
          <label key={f.key} className="block">
            <span className="mb-1 block text-xs font-medium text-slate-500">{f.label}</span>
            <input
              type="number"
              min={f.min}
              step="any"
              className="input w-full"
              value={Number(settings?.[f.key] ?? 0)}
              onChange={(e) => patch({ [f.key]: Number(e.target.value) || 0 } as Partial<WorkBuddySettings>)}
            />
            <span className="mt-1 block text-xs text-slate-400">{f.hint}</span>
          </label>
        ))}
      </div>

      {/* 轮换日志 */}
      {logs.length > 0 && (
        <div className="mt-3 max-h-40 space-y-0.5 overflow-auto rounded-lg border border-slate-100 p-2 font-mono text-[11px] text-slate-500 dark:border-zinc-800 dark:text-zinc-400">
          {logs.map((l, i) => (
            <div key={i}>
              {new Date(l.ts).toLocaleString('zh-CN')} · {l.action}
              {l.to ? ` → ${l.to.name}` : ''}
              {l.reason ? `（${l.reason}）` : ''}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

/** 星期中文标签（每周兜底续期任务用） */
const DAY_LABEL: Record<string, string> = {
  MON: '周一',
  TUE: '周二',
  WED: '周三',
  THU: '周四',
  FRI: '周五',
  SAT: '周六',
  SUN: '周日',
};

/** 任务配置卡：JWT 定时续期（refreshToken 配置 + 每周兜底任务）+ 自动签到（含每日时刻）+ 积分与 Token 同步 + 自动成长 */
function TaskConfigCard({
  settings,
  patch,
  growthForm,
  setGrowthForm,
  creditsForm,
  setCreditsForm,
  checkinHhmm,
  setCheckinHhmm,
}: {
  settings: WorkBuddySettings | null;
  patch: (p: Partial<WorkBuddySettings>) => void;
  growthForm: { enabled: boolean; hhmm: string };
  setGrowthForm: (f: { enabled: boolean; hhmm: string }) => void;
  creditsForm: { mode: string; hhmm: string };
  setCreditsForm: (f: { mode: string; hhmm: string }) => void;
  checkinHhmm: string;
  setCheckinHhmm: (v: string) => void;
}) {
  const pushToast = useAppStore((s) => s.pushToast);
  const [tasks, setTasks] = useState<string[]>([]);
  const [renewOn, setRenewOn] = useState(false);
  const [savingSettings, setSavingSettings] = useState(false);
  // 定时任务（签到/续期）注册与卸载 pending（防连点重复注册）
  const [taskBusy, setTaskBusy] = useState(false);
  // UI 点击兜底执行态（防连点重复驱动鼠标）
  const [uiClickBusy, setUiClickBusy] = useState<'capture' | 'run' | null>(null);
  // 每日签到注册时刻（默认 09:00 可改；第二时段可清空 = 单时段）
  const [t1, setT1] = useState('09:00');
  const [t2, setT2] = useState('21:00');
  // 每周兜底续期任务触发日/时刻（默认周日 10:30，可改）
  const [renewDay, setRenewDay] = useState('SUN');
  const [renewTime, setRenewTime] = useState('10:30');

  const refreshTasks = useCallback(() => {
    api.workbuddy.checkinTaskStatus().then(setTasks).catch(() => setTasks([]));
    api.workbuddy.renewTaskStatus().then(setRenewOn).catch(() => setRenewOn(false));
  }, []);

  useEffect(() => {
    refreshTasks();
  }, [refreshTasks]);

  // settings 单项保存（走 settingsSet 全量 patch，与父页「保存配置」同通道）
  const savePatch = async (p: Partial<WorkBuddySettings>) => {
    if (!settings) {
      pushToast('warn', '设置尚未加载，请稍后重试');
      return;
    }
    setSavingSettings(true);
    try {
      patch(p);
      await api.workbuddy.settingsSet({ ...settings, ...p });
    } catch (err) {
      pushToast('error', `保存设置失败：${String(err)}`);
    } finally {
      setSavingSettings(false);
    }
  };

  const registerTasks = async () => {
    const times = [t1.trim(), t2.trim()].filter(Boolean);
    if (times.length === 0) {
      pushToast('warn', '请至少填写一个签到时刻');
      return;
    }
    setTaskBusy(true);
    try {
      await api.workbuddy.checkinTaskRegister(times);
      setTasks(await api.workbuddy.checkinTaskStatus());
      pushToast('success', `已注册每日签到任务：${times.join(' / ')}`);
    } catch (err) {
      pushToast('error', `注册任务失败：${String(err)}`);
    } finally {
      setTaskBusy(false);
    }
  };

  const unregisterTasks = async () => {
    setTaskBusy(true);
    try {
      await api.workbuddy.checkinTaskUnregister();
      setTasks([]);
      pushToast('info', '已卸载定时签到任务');
    } catch (err) {
      pushToast('error', `卸载任务失败：${String(err)}`);
    } finally {
      setTaskBusy(false);
    }
  };

  const toggleRenew = async () => {
    setTaskBusy(true);
    try {
      if (renewOn) {
        await api.workbuddy.renewTaskUnregister();
        setRenewOn(false);
        pushToast('info', '已卸载每周续期任务');
      } else {
        await api.workbuddy.renewTaskRegister(renewDay, renewTime);
        setRenewOn(true);
        pushToast('success', `已注册每周${DAY_LABEL[renewDay] ?? renewDay} ${renewTime} 凭证续期任务`);
      }
    } catch (err) {
      pushToast('error', `续期任务操作失败：${String(err)}`);
    } finally {
      setTaskBusy(false);
    }
  };

  return (
    <div className="card p-4">
      <div className="mb-3 flex items-center gap-2">
        <ListChecks size={16} className="text-emerald-500" />
        <h2 className="font-medium">任务配置</h2>
      </div>

      {/* JWT Token 定时续期：refreshToken 配置 + 每周兜底任务（注册即生效） */}
      <h3 className="mb-1 font-medium">JWT Token 定时续期</h3>
      <div className="grid gap-3 lg:grid-cols-2">
        <label className="block">
          <span className="mb-1 block text-xs font-medium text-slate-500">保活阈值（天）</span>
          <input
            type="number"
            min={0}
            className="input w-full"
            value={settings?.keepalive_days ?? 0}
            onChange={(e) => patch({ keepalive_days: Number(e.target.value) || 0 })}
          />
          <span className="mt-1 block text-xs text-slate-400">0 = 每天无条件刷新全部带 refreshToken 账号</span>
        </label>
        <label className="block">
          <span className="mb-1 block text-xs font-medium text-slate-500">惰性刷新（小时）</span>
          <input
            type="number"
            min={1}
            className="input w-full"
            value={settings?.lazy_refresh_hours ?? 24}
            onChange={(e) => patch({ lazy_refresh_hours: Number(e.target.value) || 24 })}
          />
          <span className="mt-1 block text-xs text-slate-400">剩余有效期低于该值才触发刷新（默认 24）</span>
        </label>
      </div>
      <div className="mt-3 rounded-lg border border-slate-100 p-3 dark:border-zinc-800">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <div>
            <div className="text-sm font-medium">每周兜底续期任务</div>
            <div className="text-xs text-slate-400">
              {renewOn ? `已注册：每周${DAY_LABEL[renewDay] ?? renewDay} ${renewTime} 惰性刷新临期账号凭证` : '未注册：凭证临期后需手动续期'}
            </div>
          </div>
          <div className="flex items-center gap-2">
            <select
              className="input h-9 !w-24 text-sm"
              value={renewDay}
              onChange={(e) => setRenewDay(e.target.value)}
            >
              {Object.entries(DAY_LABEL).map(([k, v]) => (
                <option key={k} value={k}>
                  {v}
                </option>
              ))}
            </select>
            <input
              type="time"
              value={renewTime}
              onChange={(e) => setRenewTime(e.target.value || '10:30')}
              className="input h-9 !w-28 text-sm"
            />
            <button className="btn-outline !px-2 !py-1 text-xs" disabled={taskBusy} onClick={() => void toggleRenew()}>
              {taskBusy ? <Spinner /> : null} {renewOn ? '卸载' : '注册'}
            </button>
          </div>
        </div>
      </div>

      <div className="my-4 border-t border-slate-100 dark:border-zinc-800" />

      {/* 自动签到：启动补签 + 每日签到任务（时刻可改） + UI 坐标点击兜底 */}
      <h3 className="mb-1 font-medium">自动签到</h3>
      <div className="space-y-3">
        <label className="flex items-start gap-2">
          <input
            type="checkbox"
            className="mt-0.5"
            checked={settings?.auto_checkin ?? false}
            onChange={(e) => patch({ auto_checkin: e.target.checked })}
          />
          <span className="text-sm">
            启用自动签到（启动补签）
            <span className="block text-xs text-slate-400">
              应用启动时立即核验服务端状态，未签到账号会自动补签；同时作为应用内每日{' '}
              {checkinHhmm || '09:10'} Rust 调度签到的总开关（关闭后仅 Windows 计划任务生效）
            </span>
          </span>
        </label>
        {/* 每日签到调度时刻（wb_checkin_hhmm 存 app Settings，随「保存配置」统一提交） */}
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-xs font-medium text-slate-500">每日签到时刻</span>
          <input
            type="time"
            value={checkinHhmm}
            onChange={(e) => setCheckinHhmm(e.target.value || '09:10')}
            className="input h-9 !w-28 text-sm"
          />
          <span className="text-xs text-slate-400">
            每天 {checkinHhmm || '09:10'} 自动签到；当天已过该时刻，下次启动应用会自动补跑
          </span>
        </div>
        <div className="rounded-lg border border-slate-100 p-3 dark:border-zinc-800">
          <div className="mb-2 text-xs text-slate-400">
            应用内置 Rust 定时调度器：应用运行期间每日 {checkinHhmm || '09:10'} 自动签到（晚于该时刻启动会自动补跑，无需管理员权限，失败
            30 分钟后自动重试）。下方注册的 Windows 计划任务作为兜底，在应用未启动时于指定时刻直接运行签到（注册/卸载需要管理员权限）。
          </div>
          <div className="flex flex-wrap items-center justify-between gap-2">
            <div>
              <div className="text-sm font-medium">Windows 计划任务（兜底）</div>
              <div className="text-xs text-slate-400">
                {tasks.length > 0 ? `已注册：${tasks.join('、')}` : '未注册（第二时刻可清空 = 单时段）'}
              </div>
            </div>
            <div className="flex items-center gap-2">
              <input type="time" value={t1} onChange={(e) => setT1(e.target.value || '09:00')} className="input h-9 !w-28 text-sm" />
              <input type="time" value={t2} onChange={(e) => setT2(e.target.value)} className="input h-9 !w-28 text-sm" />
              {tasks.length === 0 ? (
                <button className="btn-outline !px-2 !py-1 text-xs" disabled={taskBusy} onClick={() => void registerTasks()}>
                  {taskBusy ? <Spinner /> : null} 注册
                </button>
              ) : (
                <button className="btn-outline !px-2 !py-1 text-xs" disabled={taskBusy} onClick={() => void unregisterTasks()}>
                  {taskBusy ? <Spinner /> : null} 卸载
                </button>
              )}
            </div>
          </div>
        </div>

        {/* UI 坐标点击签到兜底（F-18）：仅手动触发、默认关闭 */}
        <div className="rounded-lg border border-slate-100 p-3 dark:border-zinc-800">
          <div className="mb-2 flex items-center justify-between gap-2">
            <div className="flex items-center gap-2">
              <MousePointerClick size={15} className="text-amber-500" />
              <span className="text-sm font-medium">UI 坐标点击兜底</span>
              <Badge tone={settings?.ui_click_enabled ? 'amber' : 'slate'}>
                {settings?.ui_click_enabled ? '已启用' : '默认关闭'}
              </Badge>
            </div>
            <label className="flex cursor-pointer items-center gap-1.5 text-xs text-slate-500">
              <input
                type="checkbox"
                checked={settings?.ui_click_enabled ?? false}
                onChange={(e) => void savePatch({ ui_click_enabled: e.target.checked })}
              />
              启用（最后手段）
            </label>
          </div>
          <p className="mb-2 text-xs text-slate-400">
            签到 API 不可用时的最后手段：驱动鼠标对客户端「立即签到」按钮做坐标点击。
            使用方法：打开客户端签到页 → 把鼠标悬停在签到按钮上 → 点「取点」记录坐标 → 回来点「执行点击」。
            全程仅手动触发，不会自动连点。
          </p>
          <div className="flex flex-wrap items-center gap-2 text-xs">
            <span className="rounded-md border border-slate-100 px-2 py-1 font-mono dark:border-zinc-800">
              坐标：{settings?.ui_click_x ? `${settings.ui_click_x}, ${settings.ui_click_y}` : '未配置'}
            </span>
            <button
              className="btn-outline !px-2 !py-1"
              disabled={savingSettings || uiClickBusy != null}
              onClick={() => {
                setUiClickBusy('capture');
                void api.workbuddy
                  .uiClickCapture()
                  .then((r) => {
                    if (r.ok) {
                      void savePatch({ ui_click_x: r.x, ui_click_y: r.y });
                      pushToast('success', r.message);
                    } else {
                      pushToast('warn', r.message);
                    }
                  })
                  .catch((e) => pushToast('error', String(e)))
                  .finally(() => setUiClickBusy(null));
              }}
            >
              {uiClickBusy === 'capture' ? '取点中…' : '取点（3 秒倒计时）'}
            </button>
            <button
              className="btn-outline !px-2 !py-1"
              disabled={!settings?.ui_click_enabled || savingSettings || uiClickBusy != null}
              title={settings?.ui_click_enabled ? '' : '先启用后才可执行（F-18 默认关闭）'}
              onClick={() => {
                setUiClickBusy('run');
                void api.workbuddy
                  .uiClickCheckin()
                  .then((r) => pushToast(r.ok ? 'success' : 'warn', r.message))
                  .catch((e) => pushToast('error', String(e)))
                  .finally(() => setUiClickBusy(null));
              }}
            >
              {uiClickBusy === 'run' ? '执行中…' : '执行点击'}
            </button>
          </div>
        </div>
      </div>

      <div className="my-4 border-t border-slate-100 dark:border-zinc-800" />

      {/* 积分与 Token 数据同步：看板数据定时刷新（模式/时刻随「保存配置」生效） */}
      <h3 className="mb-1 font-medium">积分与 Token 数据同步</h3>
      <p className="mb-3 text-xs text-slate-400">
        应用运行期间按所选模式自动同步积分与 Token 看板数据（积分快照 + Token 统计重扫 + 官网用量刷新，一次配置管两个看板）；
        无可用凭证时静默跳过，失败 30 分钟后自动重试。模式与时刻随右上角「保存配置」生效。
      </p>
      <div className="flex flex-wrap items-center gap-4">
        <label className="flex items-center gap-2 text-sm">
          <span className="text-xs font-medium text-slate-500">同步模式</span>
          <select
            className="input h-9 !w-32 text-sm"
            value={creditsForm.mode}
            onChange={(e) => setCreditsForm({ ...creditsForm, mode: e.target.value })}
          >
            <option value="daily">每日定时</option>
            <option value="hourly">每小时</option>
            <option value="off">关闭</option>
          </select>
        </label>
        {creditsForm.mode === 'daily' && (
          <div className="flex items-center gap-2">
            <span className="text-xs font-medium text-slate-500">每日执行时刻</span>
            <input
              type="time"
              value={creditsForm.hhmm}
              onChange={(e) => setCreditsForm({ ...creditsForm, hhmm: e.target.value || '23:30' })}
              className="input h-9 !w-28 text-sm"
            />
            <span className="text-xs text-slate-400">每天 {creditsForm.hhmm || '23:30'} 执行（应用关闭期间不执行）</span>
          </div>
        )}
        {creditsForm.mode === 'hourly' && (
          <span className="text-xs text-slate-400">应用运行期间每小时同步一次</span>
        )}
      </div>

      <div className="my-4 border-t border-slate-100 dark:border-zinc-800" />

      {/* 自动成长：应用内调度器每日成长轮（开关/时刻随「保存配置」生效） */}
      <h3 className="mb-1 font-medium">自动成长</h3>
      <p className="mb-3 text-xs text-slate-400">
        应用运行期间每日到点自动执行成长轮（旅行 / 盲盒 / 任务，按下方开关项）；手动执行入口在「签到与成长」页。
        开关与时刻随右上角「保存配置」生效。
      </p>
      <div className="space-y-3">
        <div className="flex flex-wrap items-center gap-4">
          <label className="flex items-center gap-2 text-sm">
            <input
              type="checkbox"
              checked={growthForm.enabled}
              onChange={(e) => setGrowthForm({ ...growthForm, enabled: e.target.checked })}
            />
            每日执行成长任务
          </label>
          <div className="flex items-center gap-2">
            <span className="text-xs font-medium text-slate-500">每日执行时刻</span>
            <input
              type="time"
              value={growthForm.hhmm}
              onChange={(e) => setGrowthForm({ ...growthForm, hhmm: e.target.value || '09:00' })}
              disabled={!growthForm.enabled}
              className="input h-9 !w-28 text-sm"
            />
            {growthForm.enabled && (
              <span className="text-xs text-slate-400">每天 {growthForm.hhmm || '09:00'} 执行（应用关闭期间不执行）</span>
            )}
          </div>
        </div>
        <div className="grid gap-3 text-sm lg:grid-cols-3">
          <label className="flex items-center gap-2">
            <input
              type="checkbox"
              checked={settings?.growth_travel ?? true}
              onChange={(e) => patch({ growth_travel: e.target.checked })}
            />
            成长旅行
          </label>
          <label className="flex items-center gap-2">
            <input
              type="checkbox"
              checked={settings?.growth_lottery ?? true}
              onChange={(e) => patch({ growth_lottery: e.target.checked })}
            />
            盲盒抽奖
          </label>
          <label className="flex items-center gap-2">
            <input
              type="checkbox"
              checked={settings?.growth_tasks ?? true}
              onChange={(e) => patch({ growth_tasks: e.target.checked })}
            />
            每日任务
          </label>
        </div>
      </div>
    </div>
  );
}

export default function BuddySettings() {
  const pushToast = useAppStore((s) => s.pushToast);
  const saveSettings = useAppStore((s) => s.saveSettings);
  const [env, setEnv] = useState<WorkBuddyEnvCheck | null>(null);
  const [settings, setSettings] = useState<WorkBuddySettings | null>(null);
  const [saving, setSaving] = useState(false);
  // 应用级路径配置（workbuddy_path / codebuddy_path / wb_auth_file_path 存 app Settings）
  const [appSettings, setAppSettings] = useState<Settings | null>(null);
  const [locWb, setLocWb] = useState<AppLocate | null>(null);
  const [locCb, setLocCb] = useState<AppLocate | null>(null);
  // 路径表单：输入框内直接展示（自动检测预填，可人工修改替换），随右上角「保存配置」统一提交
  const [pathForm, setPathForm] = useState({ workbuddy_path: '', codebuddy_path: '', wb_auth_file_path: '' });
  const [locWbDone, setLocWbDone] = useState(false);
  const [locCbDone, setLocCbDone] = useState(false);
  // 成长调度表单（wb_growth_enabled/hhmm 存 app Settings，随「保存配置」统一提交）
  const [growthForm, setGrowthForm] = useState({ enabled: true, hhmm: '09:00' });
  // 积分与 Token 同步表单（wb_credits_sync_mode/hhmm 存 app Settings，随「保存配置」统一提交）
  const [creditsForm, setCreditsForm] = useState({ mode: 'daily', hhmm: '23:30' });
  // 每日签到调度时刻（wb_checkin_hhmm 存 app Settings，随「保存配置」统一提交）
  const [checkinHhmm, setCheckinHhmm] = useState('09:10');
  const [detecting, setDetecting] = useState<string | null>(null);
  const [refreshing, setRefreshing] = useState(false);
  const prefilled = useRef(false);

  const refresh = useCallback(async () => {
    setRefreshing(true);
    try {
      const [e, st] = await Promise.all([
        api.workbuddy.envCheck(),
        api.workbuddy.settingsGet().catch(() => null),
      ]);
      setEnv(e);
      setSettings(st);
    } catch (err) {
      pushToast('error', `环境检测失败：${String(err)}`);
    }
    setRefreshing(false);
    // 路径配置与四级探测来源（失败静默，不阻断主检测）
    api.misc
      .settingsGet()
      .then(setAppSettings)
      .catch(() => setAppSettings(null));
    api.env
      .locate('workbuddy')
      .then(setLocWb)
      .catch(() => setLocWb(null))
      .finally(() => setLocWbDone(true));
    api.env
      .locate('codebuddy')
      .then(setLocCb)
      .catch(() => setLocCb(null))
      .finally(() => setLocCbDone(true));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // 首次数据就绪后预填路径输入框：人工配置优先，否则自动检测值（auth 为实际读取路径）
  useEffect(() => {
    if (prefilled.current || !appSettings || !locWbDone || !locCbDone) return;
    prefilled.current = true;
    setPathForm({
      workbuddy_path: appSettings.workbuddy_path ?? locWb?.exe ?? '',
      codebuddy_path: appSettings.codebuddy_path ?? locCb?.exe ?? '',
      wb_auth_file_path: appSettings.wb_auth_file_path ?? env?.auth_file_path ?? '',
    });
    setGrowthForm({
      enabled: appSettings.wb_growth_enabled ?? true,
      hhmm: appSettings.wb_growth_hhmm || '09:00',
    });
    setCreditsForm({
      mode: appSettings.wb_credits_sync_mode || 'daily',
      hhmm: appSettings.wb_credits_sync_hhmm || '23:30',
    });
    setCheckinHhmm(appSettings.wb_checkin_hhmm || '09:10');
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [appSettings, locWbDone, locCbDone, locWb, locCb, env]);

  const save = async () => {
    if (!settings) return;
    setSaving(true);
    try {
      await withMinDelay(api.workbuddy.settingsSet(settings), 800);
      // 路径配置随「保存配置」一并提交：settings_set 真 patch 语义，只写本页维护的字段——
      // 不 spread appSettings 快照（挂载时旧快照会把系统设置弹框等外部通道刚保存的值回滚）
      await api.misc.settingsSet({
        workbuddy_path: pathForm.workbuddy_path.trim() || null,
        codebuddy_path: pathForm.codebuddy_path.trim() || null,
        wb_auth_file_path: pathForm.wb_auth_file_path.trim() || null,
        wb_growth_enabled: growthForm.enabled,
        wb_growth_hhmm: growthForm.hhmm.trim() || '09:00',
        wb_credits_sync_mode: creditsForm.mode,
        wb_credits_sync_hhmm: creditsForm.hhmm.trim() || '23:30',
        wb_checkin_hhmm: checkinHhmm.trim() || '09:10',
      });
      pushToast('success', '配置已保存');
      await refresh();
    } catch (err) {
      pushToast('error', `保存失败：${String(err)}`);
    } finally {
      setSaving(false);
    }
  };

  const patch = (p: Partial<WorkBuddySettings>) => {
    setSettings((prev) => (prev ? { ...prev, ...p } : prev));
  };

  /** F-74：切换时自动迁移会话——勾选即存（app Settings 真 patch 语义），失败原地回滚 */
  const toggleSwitchMigrateChats = async (v: boolean) => {
    const prev = appSettings;
    setAppSettings((p) => (p ? { ...p, buddy_switch_migrate_chats: v } : p));
    try {
      await saveSettings({ buddy_switch_migrate_chats: v });
      pushToast(
        'success',
        v
          ? '已开启：WorkBuddy/CodeBuddy 切换账号前会自动备份当前账号会话并以新 id 复制到目标账号（进度见切换进度流）'
          : '已关闭：切换账号仅切换登录态，不再写会话',
      );
    } catch (err) {
      setAppSettings(prev);
      pushToast('error', `保存失败：${String(err)}`);
    }
  };

  /** 探测来源中文标签（settings = 人工指定） */
  const LOCATE_SOURCE_LABEL: Record<string, string> = {
    settings: '人工指定',
    registry: '注册表',
    default: '默认路径',
    process: '运行进程',
    not_found: '未检测到',
  };

  /** 自动检测：四级探测 → 命中即替换输入框内容，随「保存配置」统一生效 */
  const detectPath = async (target: 'workbuddy' | 'codebuddy') => {
    setDetecting(target);
    try {
      const r = await withMinDelay(api.env.locate(target), 600);
      const key = target === 'workbuddy' ? 'workbuddy_path' : 'codebuddy_path';
      if (r.exe) {
        setPathForm((f) => ({ ...f, [key]: r.exe as string }));
        pushToast('info', `已定位（${LOCATE_SOURCE_LABEL[r.source] ?? r.source}${r.version ? `，v${r.version}` : ''}）`);
      } else {
        pushToast('warn', '未检测到客户端，请人工填写 exe 路径');
      }
    } catch (err) {
      pushToast('error', `检测失败：${String(err)}`);
    } finally {
      setDetecting(null);
    }
  };

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="Buddy · 环境配置"
        desc="通用配置 · 任务配置 · 通知与轮换"
        actions={
          <>
            <button className="btn-outline" onClick={() => void refresh()} disabled={refreshing}>
              <RefreshCw size={15} className={refreshing ? 'animate-spin' : ''} /> 重新检测
            </button>
            <button className="btn-outline" onClick={() => void save()} disabled={saving || !settings}>
              {saving ? <Spinner /> : <Save size={15} />} 保存配置
            </button>
          </>
        }
      />

      {/* 通用配置 + 任务配置：一行两列 */}
      <div className="grid items-start gap-4 lg:grid-cols-2">
        {/* 左：通用配置（应用环境 + 切换账号自动迁移会话）+ CLI 自动轮换（同列其下） */}
        <div className="space-y-4">
        <div className="card p-4">
          <div className="mb-3 flex items-center gap-2">
            <SlidersHorizontal size={16} className="text-emerald-500" />
            <h2 className="font-medium">通用配置</h2>
          </div>

          <div className="mb-1 flex items-center justify-between">
            <h3 className="font-medium">应用环境</h3>
            <div className="flex items-center gap-2">
              <Badge tone={env?.installed ? 'green' : 'red'}>
                WorkBuddy{env?.installed ? `已安装${env?.version ? ` v${env.version}` : ''}` : '未安装'}
              </Badge>
              <Badge tone={env?.running ? 'green' : 'slate'}>{env?.running ? '运行中' : '已停止'}</Badge>
            </div>
          </div>
          <div className="space-y-3 text-xs">
            {/* WorkBuddy 客户端路径（输入框直显：自动检测预填，可人工替换，随「保存配置」生效） */}
            <div className="rounded-lg border border-slate-100 p-3 dark:border-zinc-800">
              <div className="mb-2 flex items-center justify-between font-medium text-slate-500">
                WorkBuddy 客户端路径
                <Badge tone={locWb?.exe ? 'green' : 'amber'}>{LOCATE_SOURCE_LABEL[locWb?.source ?? 'not_found']}</Badge>
              </div>
              <div className="flex items-center gap-2">
                <input
                  className="input flex-1 font-mono text-xs"
                  value={pathForm.workbuddy_path}
                  onChange={(e) => setPathForm((f) => ({ ...f, workbuddy_path: e.target.value }))}
                  placeholder={locWb?.exe ?? '未检测到，留空 = 自动检测'}
                />
                <button className="btn-outline shrink-0 !px-2 !py-1" onClick={() => void detectPath('workbuddy')} disabled={detecting !== null}>
                  {detecting === 'workbuddy' ? <Spinner /> : <Search size={13} />} 自动检测
                </button>
              </div>
            </div>
            {/* CodeBuddy 客户端路径 */}
            <div className="rounded-lg border border-slate-100 p-3 dark:border-zinc-800">
              <div className="mb-2 flex items-center justify-between font-medium text-slate-500">
                CodeBuddy 客户端路径
                <Badge tone={locCb?.exe ? 'green' : 'amber'}>{LOCATE_SOURCE_LABEL[locCb?.source ?? 'not_found']}</Badge>
              </div>
              <div className="flex items-center gap-2">
                <input
                  className="input flex-1 font-mono text-xs"
                  value={pathForm.codebuddy_path}
                  onChange={(e) => setPathForm((f) => ({ ...f, codebuddy_path: e.target.value }))}
                  placeholder={locCb?.exe ?? '未检测到，留空 = 自动检测'}
                />
                <button className="btn-outline shrink-0 !px-2 !py-1" onClick={() => void detectPath('codebuddy')} disabled={detecting !== null}>
                  {detecting === 'codebuddy' ? <Spinner /> : <Search size={13} />} 自动检测
                </button>
              </div>
            </div>
            {/* auth 文件路径（实际读取路径直显；人工覆盖 + 打开目录） */}
            <div className="rounded-lg border border-slate-100 p-3 dark:border-zinc-800">
              <div className="mb-2 flex items-center justify-between font-medium text-slate-500">
                auth 文件路径
                <Badge tone={env?.auth_file_exists ? 'green' : 'amber'}>{env?.auth_file_exists ? '存在 ✓' : '不存在'}</Badge>
              </div>
              <div className="flex items-center gap-2">
                <input
                  className="input flex-1 font-mono text-xs"
                  value={pathForm.wb_auth_file_path}
                  onChange={(e) => setPathForm((f) => ({ ...f, wb_auth_file_path: e.target.value }))}
                  placeholder={env?.auth_file_path || '检测中…'}
                />
                <button className="btn-outline shrink-0 !px-2 !py-1" onClick={() => void api.workbuddy.openAuthDir().catch((e) => pushToast('error', String(e)))}>
                  <FolderOpen size={13} /> 打开所在目录
                </button>
              </div>
              <p className="mt-1.5 text-slate-400">路径改动随右上角「保存配置」一并生效；留空恢复默认位置</p>
            </div>
          </div>

          <div className="my-4 border-t border-slate-100 dark:border-zinc-800" />

          {/* 切换时自动迁移会话（F-74）：默认关；勾选即存，进度走切换进度流 */}
          <div className="mb-2 flex items-center gap-2 font-medium">
            切换账号时自动迁移会话
            <Badge tone={appSettings?.buddy_switch_migrate_chats ? 'green' : 'slate'}>
              {appSettings?.buddy_switch_migrate_chats ? '已开启' : '已关闭'}
            </Badge>
          </div>
          <label className="flex cursor-pointer items-start gap-2 text-xs text-slate-600 dark:text-zinc-300">
            <input
              type="checkbox"
              className="mt-0.5 h-3.5 w-3.5 rounded border-slate-300 text-brand-600 focus:ring-brand-500"
              checked={!!appSettings?.buddy_switch_migrate_chats}
              onChange={(e) => void toggleSwitchMigrateChats(e.target.checked)}
            />
            <span>
              切换 WorkBuddy/CodeBuddy 账号前，自动备份当前账号的会话三件套（projects + workbuddy.db +
              edge-sync-mapping-v2.db），再以新 id 复制到目标账号名下并注册云端映射，之后才执行桥的
              Stop→Restore→Start。任何一步失败只告警不阻断登录态切换（可稍后在账号管理页手动「复制会话」）。
            </span>
          </label>
        </div>

        {/* CLI 五重防护自动轮换（CodeBuddy）：与通用配置同列，置于其下 */}
        <CliRotateCard settings={settings} patch={patch} />
        </div>

        {/* 右：任务配置（JWT 续期 / 自动签到 / 积分与 Token 同步 / 自动成长） */}
        <TaskConfigCard
          settings={settings}
          patch={patch}
          growthForm={growthForm}
          setGrowthForm={setGrowthForm}
          creditsForm={creditsForm}
          setCreditsForm={setCreditsForm}
          checkinHhmm={checkinHhmm}
          setCheckinHhmm={setCheckinHhmm}
        />
      </div>
    </div>
  );
}
