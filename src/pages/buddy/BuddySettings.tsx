import { useCallback, useEffect, useState } from 'react';
import { Save, RefreshCw, CalendarClock, ScanSearch } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import SchedulerTasksCard, { type TaskToggleOverride } from '../../components/SchedulerTasksCard';
import { Spinner } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { withMinDelay } from '../../lib/delay';
import type { Settings, WorkBuddySettings } from '../../types';

/**
 * Buddy · 环境配置（§3.7.5，F-55/F-59/F-13）：
 * 定时任务（自动签到和成长 / 自动续期 / 自动同步服务器数据，开关与执行时刻即时保存）+
 * 签到参数（保活阈值 / 惰性刷新）+ 本机 auth 文件路径（「扫描本机账号」数据源）。
 * wb-checkin 开关绑定 WorkBuddySettings.auto_checkin（单源：同时门控定时签到与启动补签）；
 * 失败通知渠道已并入全局「通知渠道」（系统设置弹框，Trae / Buddy 共用）。
 * auth 文件路径：桌面版默认扫描本机客户端布局，Web/Docker 版须将宿主机客户端
 * auth 文件挂载进容器后在此填写容器内路径（settings_set 为 patch 合并语义）。
 */
export default function BuddySettings() {
  const pushToast = useAppStore((s) => s.pushToast);
  const [settings, setSettings] = useState<WorkBuddySettings | null>(null);
  const [saving, setSaving] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  // wb-checkin 开关（auto_checkin）即时保存 pending
  const [checkinToggleBusy, setCheckinToggleBusy] = useState(false);
  // 本机 auth 文件路径（存全局 Settings.wb_auth_file_path，随「保存配置」统一提交）
  const [authPath, setAuthPath] = useState('');

  const refresh = useCallback(async () => {
    setRefreshing(true);
    try {
      setSettings(await api.workbuddy.settingsGet().catch(() => null));
      const app = await api.misc.settingsGet().catch(() => null);
      setAuthPath(app?.wb_auth_file_path ?? '');
    } catch (err) {
      pushToast('error', `读取配置失败：${String(err)}`);
    }
    setRefreshing(false);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const save = async () => {
    if (!settings) return;
    setSaving(true);
    try {
      await withMinDelay(
        Promise.all([
          api.workbuddy.settingsSet(settings),
          api.misc.settingsSet({ ...(await api.misc.settingsGet()), wb_auth_file_path: authPath.trim() || null }),
        ]),
        800,
      );
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

  // wb-checkin 开关：绑定 auto_checkin 并即时保存（与定时任务卡其他开关交互一致）
  const toggleCheckin = async (v: boolean) => {
    if (!settings) return;
    const prev = settings;
    const next = { ...prev, auto_checkin: v };
    setSettings(next);
    setCheckinToggleBusy(true);
    try {
      await withMinDelay(api.workbuddy.settingsSet(next));
      pushToast('success', v ? '自动签到和成长已启用' : '自动签到和成长已停用');
    } catch (err) {
      setSettings(prev);
      pushToast('error', `保存失败：${String(err)}`);
    } finally {
      setCheckinToggleBusy(false);
    }
  };

  const overrides: Record<string, TaskToggleOverride> | undefined = settings
    ? {
        'wb-checkin': { checked: settings.auto_checkin, onToggle: toggleCheckin, busy: checkinToggleBusy },
      }
    : undefined;

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="Buddy · 环境配置"
        desc="定时任务 · 签到参数 · 本机 auth 文件（失败通知在系统设置 → 通知渠道配置）"
        actions={
          <>
            <button className="btn-outline" onClick={() => void refresh()} disabled={refreshing}>
              <RefreshCw size={15} className={refreshing ? 'animate-spin' : ''} /> 刷新
            </button>
            <button className="btn-outline" onClick={() => void save()} disabled={saving || !settings}>
              {saving ? <Spinner /> : <Save size={15} />} 保存配置
            </button>
          </>
        }
      />

      {/* 本机 auth 文件路径（「账号管理 → 扫描本机账号」的数据源）：Web/Docker 版将
          宿主机客户端 auth 文件挂载进容器后填写容器内路径；留空 = 服务端默认布局 */}
      <section className="card mb-4 p-4">
        <div className="mb-1 flex items-center gap-2">
          <ScanSearch size={16} className="text-brand-500" />
          <span className="font-medium">本机 auth 文件（扫描导入）</span>
        </div>
        <p className="mb-3 text-xs text-slate-400">
          「账号管理 → 扫描本机账号」从此文件读取登录凭证。Docker / 远端部署时，请先把宿主机
          WorkBuddy 客户端的 auth 文件（<code className="text-slate-500">%LOCALAPPDATA%\CodeBuddyExtension\Data\Public\auth\workbuddy-desktop.info</code>）
          挂载进容器（如 <code className="text-slate-500">-v /宿主机/auth:/data/auth</code>），再在此填写容器内路径；本地裸跑可留空走默认布局。
        </p>
        <label className="block rounded-lg bg-slate-50 px-3 py-2.5 dark:bg-zinc-900">
          <span className="mb-1 block text-xs font-medium text-slate-500">auth 文件路径（容器内绝对路径）</span>
          <input
            type="text"
            className="input w-full font-mono text-xs"
            placeholder="/data/auth/workbuddy-desktop.info（留空 = 服务端默认布局）"
            value={authPath}
            onChange={(e) => setAuthPath(e.target.value)}
          />
          <span className="mt-1 block text-xs text-slate-400">改动后点击右上角「保存配置」生效；客户端退出登录后文件凭证被清空，需重新登录客户端再扫描</span>
        </label>
      </section>

      <div className="grid items-start gap-4 md:grid-cols-2">
        {/* 左列：签到参数（F-55/F-16；wb-checkin 开关在右侧定时任务卡） */}
        <section className="card p-4">
          <div className="mb-1 flex items-center gap-2">
            <CalendarClock size={16} className="text-brand-500" />
            <span className="font-medium">签到参数</span>
          </div>
          <p className="mb-3 text-xs text-slate-400">自动签到与续期的执行参数，改动后点击「保存配置」生效。</p>
          <div className="grid gap-3 lg:grid-cols-2">
            <label className="block rounded-lg bg-slate-50 px-3 py-2.5 dark:bg-zinc-900">
              <span className="mb-1 block text-xs font-medium text-slate-500">保活阈值（天）</span>
              <input
                type="number"
                min={0}
                className="input w-full"
                value={settings?.keepalive_days ?? 0}
                onChange={(e) => patch({ keepalive_days: Number(e.target.value) || 0 })}
              />
              <span className="mt-1 block text-xs text-slate-400">0 = 每天无条件刷新全部带 refreshToken 账号（推荐）</span>
            </label>
            <label className="block rounded-lg bg-slate-50 px-3 py-2.5 dark:bg-zinc-900">
              <span className="mb-1 block text-xs font-medium text-slate-500">惰性刷新（小时）</span>
              <input
                type="number"
                min={1}
                className="input w-full"
                value={settings?.lazy_refresh_hours ?? 24}
                onChange={(e) => patch({ lazy_refresh_hours: Number(e.target.value) || 24 })}
              />
              <span className="mt-1 block text-xs text-slate-400">剩余有效期低于该值才触发刷新（推荐 24）</span>
            </label>
          </div>
        </section>

        {/* 右列：定时任务（开关与执行时刻即时保存，含最近执行状态；推荐配置 = 全部启用 + 默认时刻） */}
        <SchedulerTasksCard
          taskKeys={['wb-checkin', 'wb-growth', 'wb-renew', 'wb-catalog-sync', 'wb-credits-snapshot']}
          overrides={overrides}
          desc="服务端内置调度器自动执行，覆盖自动签到 / 自动成长 / 自动续期 / 自动同步模型目录与积分看板数据。推荐保持全部启用。"
        />
      </div>
    </div>
  );
}
