import { useCallback, useEffect, useState } from 'react';
import { Save, RefreshCw, ShieldAlert, SlidersHorizontal } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import SchedulerTasksCard, { type TaskToggleOverride } from '../../components/SchedulerTasksCard';
import { Spinner } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { withMinDelay } from '../../lib/delay';
import type { QoderSettings } from '../../types';

/**
 * qoder-settings 环境配置（F-80 §5.8，布局对齐 BuddySettings，docker 版裁剪）：
 * 左列 通用配置（自动签到开关）；右列 定时任务（SchedulerTasksCard 统一配置
 * qoder-checkin / qoder-refresh / qoder-credits-snapshot / qoder-catalog-sync）。
 * qoder-checkin 开关绑定 QoderSettings.auto_checkin（单源：同时门控定时签到与启动补签）；
 * 桌面版的客户端路径检测 / Windows 计划任务双轨为 OS 专属，docker 版不含。
 */
export default function QoderSettings() {
  const pushToast = useAppStore((s) => s.pushToast);
  const [settings, setSettings] = useState<QoderSettings | null>(null);
  const [saving, setSaving] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  // qoder-checkin 开关（auto_checkin）即时保存 pending
  const [checkinToggleBusy, setCheckinToggleBusy] = useState(false);
  // 多账号签到间隔（qoder_checkin_gap_secs 存 app Settings，随「保存配置」统一提交）
  const [checkinGap, setCheckinGap] = useState(3);

  const refresh = useCallback(async () => {
    setRefreshing(true);
    try {
      setSettings(await api.qoder.settingsGet().catch(() => null));
      const app = await api.misc.settingsGet().catch(() => null);
      setCheckinGap(app?.qoder_checkin_gap_secs ?? 3);
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
    if (!settings) {
      // 配置未加载时静默返回会让用户误以为保存成功（对齐 BuddyApiService 守卫模式）
      pushToast('warn', '配置尚未加载，无法保存（请刷新重试）');
      return;
    }
    setSaving(true);
    try {
      await withMinDelay(
        Promise.all([
          api.qoder.settingsSet(settings),
          api.misc.settingsSet({
            ...(await api.misc.settingsGet()),
            qoder_checkin_gap_secs: checkinGap,
          }),
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

  // qoder-checkin 开关：绑定 auto_checkin 并即时保存（与定时任务卡其他开关交互一致）
  const toggleCheckin = async (v: boolean) => {
    if (!settings) return;
    const prev = settings;
    const next = { ...prev, auto_checkin: v };
    setSettings(next);
    setCheckinToggleBusy(true);
    try {
      await withMinDelay(api.qoder.settingsSet(next));
      pushToast('success', v ? '自动签到已启用' : '自动签到已停用');
    } catch (err) {
      setSettings(prev);
      pushToast('error', `保存失败：${String(err)}`);
    } finally {
      setCheckinToggleBusy(false);
    }
  };

  const overrides: Record<string, TaskToggleOverride> | undefined = settings
    ? {
        'qoder-checkin': { checked: settings.auto_checkin, onToggle: toggleCheckin, busy: checkinToggleBusy },
      }
    : undefined;

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="Qoder · 环境配置"
        desc="通用配置 · 定时任务（服务端内置调度器自动执行）"
        actions={
          <>
            <button className="btn-outline" disabled={refreshing} onClick={() => void refresh()}>
              <RefreshCw size={15} className={refreshing ? 'animate-spin' : ''} /> 刷新
            </button>
            <button className="btn-outline" disabled={saving || !settings} onClick={() => void save()}>
              {saving ? <Spinner /> : <Save size={15} />} 保存配置
            </button>
          </>
        }
      />

      {/* 合规提示（固定展示） */}
      <div className="mb-4 flex items-start gap-2 rounded-lg border border-amber-200 bg-amber-50 p-3 text-xs text-amber-700 dark:border-amber-500/30 dark:bg-amber-500/10 dark:text-amber-300">
        <ShieldAlert size={15} className="mt-0.5 shrink-0" />
        <span>
          合规提示：平台条款对「同一设备 / 手机号 / 支付宝账号」多维去重并限制技术手段自动化参与。
          本功能定位为辅助个人账号的日常领取：多账号并发签到、
          设备指纹按「每账号稳定绑定」注入（入池生成一次永不轮换，真实捕获值优先透传，
          不做随机轮换）、请求间隔抖动、失败退避。请自行评估并承担条款风险。
        </span>
      </div>

      <div className="grid items-start gap-4 lg:grid-cols-2">
        {/* 左列：通用配置（自动签到开关；桌面版的应用环境路径为 OS 专属，docker 版不含） */}
        <section className="card p-4">
          <div className="mb-1 flex items-center gap-2">
            <SlidersHorizontal size={16} className="text-violet-500" />
            <span className="font-medium">通用配置</span>
          </div>
          <p className="mb-3 text-xs text-slate-400">
            签到行为开关，改动后点击「保存配置」生效；qoder-checkin 的开关在右侧定时任务卡内即时保存。
          </p>
          <div className="space-y-3">
            <label
              className={`flex items-start gap-2 rounded-lg border p-3 ${
                settings
                  ? 'border-slate-100 dark:border-zinc-800'
                  : 'border-amber-200 bg-amber-50/50 opacity-70 dark:border-amber-500/30 dark:bg-amber-500/5'
              }`}
            >
              <input
                type="checkbox"
                className="mt-0.5"
                disabled={!settings}
                checked={settings?.auto_checkin ?? true}
                onChange={(e) =>
                  setSettings((s) => (s ? { ...s, auto_checkin: e.target.checked } : s))
                }
              />
              <span className="text-sm">
                自动签到（默认开）
                <span className="block text-xs text-slate-400">
                  应用内调度器 qoder-checkin 启用判定（兼启动补签）；多账号并发签到，
                  每账号绑定一份稳定设备指纹（账号页可查看），不同账号以不同设备身份请求，互不影响
                </span>
                {!settings && (
                  <span className="mt-1 block text-xs text-amber-600 dark:text-amber-400">
                    qoder-settings 加载失败，当前显示为默认值且不可修改；请点右上角「刷新」重试
                  </span>
                )}
              </span>
            </label>

            {/* 多账号签到间隔（qoder_checkin_gap_secs 存 app Settings，随「保存配置」统一提交） */}
            <div className="rounded-lg bg-slate-50 px-3 py-2.5 dark:bg-zinc-900">
              <label className="label">账号间隔（秒）</label>
              <div className="flex items-center gap-3">
                <input
                  type="number"
                  min={0}
                  max={600}
                  className="input w-20"
                  value={checkinGap}
                  onChange={(e) => {
                    if (e.target.value === '') return; // 清空输入中间态不落值
                    const n = Math.min(600, Math.max(0, Math.floor(Number(e.target.value))));
                    setCheckinGap(Number.isFinite(n) ? n : 3);
                  }}
                />
                <span className="text-xs text-slate-400">
                  多账号串行签到的间隔，默认 3 秒防频控，0 = 关闭
                </span>
              </div>
            </div>
          </div>
        </section>

        {/* 右列：定时任务（开关与执行时刻即时保存，含最近执行状态；推荐配置 = 全部启用 + 默认时刻） */}
        <SchedulerTasksCard
          taskKeys={['qoder-checkin', 'qoder-refresh', 'qoder-credits-snapshot', 'qoder-catalog-sync']}
          overrides={overrides}
          desc="服务端内置调度器自动执行，覆盖每日签到 / 凭证定时刷新 / 积分快照 / 模型目录同步。推荐保持全部启用。"
        />
      </div>
    </div>
  );
}
