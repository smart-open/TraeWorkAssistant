import { useEffect, useRef, useState } from 'react';
import { Save, RotateCcw, Bell, Send, GripVertical } from 'lucide-react';
import { useAppStore } from '../store';
import { withMinDelay } from '../lib/delay';
import { api } from '../lib/tauri';
import { THEMES } from '../lib/themes';
import { APP_TABS } from './Sidebar';
import { ICON_CHOICES, resolveAppIcon, DEFAULT_APP_ICONS } from '../lib/appIcons';
import { getOrderedApps } from '../lib/appVisibility';
import { cn } from '../lib/cn';
import type { AppKey, Settings as SettingsType } from '../types';

/**
 * 通用设置面板：外观 / 语言 / 通用与通知 / 代理相关配置。
 * 供「系统设置」弹框（左下角系统图标）使用；环境配置页不包含这些区块。
 * 表单为本地状态，点击「保存」才持久化。
 */
export default function GeneralSettingsPanel() {
  const settings = useAppStore((s) => s.settings);
  const saveSettings = useAppStore((s) => s.saveSettings);
  const refreshSettings = useAppStore((s) => s.refreshSettings);
  const toast = useAppStore((s) => s.pushToast);

  const [form, setForm] = useState<SettingsType | null>(null);
  const [saving, setSaving] = useState(false);
  // 开机自启（T11）：注册表 Run 项即时生效，不随「保存设置」提交
  const [autostart, setAutostart] = useState(false);
  const [autostartBusy, setAutostartBusy] = useState(false);
  // 通知渠道「发送测试」执行态
  const [testing, setTesting] = useState(false);
  // 应用图标选择器：当前展开替换哪个应用的图标（null = 全部收起）
  const [iconPickerFor, setIconPickerFor] = useState<AppKey | null>(null);
  // 侧边栏显示拖拽排序态：dragKey = 正在拖拽的行（半透明反馈）；listRef = 列表容器
  // （pointermove 据此定位指针所在行）；「侧边栏显示」列表行序即侧边栏应用 Tab 展示顺序
  const [dragKey, setDragKey] = useState<AppKey | null>(null);
  const listRef = useRef<HTMLDivElement | null>(null);
  // 当前拖拽的监听器清理函数（onUp）：拖拽中组件被卸载（如 Esc 关闭设置弹框）时兜底移除
  const dragCleanupRef = useRef<(() => void) | null>(null);

  // 卸载兜底：window 级 mousemove/mouseup 监听器不随组件销毁自动移除，必须显式清理
  useEffect(() => () => dragCleanupRef.current?.(), []);

  const sendTest = async () => {
    setTesting(true);
    try {
      const msg = await api.misc.notifyTest();
      toast('success', msg);
    } catch (e) {
      toast('error', String(e));
    } finally {
      setTesting(false);
    }
  };

  useEffect(() => {
    void refreshSettings();
    api.misc
      .autostartStatus()
      .then(setAutostart)
      .catch(() => setAutostart(false));
  }, [refreshSettings]);

  const toggleAutostart = async () => {
    if (autostartBusy) return;
    setAutostartBusy(true);
    const next = !autostart;
    try {
      await api.misc.autostartSet(next);
      setAutostart(next);
      toast('success', next ? '已开启开机自启' : '已关闭开机自启');
    } catch (e) {
      toast('error', `设置开机自启失败：${String(e)}`);
    } finally {
      setAutostartBusy(false);
    }
  };

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

  // 拖拽落下重排：按「侧边栏显示」当前展示顺序（getOrderedApps）把 from 移到 to 的位置，
  // 全量写回 app_order（后端 retain 归一合法 key + 去重）；点底部「保存设置」后生效
  const reorderApps = (from: AppKey, to: AppKey) => {
    if (from === to) return;
    setForm((prev) => {
      if (!prev) return prev;
      const order = getOrderedApps(prev);
      const fromIdx = order.indexOf(from);
      const toIdx = order.indexOf(to);
      if (fromIdx < 0 || toIdx < 0) return prev;
      const next = [...order];
      next.splice(fromIdx, 1);
      next.splice(toIdx, 0, from);
      return { ...prev, app_order: next };
    });
  };

  // 抓手按下启动拖拽（C2：Pointer Events 统一鼠标/触控/触笔——鼠标事件在触屏
  // 上不触发）：window 级 pointermove 实时检测指针所在行并换位（松开即止）。
  // 不用 HTML5 DnD——Tauri Windows 端 dragDropEnabled 默认开启，原生文件拖放
  // 处理器会拦截网页内 dragstart，draggable 完全失效（实测无法拖动）
  const beginDrag = (key: AppKey) => (e: React.PointerEvent) => {
    let current = key;
    setDragKey(current);
    const onMove = (ev: PointerEvent) => {
      // 按键已全部抬起却没有收到 pointerup：焦点被抢走（alt-tab/系统弹窗）后
      // 松开按键的事件落在了别处。此时若继续处理移动会无按键持续换位、拖拽态
      // 卡死到下一次点击，且乱序会被「保存设置」持久化——立即结束拖拽。
      if (ev.buttons === 0) {
        onUp();
        return;
      }
      const rows = listRef.current?.querySelectorAll<HTMLElement>('[data-app-key]');
      if (!rows) return;
      rows.forEach((el) => {
        const r = el.getBoundingClientRect();
        if (ev.clientY >= r.top && ev.clientY < r.bottom) {
          const target = el.dataset.appKey as AppKey | undefined;
          if (target && target !== current) {
            reorderApps(current, target);
            current = target;
            setDragKey(target);
          }
        }
      });
    };
    const onUp = () => {
      setDragKey(null);
      window.removeEventListener('pointermove', onMove);
      window.removeEventListener('pointerup', onUp);
      window.removeEventListener('pointercancel', onUp);
      dragCleanupRef.current = null;
    };
    window.addEventListener('pointermove', onMove);
    window.addEventListener('pointerup', onUp);
    window.addEventListener('pointercancel', onUp);
    dragCleanupRef.current = onUp;
  };

  // C1：键盘替代拖拽——抓手聚焦后 ↑/↓ 与相邻行换位（纯拖拽对键盘用户不可达；
  // 与拖拽同走 reorderApps，保存语义一致）
  const moveByKeyboard = (key: AppKey) => (e: React.KeyboardEvent) => {
    if (e.key !== 'ArrowUp' && e.key !== 'ArrowDown') return;
    if (!form) return;
    const order = getOrderedApps(form);
    const i = order.indexOf(key);
    const j = e.key === 'ArrowUp' ? i - 1 : i + 1;
    if (i < 0 || j < 0 || j >= order.length) return;
    e.preventDefault();
    reorderApps(order[i], order[j]);
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
    if (!form || !settings) return;
    setSaving(true);
    try {
      // 审查修复（L35）：逐字段 diff，只把与已保存值不同的字段提交（saveSettings 即 patch 语义），不回传整份表单快照
      const patch: Partial<SettingsType> = {};
      (Object.keys(form) as (keyof SettingsType)[]).forEach((k) => {
        if (JSON.stringify(form[k] ?? null) !== JSON.stringify(settings[k] ?? null)) {
          (patch as Record<string, unknown>)[k] = form[k];
        }
      });
      await withMinDelay(saveSettings(patch));
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

      {/* 应用显示（侧边栏应用 Tab：固定应用单选 + 显示勾选 + 拖拽排序 + 行首图标点击换图标） */}
      <section className="card p-4">
        <h3 className="mb-1 font-medium">应用显示</h3>
        <p className="mb-3 text-xs text-slate-400">
          控制左下角侧边栏显示哪些应用。「固定应用」始终显示且不可隐藏（默认 Trae）；其余应用可自由勾选。
          「侧边栏显示」行可上下拖动调整展示顺序（保存后侧边栏应用 Tab 按新顺序排列）；
          隐藏仅收起入口，<b>不删除账号数据、不影响签到 / 保活等定时任务</b>，重新勾选即可恢复；
          若当前正在浏览的应用被隐藏，会自动跳回固定应用。按住行首抓手上下拖动调整顺序（触屏/触笔同样支持），
          或聚焦抓手后按 ↑/↓ 键与相邻行换位；图标可点击更换应用图标。
        </p>
        <div className="grid gap-x-8 gap-y-2 sm:grid-cols-2">
          <div className="space-y-2">
            <label className="label">固定应用（始终显示）</label>
            {/* buddy-ai（国际版）默认隐藏、不参与固定应用（Sidebar 同口径；后端 state.rs 脏值回落 trae） */}
            {APP_TABS.filter((t) => t.key !== 'buddy-ai').map((tab) => (
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
          <div className="space-y-1" ref={listRef}>
            <label className="label">侧边栏显示</label>
            {/* 行序即侧边栏应用 Tab 展示顺序：按住行首抓手上下拖动实时换位（getOrderedApps 渲染，未提及项默认顺序补尾） */}
            {getOrderedApps(form).map((appKey) => {
              const tab = APP_TABS.find((t) => t.key === appKey)!;
              const isBuddyAi = tab.key === 'buddy-ai';
              const pinned = form.pinned_app === tab.key;
              // buddy-ai 可见性走独立 opt-in 开关（默认 false = 不展示），不进 hidden_apps
              const visible = isBuddyAi ? form.buddy_ai_visible === true : pinned || !form.hidden_apps.includes(tab.key);
              const customizedName = form.app_icons[tab.key];
              const Current = resolveAppIcon(customizedName) ?? tab.icon;
              const dragging = dragKey === tab.key;
              return (
                <div
                  key={tab.key}
                  data-app-key={tab.key}
                  className={cn(
                    'flex items-center gap-1.5 rounded-md px-0.5 transition-opacity',
                    // 互斥：拖拽中优先显示拖拽反馈（cn 为纯 clsx，无 tailwind-merge，
                    // 同组 opacity 类共存时生效值取决于样式表顺序，不能依赖覆盖）
                    dragging ? 'opacity-40' : pinned && 'opacity-60',
                  )}
                >
                  {/* 拖拽抓手（button）：按住上下拖动实时换位，或聚焦后按 ↑/↓ 与相邻行换位
                      （C1 键盘替代 + C2 Pointer Events；select-none 承担原 preventDefault 防文字选择，
                       保留按钮聚焦能力供键盘操作） */}
                  <button
                    type="button"
                    title={`调整「${tab.label}」顺序：按住拖动，或聚焦后按 ↑/↓ 键`}
                    aria-label={`调整「${tab.label}」顺序：按住拖动，或按上下方向键`}
                    onPointerDown={beginDrag(tab.key)}
                    onKeyDown={moveByKeyboard(tab.key)}
                    className="flex h-7 w-7 shrink-0 cursor-grab touch-none select-none items-center justify-center rounded-md outline-none focus-visible:ring-2 focus-visible:ring-sky-500 active:cursor-grabbing"
                  >
                    <GripVertical
                      size={14}
                      className="text-slate-300 dark:text-zinc-600"
                      aria-hidden
                    />
                  </button>
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
                      onChange={() =>
                        isBuddyAi ? update('buddy_ai_visible', !(form.buddy_ai_visible === true)) : toggleAppVisible(tab.key)
                      }
                    />
                    {tab.label}
                    {pinned ? <span className="text-xs text-slate-400">（固定应用，不可隐藏）</span> : null}
                    {isBuddyAi ? <span className="text-xs text-slate-400">（国际版，默认不展示）</span> : null}
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
                <option value="system">系统通知</option>
                <option value="both">Toast + 系统通知</option>
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
                应用启动时自动清理超过保留天数的运行日志（代理 / 签到 / 切换日志）。
              </p>
            </div>
          </div>
          <div className="space-y-3">
            <label className="flex items-center gap-2">
              <input
                type="checkbox"
                checked={form.launch_minimized}
                onChange={(e) => update('launch_minimized', e.target.checked)}
              />
              启动时最小化到托盘
            </label>
            <label className="flex items-center gap-2">
              <input type="checkbox" checked={form.tray} onChange={(e) => update('tray', e.target.checked)} />
              启用系统托盘图标
            </label>
            <label className="flex items-center gap-2">
              <input
                type="checkbox"
                checked={autostart}
                onChange={() => void toggleAutostart()}
                disabled={autostartBusy}
              />
              开机自启
              <span className="text-xs text-slate-400">（开关即时生效，无需保存）</span>
            </label>
            <label className="flex items-center gap-2">
              <input
                type="checkbox"
                checked={form.silent_checkin}
                onChange={(e) => update('silent_checkin', e.target.checked)}
              />
              启动静默签到
              <span className="text-xs text-slate-400">（启动 60 秒后自动为未签到账号签到）</span>
            </label>
            <label className="flex items-center gap-2">
              <input
                type="checkbox"
                checked={form.auto_start_api}
                onChange={(e) => update('auto_start_api', e.target.checked)}
              />
              启动时自动开启 API 网关
              <span className="text-xs text-slate-400">（网关监听所有网卡，局域网设备可访问）</span>
            </label>
            <label className="flex items-center gap-2">
              <input
                type="checkbox"
                checked={form.sessions_preview_enabled}
                onChange={(e) => update('sessions_preview_enabled', e.target.checked)}
              />
              会话明细显示首条消息概述
              <span className="text-xs text-slate-400">
                （用量看板「会话说明」列；关闭仅隐藏展示，不删除缓存）
              </span>
            </label>
            <p className="text-xs text-slate-400">托盘与最小化设置变更后需重启应用生效。</p>
          </div>
        </div>
      </section>

      {/* 通知渠道（F-19，全部应用共用） */}
      <section className="card p-4">
        <div className="mb-1 flex items-center justify-between">
          <div className="flex items-center gap-2">
            <Bell size={16} className="text-amber-500" />
            <h3 className="font-medium">通知渠道</h3>
          </div>
          <button
            className="btn-outline !px-2.5 !py-1 text-xs"
            disabled={testing || dirty}
            title={dirty ? '表单有未保存的修改，请先点底部「保存设置」再测试' : '按已保存的配置向全部渠道发送测试消息'}
            onClick={() => void sendTest()}
          >
            <Send size={13} /> {testing ? '发送中…' : '发送测试'}
          </button>
        </div>
        <p className="mb-3 text-xs text-slate-400">
          签到完成 / 调度任务失败时推送到手机（Bark / Server酱 / PushPlus）或自建 Webhook，对全部应用（Trae、WorkBuddy、Qoder、豆包、Buddy 国际版）共用，
          未配置的渠道自动跳过；修改后点底部「保存设置」生效。
        </p>
        {/* 事件开关组：横排一条，与下方渠道输入视觉分离 */}
        <div className="mb-3 flex flex-wrap items-center gap-x-5 gap-y-2 rounded-lg border border-slate-200/70 bg-slate-50/60 px-3.5 py-2.5 text-sm dark:border-zinc-700/50 dark:bg-zinc-800/30">
          <label className="flex items-center gap-2">
            <input
              type="checkbox"
              checked={form.notify_enabled}
              onChange={(e) => update('notify_enabled', e.target.checked)}
            />
            启用通知推送（总开关）
          </label>
          <label className="flex items-center gap-2">
            <input
              type="checkbox"
              checked={form.notify_on_checkin}
              onChange={(e) => update('notify_on_checkin', e.target.checked)}
            />
            签到完成时通知
          </label>
          <label className="flex items-center gap-2">
            <input
              type="checkbox"
              checked={form.notify_on_task_fail}
              onChange={(e) => update('notify_on_task_fail', e.target.checked)}
            />
            调度任务失败时通知
          </label>
        </div>
        <div className="grid gap-x-4 gap-y-3 text-sm sm:grid-cols-2">
          <div>
            <label className="label">Bark 推送地址</label>
            <input
              type="text"
              value={form.notify_bark_url ?? ''}
              onChange={(e) => update('notify_bark_url', e.target.value.trim() || null)}
              placeholder="https://api.day.app/你的Key"
              className="input"
            />
          </div>
          <div>
            <label className="label">Server酱 SendKey</label>
            <input
              type="text"
              value={form.notify_serverchan_sendkey ?? ''}
              onChange={(e) => update('notify_serverchan_sendkey', e.target.value.trim() || null)}
              placeholder="SCT...（sct.ftqq.com 获取）"
              className="input"
            />
          </div>
          {/* PushPlus 子面板：三控件（Token / 模板 / 群组编码）收拢为一组，issue #157 */}
          <div className="rounded-lg border border-slate-200/70 bg-slate-50/60 p-3.5 dark:border-zinc-700/50 dark:bg-zinc-800/30 sm:col-span-2">
            <div className="mb-2.5 flex flex-wrap items-center justify-between gap-x-3 gap-y-1">
              <span className="text-xs font-medium text-slate-600 dark:text-zinc-300">PushPlus 微信推送</span>
              <a
                href="https://www.pushplus.plus/"
                target="_blank"
                rel="noreferrer"
                className="text-[11px] text-slate-400 transition-colors hover:text-zinc-500 dark:text-zinc-500 dark:hover:text-zinc-400"
              >
                pushplus.plus · 微信扫码登录获取 Token ↗
              </a>
            </div>
            <div className="space-y-3">
              <div>
                <label className="label">Token</label>
                <input
                  type="text"
                  value={form.notify_pushplus_token ?? ''}
                  onChange={(e) => update('notify_pushplus_token', e.target.value.trim() || null)}
                  placeholder="PushPlus Token（留空 = 关闭该渠道）"
                  className="input"
                />
              </div>
              <div className="grid gap-3 sm:grid-cols-2">
                <div>
                  <label className="label">推送模板</label>
                  <select
                    value={form.notify_pushplus_template ?? ''}
                    onChange={(e) => update('notify_pushplus_template', e.target.value || null)}
                    className="input"
                  >
                    <option value="">默认（txt，保留换行）</option>
                    <option value="txt">txt · 纯文本</option>
                    <option value="html">html · 网页</option>
                    <option value="markdown">markdown</option>
                  </select>
                </div>
                <div>
                  <label className="label">群组编码（可选）</label>
                  <input
                    type="text"
                    value={form.notify_pushplus_topic ?? ''}
                    onChange={(e) => update('notify_pushplus_topic', e.target.value.trim() || null)}
                    placeholder="一对多群发；留空 = 仅发给自己"
                    className="input"
                  />
                </div>
              </div>
            </div>
          </div>
          <div className="sm:col-span-2">
            <label className="label">通用 Webhook 地址</label>
            <input
              type="text"
              value={form.notify_webhook_url ?? ''}
              onChange={(e) => update('notify_webhook_url', e.target.value.trim() || null)}
              placeholder="https://...（POST JSON，兼容企业微信机器人等自建端）"
              className="input"
            />
          </div>
        </div>
      </section>

      {/* 代理相关 */}
      <section className="card p-4">
        <h3 className="mb-1 font-medium">代理</h3>
        <p className="mb-3 text-xs text-slate-400">本地 MITM 代理的端口与抓包行为，修改后需重启代理生效。</p>
        <div className="space-y-3 text-sm">
          <div className="grid gap-3 sm:grid-cols-2">
            <div>
              <label className="label">代理端口</label>
              <input
                type="number"
                value={form.proxy_port}
                onChange={(e) => update('proxy_port', Math.min(65535, Math.max(1, Number(e.target.value) || 8899)))}
                className="input w-32"
                min={1}
                max={65535}
              />
            </div>
            <label className="flex items-center gap-2 self-end pb-2.5">
              <input
                type="checkbox"
                checked={form.auto_start_proxy}
                onChange={(e) => update('auto_start_proxy', e.target.checked)}
              />
              启动时自动开启代理
            </label>
          </div>
          <div>
            <label className="label">代理监听域名列表</label>
            <textarea
              value={form.proxy_domains}
              onChange={(e) => update('proxy_domains', e.target.value)}
              className="input min-h-[60px] text-xs"
              placeholder="trae.cn,trae.com.cn,mchost.guru,zijieapi.com,bytedance.com,volcengine.com,volces.com,treecode.com,doubao.com"
            />
            <p className="mt-1 text-xs text-slate-400">
              逗号分隔的域名后缀列表，即 <b>解密白名单</b>（同 Charles SSL Proxying）：列表内域名走
              MITM 解密并记录日志；未列出的域名透明直通、不记录。注意：做证书锁定（cert pinning）的客户端
              （如豆包 ttnet 原生栈）对其 API 域解密会被拒，应只列入需要抓取凭证/流量的具体域名
              （如 www.doubao.com），不要配宽后缀。留空则使用默认值。
            </p>
          </div>
          <div>
            <label className="label">代理抓取日志路径</label>
            <input
              type="text"
              value={form.proxy_log_path ?? ''}
              onChange={(e) => update('proxy_log_path', e.target.value.trim() || null)}
              placeholder="留空则默认 %APPDATA%\AIWorkAssistant\logs"
              className="input"
            />
            <p className="mt-1 text-xs text-slate-400">
              代理拦截到的完整请求/响应将记录到此目录，按 100MB 滚动存储。
            </p>
          </div>
        </div>
      </section>

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
