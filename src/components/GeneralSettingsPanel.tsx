import { useEffect, useState } from 'react';
import { Save, RotateCcw, BellRing, Send } from 'lucide-react';
import { useAppStore } from '../store';
import { withMinDelay } from '../lib/delay';
import { THEMES } from '../lib/themes';
import { api } from '../lib/tauri';
import type { Settings as SettingsType, NotifyConfig as NotifyConfigType, NotifyResult } from '../types';

/**
 * 通用设置面板：外观 / 语言 / 通用与通知 / 通知渠道。
 * 供「系统设置」弹框（左下角系统图标）使用；环境配置页不包含这些区块。
 * 表单为本地状态，点击「保存」才持久化；通知渠道为独立 kv，卡片内即时保存。
 */
export default function GeneralSettingsPanel() {
  const settings = useAppStore((s) => s.settings);
  const saveSettings = useAppStore((s) => s.saveSettings);
  const refreshSettings = useAppStore((s) => s.refreshSettings);
  const toast = useAppStore((s) => s.pushToast);

  const [form, setForm] = useState<SettingsType | null>(null);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    void refreshSettings();
  }, [refreshSettings]);

  useEffect(() => {
    if (settings && !form) {
      // 兼容旧主题值：light→graphite、dark→charcoal
      const theme = settings.theme === 'light' ? 'graphite' : settings.theme === 'dark' ? 'charcoal' : settings.theme;
      setForm({ ...settings, theme });
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
      toast('success', '设置已保存');
    } catch {
      /* toast 已发出 */
    } finally {
      setSaving(false);
    }
  };

  const reset = () => {
    if (settings) setForm({ ...settings });
  };

  // ---- 通知渠道（Trae / Buddy 全平台共用；独立 kv，卡片内保存）----
  const [notifyForm, setNotifyForm] = useState<NotifyConfigType | null>(null);
  const [notifySaving, setNotifySaving] = useState(false);
  const [notifyTesting, setNotifyTesting] = useState(false);

  useEffect(() => {
    api.notify
      .getConfig()
      .then(setNotifyForm)
      .catch(() => setNotifyForm(null));
  }, []);

  const saveNotify = async () => {
    if (!notifyForm) return;
    setNotifySaving(true);
    try {
      const saved = await withMinDelay(api.notify.setConfig(notifyForm));
      setNotifyForm(saved);
      toast('success', '通知配置已保存');
    } catch (e) {
      toast('error', e instanceof Error ? e.message : '通知配置保存失败');
    } finally {
      setNotifySaving(false);
    }
  };

  const testNotify = async () => {
    if (!notifyForm) return;
    setNotifyTesting(true);
    try {
      const res: NotifyResult = await withMinDelay(api.notify.test(notifyForm));
      if (res.sent) {
        toast('success', '测试通知已发送，请查收');
      } else {
        // 汇总各渠道失败原因；全部 null 视为未配置
        const fails = [res.bark, res.serverchan, res.webhook].filter(
          (x): x is string => !!x && x !== 'ok',
        );
        toast('error', fails.length ? `发送失败：${fails.join('；')}` : res.reason ?? '未配置任何通知渠道');
      }
    } catch (e) {
      toast('error', e instanceof Error ? e.message : '测试发送失败');
    } finally {
      setNotifyTesting(false);
    }
  };

  if (!form) {
    return <div className="py-6 text-center text-sm text-slate-400">加载中…</div>;
  }

  return (
    <div className="flex flex-col gap-4 text-sm">
      {/* 外观 */}
      <section className="card p-4">
        <h3 className="mb-3 font-medium">外观</h3>
        <div className="grid gap-3 sm:grid-cols-2">
          <div>
            <label className="label">主题</label>
            <select value={form.theme} onChange={(e) => update('theme', e.target.value)} className="input">
              <option value="system">跟随系统</option>
              {THEMES.map((t) => (
                <option key={t.id} value={t.id}>
                  {t.name}
                </option>
              ))}
            </select>
          </div>
          <div>
            <label className="label">语言</label>
            <select value={form.language} onChange={(e) => update('language', e.target.value)} className="input">
              <option value="zh-CN">简体中文</option>
              <option value="en-US">English</option>
            </select>
          </div>
        </div>
      </section>

      {/* 通用与通知（两列布局：左选项/右开关组） */}
      <section className="card p-4">
        <h3 className="mb-3 font-medium">通用与通知</h3>
        <div className="grid gap-x-8 gap-y-3 text-sm sm:grid-cols-2">
          <div className="space-y-3">
            <div>
              <label className="label">通知方式</label>
              <select value={form.notify} onChange={(e) => update('notify', e.target.value)} className="input">
                <option value="toast">应用内 Toast</option>
                <option value="none">不通知</option>
              </select>
            </div>
            <div>
              <label className="label">日志保留天数</label>
              <input
                type="number"
                value={form.log_retention_days}
                onChange={(e) => update('log_retention_days', Math.min(365, Math.max(1, Number(e.target.value) || 30)))}
                className="input w-24"
                min={1}
                max={365}
              />
              <p className="mt-1 text-xs text-slate-400">
                应用启动时自动清理超过保留天数的运行日志（签到 / 网关日志）。
              </p>
            </div>
          </div>
        </div>
      </section>

      {/* 通知渠道（Trae / Buddy 全平台共用：Bark / Server酱 / Webhook；独立 kv 即时保存） */}
      {notifyForm && (
        <section className="card p-4">
          <div className="mb-1 flex flex-wrap items-center justify-between gap-2">
            <div className="flex items-center gap-2">
              <BellRing size={16} className="text-brand-500" />
              <h3 className="font-medium">通知渠道</h3>
            </div>
            <div className="flex items-center gap-2">
              <button onClick={testNotify} disabled={notifyTesting} className="btn-outline">
                <Send size={15} /> {notifyTesting ? '发送中…' : '发送测试'}
              </button>
              <button onClick={saveNotify} disabled={notifySaving} className="btn-primary">
                <Save size={15} /> {notifySaving ? '保存中…' : '保存'}
              </button>
            </div>
          </div>
          <p className="mb-3 text-xs text-slate-400">
            签到完成 / 调度任务失败时推送到手机（Bark / Server酱）或自建 Webhook，Trae 与 Buddy 全平台共用，未配置的渠道自动跳过。
          </p>
          <div className="grid gap-4 text-sm md:grid-cols-2">
            <div className="space-y-3">
              <label className="flex items-center gap-2">
                <input
                  type="checkbox"
                  checked={notifyForm.enabled}
                  onChange={(e) => setNotifyForm({ ...notifyForm, enabled: e.target.checked })}
                />
                启用通知推送（总开关）
              </label>
              <label className="flex items-center gap-2">
                <input
                  type="checkbox"
                  checked={notifyForm.on_checkin_done}
                  onChange={(e) => setNotifyForm({ ...notifyForm, on_checkin_done: e.target.checked })}
                />
                签到完成时通知
              </label>
              <label className="flex items-center gap-2">
                <input
                  type="checkbox"
                  checked={notifyForm.on_task_failed}
                  onChange={(e) => setNotifyForm({ ...notifyForm, on_task_failed: e.target.checked })}
                />
                调度任务失败时通知
              </label>
            </div>
            <div className="space-y-3">
              <div>
                <label className="label">Bark 推送地址</label>
                <input
                  value={notifyForm.bark_url ?? ''}
                  onChange={(e) => setNotifyForm({ ...notifyForm, bark_url: e.target.value || null })}
                  placeholder="https://api.day.app/你的Key"
                  className="input w-full"
                />
              </div>
              <div>
                <label className="label">Server酱 SendKey</label>
                <input
                  value={notifyForm.serverchan_sendkey ?? ''}
                  onChange={(e) =>
                    setNotifyForm({ ...notifyForm, serverchan_sendkey: e.target.value || null })
                  }
                  placeholder="SCT…（sct.ftqq.com 获取）"
                  className="input w-full"
                />
              </div>
              <div>
                <label className="label">通用 Webhook 地址</label>
                <input
                  value={notifyForm.webhook_url ?? ''}
                  onChange={(e) => setNotifyForm({ ...notifyForm, webhook_url: e.target.value || null })}
                  placeholder="https://…（POST JSON，兼容企业微信机器人等自建端）"
                  className="input w-full"
                />
              </div>
            </div>
          </div>
        </section>
      )}

      {/* 保存条 */}
      <div className="flex items-center justify-end gap-2 pb-1">
        {dirty && <span className="mr-auto text-xs text-amber-500">有未保存的更改</span>}
        <button onClick={reset} disabled={!dirty} className="btn-outline disabled:opacity-40">
          <RotateCcw size={15} /> 撤销
        </button>
        <button onClick={save} disabled={saving || !dirty} className="btn-outline disabled:opacity-40">
          <Save size={15} /> {saving ? '保存中…' : '保存设置'}
        </button>
      </div>
    </div>
  );
}
