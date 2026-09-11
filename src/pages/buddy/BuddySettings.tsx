import { useCallback, useEffect, useState } from 'react';
import { Save, FolderOpen, RefreshCw, TerminalSquare, Play, CalendarClock, MousePointerClick } from 'lucide-react';
import { open } from '@tauri-apps/plugin-shell';
import { localDataDir } from '@tauri-apps/api/path';
import PageHeader from '../../components/PageHeader';
import { Badge, Spinner } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { withMinDelay } from '../../lib/delay';
import type { WorkBuddySettings, WorkBuddyEnvCheck, WbCliStatus, WbCliRotateLog } from '../../types';

/**
 * buddy-settings 环境配置（§3.7.5，F-55/F-59/F-13）：
 * 环境卡 + 签到配置卡（自动签到 + 定时任务 + 坐标点击兜底）+ CLI 五重防护轮换卡。
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
    <div className="mt-4 card p-4">
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

/** 签到配置卡（F-55/F-16/F-18）：自动签到参数 + 定时任务注册 + UI 坐标点击兜底 */
function CheckinConfigCard({
  settings,
  patch,
}: {
  settings: WorkBuddySettings | null;
  patch: (p: Partial<WorkBuddySettings>) => void;
}) {
  const pushToast = useAppStore((s) => s.pushToast);
  const [tasks, setTasks] = useState<string[]>([]);
  const [renewOn, setRenewOn] = useState(false);
  const [savingSettings, setSavingSettings] = useState(false);

  const refreshTasks = useCallback(() => {
    api.workbuddy.checkinTaskStatus().then(setTasks).catch(() => setTasks([]));
    api.workbuddy.renewTaskStatus().then(setRenewOn).catch(() => setRenewOn(false));
  }, []);

  useEffect(() => {
    refreshTasks();
  }, [refreshTasks]);

  // settings 单项保存（走 settingsSet 全量 patch，与父页「保存配置」同通道）
  const savePatch = async (p: Partial<WorkBuddySettings>) => {
    if (!settings) return;
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
    try {
      await api.workbuddy.checkinTaskRegister(['09:00', '21:00']);
      setTasks(await api.workbuddy.checkinTaskStatus());
      pushToast('success', '已注册每日 09:00 / 21:00 双时段签到任务');
    } catch (err) {
      pushToast('error', `注册任务失败：${String(err)}`);
    }
  };

  const toggleRenew = async () => {
    try {
      if (renewOn) {
        await api.workbuddy.renewTaskUnregister();
        setRenewOn(false);
        pushToast('info', '已卸载每周续期任务');
      } else {
        await api.workbuddy.renewTaskRegister('SUN');
        setRenewOn(true);
        pushToast('success', '已注册每周日 10:30 凭证续期任务');
      }
    } catch (err) {
      pushToast('error', `续期任务操作失败：${String(err)}`);
    }
  };

  return (
    <div className="mt-4 card p-4">
      <div className="mb-3 flex items-center gap-2">
        <CalendarClock size={16} className="text-brand-500" />
        <span className="text-sm font-medium">签到配置</span>
      </div>
      <div className="space-y-3">
        {/* 自动签到（F-55） */}
        <label className="flex items-start gap-2">
          <input
            type="checkbox"
            className="mt-0.5"
            checked={settings?.auto_checkin ?? false}
            onChange={(e) => patch({ auto_checkin: e.target.checked })}
          />
          <span className="text-sm">
            启用自动签到（启动补签）
            <span className="block text-xs text-slate-400">应用启动时立即核验服务端状态，未签到账号会自动补签</span>
          </span>
        </label>
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

        {/* 定时任务（F-16） */}
        <div className="grid gap-3 lg:grid-cols-2">
          <div className="rounded-lg border border-slate-100 p-3 dark:border-zinc-800">
            <div className="flex items-center justify-between">
              <div>
                <div className="text-sm font-medium">每日签到 · 09:00 / 21:00 双时段</div>
                <div className="text-xs text-slate-400">
                  {tasks.length > 0 ? `已注册：${tasks.join('、')}` : '未注册'}
                </div>
              </div>
              <div className="flex gap-2">
                {tasks.length === 0 ? (
                  <button className="btn-outline !px-2 !py-1 text-xs" onClick={() => void registerTasks()}>注册</button>
                ) : (
                  <button
                    className="btn-outline !px-2 !py-1 text-xs"
                    onClick={() =>
                      void api.workbuddy
                        .checkinTaskUnregister()
                        .then(() => {
                          setTasks([]);
                          pushToast('info', '已卸载定时签到任务');
                        })
                        .catch((e) => pushToast('error', String(e)))
                    }
                  >
                    卸载
                  </button>
                )}
              </div>
            </div>
          </div>
          <div className="rounded-lg border border-slate-100 p-3 dark:border-zinc-800">
            <div className="flex items-center justify-between">
              <div>
                <div className="text-sm font-medium">token 每周兜底续期（周日 10:30）</div>
                <div className="text-xs text-slate-400">{renewOn ? '已注册：惰性刷新临期账号凭证' : '未注册：凭证临期后需手动续期'}</div>
              </div>
              <button className="btn-outline !px-2 !py-1 text-xs" onClick={() => void toggleRenew()}>
                {renewOn ? '卸载' : '注册'}
              </button>
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
              disabled={savingSettings}
              onClick={() =>
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
              }
            >
              取点（3 秒倒计时）
            </button>
            <button
              className="btn-outline !px-2 !py-1"
              disabled={!settings?.ui_click_enabled || savingSettings}
              title={settings?.ui_click_enabled ? '' : '先启用后才可执行（F-18 默认关闭）'}
              onClick={() =>
                void api.workbuddy
                  .uiClickCheckin()
                  .then((r) => pushToast(r.ok ? 'success' : 'warn', r.message))
                  .catch((e) => pushToast('error', String(e)))
              }
            >
              执行点击
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}

export default function BuddySettings() {
  const pushToast = useAppStore((s) => s.pushToast);
  const [env, setEnv] = useState<WorkBuddyEnvCheck | null>(null);
  const [settings, setSettings] = useState<WorkBuddySettings | null>(null);
  const [saving, setSaving] = useState(false);

  const refresh = useCallback(async () => {
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
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const save = async () => {
    if (!settings) return;
    setSaving(true);
    try {
      await withMinDelay(api.workbuddy.settingsSet(settings), 800);
      pushToast('success', '配置已保存');
    } catch (err) {
      pushToast('error', `保存失败：${String(err)}`);
    } finally {
      setSaving(false);
    }
  };

  const patch = (p: Partial<WorkBuddySettings>) => {
    setSettings((prev) => (prev ? { ...prev, ...p } : prev));
  };

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="WorkBuddy · 环境配置"
        desc="客户端环境 · 签到配置 · 通知与轮换"
        actions={
          <>
            <button className="btn-outline" onClick={() => void refresh()}>
              <RefreshCw size={15} /> 重新检测
            </button>
            <button className="btn-primary" onClick={() => void save()} disabled={saving || !settings}>
              {saving ? <Spinner className="text-white" /> : <Save size={15} />} 保存配置
            </button>
          </>
        }
      />

      {/* 环境卡 */}
      <div className="card p-4">
        <div className="mb-3 flex items-center justify-between">
          <span className="text-sm font-medium">环境</span>
          <div className="flex gap-2">
            <Badge tone={env?.installed ? 'green' : 'red'}>{env?.installed ? '已安装' : '未安装'}</Badge>
            <Badge tone={env?.running ? 'green' : 'slate'}>{env?.running ? '运行中' : '已停止'}</Badge>
            {env?.version && <Badge tone="slate">v{env.version}</Badge>}
          </div>
        </div>
        <div className="grid gap-3 lg:grid-cols-2">
          <div className="rounded-lg border border-slate-100 p-3 text-xs dark:border-zinc-800">
            <div className="mb-1 font-medium text-slate-500">客户端路径（自动检测）</div>
            <div className="break-all font-mono text-slate-600 dark:text-zinc-300">{env?.exe ?? '未检测到'}</div>
            <div className="mt-1 text-slate-400">手动路径覆盖可在 Trae 页「环境配置」的 workbuddy_path 设置（随后续批次开放）</div>
          </div>
          <div className="rounded-lg border border-slate-100 p-3 text-xs dark:border-zinc-800">
            <div className="mb-1 flex items-center justify-between font-medium text-slate-500">
              auth 文件路径
              <Badge tone={env?.auth_file_exists ? 'green' : 'amber'}>{env?.auth_file_exists ? '存在 ✓' : '不存在'}</Badge>
            </div>
            <div className="break-all font-mono text-slate-600 dark:text-zinc-300">
              %LOCALAPPDATA%\CodeBuddyExtension\Data\Public\auth\workbuddy-desktop.info
            </div>
            <button
              className="mt-2 flex items-center gap-1 text-xs text-brand-600 hover:underline dark:text-brand-400"
              onClick={() =>
                void localDataDir()
                  .then((d) => open(`file:///${d.replace(/\\/g, '/')}CodeBuddyExtension/Data/Public/auth`))
                  .catch((e) => pushToast('error', `打开目录失败：${String(e)}`))
              }
            >
              <FolderOpen size={13} /> 打开所在目录
            </button>
          </div>
        </div>
      </div>

      {/* 签到配置（F-55/F-16/F-18）：自动签到 + 定时任务 + 坐标点击兜底 */}
      <CheckinConfigCard settings={settings} patch={patch} />

      {/* 通知渠道（F-19） */}
      <div className="mt-4 card p-4">
        <div className="mb-3 text-sm font-medium">失败通知渠道</div>
        <div className="space-y-3">
          <p className="text-xs text-slate-400">
            桌面通知之外的可选渠道：签到/补签失败等关键事件会同时推送到已配置的渠道（留空 = 关闭）。
          </p>
          <label className="block">
            <span className="mb-1 block text-xs font-medium text-slate-500">企业微信群机器人 Webhook</span>
            <input
              className="input w-full font-mono text-xs"
              value={settings?.notify_wechat_webhook ?? ''}
              onChange={(e) => patch({ notify_wechat_webhook: e.target.value || null })}
              placeholder="https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=xxx"
            />
            <span className="mt-1 block text-xs text-slate-400">群机器人消息：标题 + 失败摘要</span>
          </label>
          <label className="block">
            <span className="mb-1 block text-xs font-medium text-slate-500">Server酱 SendKey</span>
            <input
              className="input w-full font-mono text-xs"
              value={settings?.notify_serverchan_sendkey ?? ''}
              onChange={(e) => patch({ notify_serverchan_sendkey: e.target.value || null })}
              placeholder="SCTxxxxxxxx（sctapi.ftqq.com）"
            />
            <span className="mt-1 block text-xs text-slate-400">推送到微信服务号；Key 仅本地保存，不进日志</span>
          </label>
        </div>
      </div>

      {/* CLI 五重防护自动轮换（F-06/F-59，批次3） */}
      <CliRotateCard settings={settings} patch={patch} />
    </div>
  );
}
