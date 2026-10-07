import { useEffect, useState } from 'react';
import { Save, RotateCcw, BellRing, Send } from 'lucide-react';
import { useAppStore } from '../store';
import { withMinDelay } from '../lib/delay';
import { THEMES } from '../lib/themes';
import { api } from '../lib/tauri';
import { APP_TABS } from './Sidebar';
import { ICON_CHOICES, resolveAppIcon, DEFAULT_APP_ICONS } from '../lib/appIcons';
import type {
  Settings as SettingsType,
  NotifyConfig as NotifyConfigType,
  NotifyResult,
  AppKey,
} from '../types';

/**
 * 通用设置面板：外观 / 语言 / 通用与通知 / 通知渠道。
 * 供「系统设置」弹框（左下角系统图标）使用；环境配置页不包含这些区块。
 * 表单为本地状态（含通知渠道），统一由底部「保存设置」持久化（通知渠道为独立 kv）。
 */
export default function GeneralSettingsPanel() {
  const settings = useAppStore((s) => s.settings);
  const saveSettings = useAppStore((s) => s.saveSettings);
  const refreshSettings = useAppStore((s) => s.refreshSettings);
  const toast = useAppStore((s) => s.pushToast);

  const [form, setForm] = useState<SettingsType | null>(null);
  const [saving, setSaving] = useState(false);
  // 应用图标选择器：当前展开替换哪个应用的图标（null = 全部收起）
  const [iconPickerFor, setIconPickerFor] = useState<AppKey | null>(null);

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

  // 切换固定应用：新固定项强制从隐藏列表移除（始终可见、不可取消）
  const setPinned = (key: AppKey) => {
    setForm((prev) =>
      prev ? { ...prev, pinned_app: key, hidden_apps: prev.hidden_apps.filter((a) => a !== key) } : prev,
    );
  };

  // 勾选 = 显示（移出隐藏列表）；取消勾选 = 隐藏（加入隐藏列表）
  const toggleAppVisible = (key: AppKey) => {
    setForm((prev) =>
      prev
        ? {
            ...prev,
            hidden_apps: prev.hidden_apps.includes(key)
              ? prev.hidden_apps.filter((a) => a !== key)
              : [...prev.hidden_apps, key],
          }
        : prev,
    );
  };

  // 设置应用图标：null 或与内置默认同名 = 删除自定义项（回退 APP_TABS 默认图标）
  const setAppIcon = (key: AppKey, icon: string | null) => {
    setForm((prev) => {
      if (!prev) return prev;
      const appIcons = { ...prev.app_icons };
      if (!icon || icon === DEFAULT_APP_ICONS[key]) delete appIcons[key];
      else appIcons[key] = icon;
      return { ...prev, app_icons: appIcons };
    });
  };

  const save = async () => {
    if (!form) return;
    setSaving(true);
    try {
      await withMinDelay(saveSettings(form));
      // 通知渠道（独立 kv）随底部保存统一持久化；设置成功而通知失败时单独报错
      let notifyErr: string | null = null;
      if (notifyForm && notifyDirty) {
        try {
          const saved = await withMinDelay(api.notify.setConfig(notifyForm));
          setNotifyForm(saved);
          setNotifyLoaded(saved);
        } catch (e) {
          notifyErr = e instanceof Error ? e.message : '保存失败';
        }
      }
      if (notifyErr) toast('error', `设置已保存，但通知渠道保存失败：${notifyErr}`);
      else toast('success', '设置已保存');
    } catch {
      /* toast 已发出 */
    } finally {
      setSaving(false);
    }
  };

  const reset = () => {
    if (settings) setForm({ ...settings });
    if (notifyLoaded) setNotifyForm(notifyLoaded);
  };

  // ---- 通知渠道（Trae / Buddy 全平台共用；独立 kv，随底部「保存设置」统一持久化）----
  const [notifyForm, setNotifyForm] = useState<NotifyConfigType | null>(null);
  const [notifyLoaded, setNotifyLoaded] = useState<NotifyConfigType | null>(null);
  const [notifyTesting, setNotifyTesting] = useState(false);
  const notifyDirty =
    notifyForm != null && notifyLoaded != null && JSON.stringify(notifyForm) !== JSON.stringify(notifyLoaded);

  useEffect(() => {
    api.notify
      .getConfig()
      .then((c) => {
        setNotifyForm(c);
        setNotifyLoaded(c);
      })
      .catch(() => {
        setNotifyForm(null);
        setNotifyLoaded(null);
      });
  }, []);

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

      {/* 应用显示（侧边栏应用 Tab：固定应用单选 + 显示勾选 + 行首图标点击换图标） */}
      <section className="card p-4">
        <h3 className="mb-1 font-medium">应用显示</h3>
        <p className="mb-3 text-xs text-slate-400">
          控制左下角侧边栏显示哪些应用。「固定应用」始终显示且不可隐藏（默认 Trae）；其余应用可自由勾选。
          隐藏仅收起入口，<b>不删除账号数据、不影响签到 / 保活等定时任务</b>，重新勾选即可恢复；
          若当前正在浏览的应用被隐藏，会自动跳回固定应用。「侧边栏显示」行首的图标可点击更换应用图标。
        </p>
        <div className="grid gap-x-8 gap-y-2 sm:grid-cols-2">
          <div className="space-y-2">
            <label className="label">固定应用（始终显示）</label>
            {APP_TABS.map((tab) => (
              <label key={tab.key} className="flex items-center gap-2">
                <input
                  type="radio"
                  name="pinned-app"
                  checked={form.pinned_app === tab.key}
                  onChange={() => setPinned(tab.key)}
                />
                {tab.label}
                {tab.title ? <span className="text-xs text-slate-400">（{tab.title}）</span> : null}
              </label>
            ))}
          </div>
          <div className="space-y-2">
            <label className="label">侧边栏显示</label>
            {APP_TABS.map((tab) => {
              const pinned = form.pinned_app === tab.key;
              const visible = pinned || !form.hidden_apps.includes(tab.key);
              const customizedName = form.app_icons[tab.key];
              const Current = resolveAppIcon(customizedName) ?? tab.icon;
              return (
                <div key={tab.key} className={`flex items-center gap-2 ${pinned ? 'opacity-60' : ''}`}>
                  {/* 行首图标：点击弹出候选表更换该应用在侧边栏的显示图标（仅 UI 偏好） */}
                  <button
                    type="button"
                    title={`更换「${tab.label}」图标${customizedName ? `（当前：${customizedName}）` : '（当前：默认）'}`}
                    onClick={() => setIconPickerFor(tab.key)}
                    className="flex h-7 w-7 shrink-0 items-center justify-center rounded-md border border-slate-200 text-slate-600 transition hover:border-zinc-400 hover:text-zinc-800 active:scale-95 dark:border-zinc-700 dark:text-zinc-300 dark:hover:border-zinc-500 dark:hover:text-zinc-100"
                  >
                    <Current size={15} />
                  </button>
                  <label className="flex items-center gap-2">
                    <input
                      type="checkbox"
                      checked={visible}
                      disabled={pinned}
                      onChange={() => toggleAppVisible(tab.key)}
                    />
                    {tab.label}
                    {pinned ? <span className="text-xs text-slate-400">（固定应用，不可隐藏）</span> : null}
                  </label>
                </div>
              );
            })}
          </div>
        </div>
      </section>

      {/* 图标选择弹框：点「侧边栏显示」行首图标弹出，在候选表内选择；选后需点底部「保存设置」生效 */}
      {iconPickerFor
        ? (() => {
            const tab = APP_TABS.find((t) => t.key === iconPickerFor)!;
            const customizedName = form.app_icons[tab.key];
            const selectedName = customizedName ?? DEFAULT_APP_ICONS[tab.key];
            return (
              <div
                className="fixed inset-0 z-[60] flex items-center justify-center p-4"
                onClick={() => setIconPickerFor(null)}
              >
                <div className="absolute inset-0 bg-black/40 backdrop-blur-sm" />
                <div
                  className="card animate-fade-in relative z-10 w-full max-w-sm p-4 shadow-xl"
                  role="dialog"
                  aria-modal="true"
                  onClick={(e) => e.stopPropagation()}
                >
                  <h3 className="mb-1 text-sm font-semibold text-slate-800 dark:text-zinc-100">
                    更换「{tab.label}」图标
                  </h3>
                  <p className="mb-3 text-xs text-slate-400">
                    选择侧边栏显示图标，点底部「保存设置」生效；非法/缺失自动回退内置默认（{DEFAULT_APP_ICONS[tab.key]}）。
                  </p>
                  <div className="grid grid-cols-8 gap-1">
                    {ICON_CHOICES.map((c) => {
                      const Choice = c.icon;
                      const selected = selectedName === c.name;
                      return (
                        <button
                          key={c.name}
                          type="button"
                          title={c.name}
                          onClick={() => {
                            setAppIcon(tab.key, c.name);
                            setIconPickerFor(null);
                          }}
                          className={`flex h-8 items-center justify-center rounded-md transition ${
                            selected
                              ? 'bg-zinc-900 text-white dark:bg-zinc-100 dark:text-zinc-900'
                              : 'text-slate-600 hover:bg-slate-200 dark:text-zinc-400 dark:hover:bg-zinc-800'
                          }`}
                        >
                          <Choice size={16} />
                        </button>
                      );
                    })}
                  </div>
                  <div className="mt-3 flex items-center justify-between">
                    {customizedName ? (
                      <button
                        className="btn-outline !px-2.5 !py-1 text-xs"
                        onClick={() => {
                          setAppIcon(tab.key, null);
                          setIconPickerFor(null);
                        }}
                      >
                        <RotateCcw size={12} /> 恢复默认
                      </button>
                    ) : (
                      <span className="text-xs text-slate-400">当前使用默认图标</span>
                    )}
                    <button className="btn-outline !px-2.5 !py-1 text-xs" onClick={() => setIconPickerFor(null)}>
                      取消
                    </button>
                  </div>
                </div>
              </div>
            );
          })()
        : null}

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

      {/* 通知渠道（Trae / Buddy 全平台共用：Bark / Server酱 / Webhook；随底部「保存设置」统一保存） */}
      {notifyForm && (
        <section className="card p-4">
          <div className="mb-1 flex flex-wrap items-center justify-between gap-2">
            <div className="flex items-center gap-2">
              <BellRing size={16} className="text-brand-500" />
              <h3 className="font-medium">通知渠道</h3>
            </div>
            <button onClick={testNotify} disabled={notifyTesting} className="btn-outline">
              <Send size={15} /> {notifyTesting ? '发送中…' : '发送测试'}
            </button>
          </div>
          <p className="mb-3 text-xs text-slate-400">
            签到完成 / 调度任务失败时推送到手机（Bark / Server酱）或自建 Webhook，Trae 与 Buddy 全平台共用，未配置的渠道自动跳过；修改后点底部「保存设置」生效。
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

      {/* 保存条（外观/通用/通知渠道统一保存） */}
      <div className="flex items-center justify-end gap-2 pb-1">
        {(dirty || notifyDirty) && <span className="mr-auto text-xs text-amber-500">有未保存的更改</span>}
        <button onClick={reset} disabled={!dirty && !notifyDirty} className="btn-outline disabled:opacity-40">
          <RotateCcw size={15} /> 撤销
        </button>
        <button onClick={save} disabled={saving || (!dirty && !notifyDirty)} className="btn-outline disabled:opacity-40">
          <Save size={15} /> {saving ? '保存中…' : '保存设置'}
        </button>
      </div>
    </div>
  );
}
