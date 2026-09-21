import { useCallback, useEffect, useState } from 'react';
import { Save, RefreshCw, CalendarClock } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { Spinner } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { withMinDelay } from '../../lib/delay';
import type { WorkBuddySettings } from '../../types';

/**
 * buddy-settings 环境配置（§3.7.5，F-55/F-59/F-13）：
 * 签到配置卡（自动签到参数 + 服务端调度说明）+ 失败通知渠道。
 * 定时签到/续期由服务端内置调度器自动执行；到期日历统一收敛在「积分看板」页。
 */

/** 签到配置卡（F-55/F-16）：自动签到参数 + 服务端调度说明 */
function CheckinConfigCard({
  settings,
  patch,
}: {
  settings: WorkBuddySettings | null;
  patch: (p: Partial<WorkBuddySettings>) => void;
}) {
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
            <span className="block text-xs text-slate-400">服务启动时立即核验签到状态，未签到账号会自动补签</span>
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

        {/* 定时任务：服务端内置调度器自动执行，无需手动注册 */}
        <div className="rounded-lg border border-slate-100 p-3 text-xs text-slate-500 dark:border-zinc-800 dark:text-zinc-400">
          定时任务由服务端内置调度器自动执行：每日 09:10 自动签到，Token 续期 10:30 自动执行，无需手动配置。
        </div>
      </div>
    </div>
  );
}

export default function BuddySettings() {
  const pushToast = useAppStore((s) => s.pushToast);
  const [settings, setSettings] = useState<WorkBuddySettings | null>(null);
  const [saving, setSaving] = useState(false);
  const [refreshing, setRefreshing] = useState(false);

  const refresh = useCallback(async () => {
    setRefreshing(true);
    try {
      setSettings(await api.workbuddy.settingsGet().catch(() => null));
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
      await withMinDelay(api.workbuddy.settingsSet(settings), 800);
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

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="Buddy · 环境配置"
        desc="签到配置 · 失败通知"
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

      {/* 失败通知渠道（F-19） */}
      <div className="card p-4">
        <div className="mb-3 text-sm font-medium">失败通知渠道</div>
        <div className="space-y-3">
          <p className="text-xs text-slate-400">
            签到/补签失败等关键事件会同时推送到已配置的渠道（留空 = 关闭）。
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

      {/* 签到配置（F-55/F-16）：自动签到参数 + 服务端调度说明 */}
      <CheckinConfigCard settings={settings} patch={patch} />
    </div>
  );
}
