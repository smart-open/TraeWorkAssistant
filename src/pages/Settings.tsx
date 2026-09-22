import { useEffect, useState } from 'react';
import { Save, RotateCcw, ClipboardCheck } from 'lucide-react';
import PageHeader from '../components/PageHeader';
import SchedulerTasksCard from '../components/SchedulerTasksCard';
import { useAppStore } from '../store';
import { withMinDelay } from '../lib/delay';
import type { Settings as SettingsType } from '../types';

/**
 * Trae · 环境配置：签到行为 + 定时任务（自动续期 / 自动签到 / 自动同步服务器数据）。
 * 全局配置（通知渠道 / IP 允许列表 / 管理员令牌）已迁至左下角系统图标的
 * 「系统设置 → 安全与管理」；外观 / 语言 / 通用在「系统设置」Tab。
 */
export default function Settings() {
  const settings = useAppStore((s) => s.settings);
  const saveSettings = useAppStore((s) => s.saveSettings);
  const refreshSettings = useAppStore((s) => s.refreshSettings);
  const toast = useAppStore((s) => s.pushToast);

  // 本地表单状态：用户编辑后点击「保存」才持久化，避免每次按键都写文件
  const [form, setForm] = useState<SettingsType | null>(null);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    void refreshSettings();
  }, [refreshSettings]);

  // settings 从后端加载完毕后同步到本地 form
  useEffect(() => {
    if (settings && !form) {
      setForm(settings);
    }
  }, [settings, form]);

  const dirty = form != null && settings != null && JSON.stringify(form) !== JSON.stringify(settings);

  const update = <K extends keyof SettingsType>(key: K, val: SettingsType[K]) => {
    setForm((prev) => (prev ? { ...prev, [key]: val } : prev));
  };

  const save = async () => {
    if (!form) return;
    setSaving(true);
    try {
      await withMinDelay(saveSettings(form));
      toast('success', '配置已保存');
    } catch {
      /* toast 已发出 */
    } finally {
      setSaving(false);
    }
  };

  const reset = () => {
    if (settings) setForm({ ...settings });
  };

  if (!form) {
    return (
      <div className="animate-fade-in">
        <PageHeader title="Trae · 环境配置" />
        <div className="text-sm text-slate-500">加载中…</div>
      </div>
    );
  }

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="Trae · 环境配置"
        desc="签到行为 · 定时任务"
        actions={
          dirty ? (
            <div className="flex items-center gap-2">
              <span className="text-xs text-amber-500">有未保存的更改</span>
              <button onClick={reset} className="btn-outline">
                <RotateCcw size={15} /> 撤销
              </button>
              <button onClick={save} disabled={saving} className="btn-primary">
                <Save size={15} /> {saving ? '保存中…' : '保存'}
              </button>
            </div>
          ) : (
            <button onClick={save} disabled={saving || !dirty} className="btn-outline opacity-50">
              <Save size={15} /> 已保存
            </button>
          )
        }
      />

      <div className="grid items-start gap-4 md:grid-cols-2">
        {/* 左列：签到行为 */}
        <section className="card p-4">
          <div className="mb-1 flex items-center gap-2">
            <ClipboardCheck size={16} className="text-brand-500" />
            <h3 className="font-medium">签到行为</h3>
          </div>
          <p className="mb-3 text-xs text-slate-400">批量签到时的默认跳过策略与重试参数，对所有签到入口生效。</p>
          <div className="space-y-3 text-sm">
            <label className="flex items-start gap-2">
              <input
                type="checkbox"
                className="mt-1"
                checked={form.checkin_skip_checked}
                onChange={(e) => update('checkin_skip_checked', e.target.checked)}
              />
              <span>
                默认跳过今日已签账号
                <span className="block text-xs text-slate-400">推荐开启：已签账号自动跳过，不重复请求</span>
              </span>
            </label>
            <label className="flex items-start gap-2">
              <input
                type="checkbox"
                className="mt-1"
                checked={form.checkin_skip_expired}
                onChange={(e) => update('checkin_skip_expired', e.target.checked)}
              />
              <span>
                默认跳过 JWT 过期账号
                <span className="block text-xs text-slate-400">推荐开启：过期账号先续期再签到，避免无效请求</span>
              </span>
            </label>
            <div className="rounded-lg bg-slate-50 px-3 py-2.5 dark:bg-zinc-900">
              <label className="label">失败重试次数</label>
              <div className="flex items-center gap-3">
                <input
                  type="number"
                  value={form.retry}
                  onChange={(e) => update('retry', Math.min(5, Math.max(0, Number(e.target.value) || 0)))}
                  className="input w-24"
                  min={0}
                  max={5}
                />
                <span className="text-xs text-slate-400">推荐 1 次：签到失败后自动重试（0–5）</span>
              </div>
            </div>
          </div>
        </section>

        {/* 右列：定时任务（开关即时保存，含最近执行状态；推荐配置 = 全部启用） */}
        <SchedulerTasksCard
          taskKeys={['trae-jwt-renew', 'models-sync', 'trae-checkin', 'trae-credits-snapshot']}
          desc="服务端内置调度器每日自动执行，覆盖自动续期 / 自动签到 / 自动同步服务器数据（模型列表 + 积分看板）。推荐保持全部启用。"
        />
      </div>
    </div>
  );
}
