import { useCallback, useEffect, useRef, useState } from 'react';
import { Save, RotateCcw, BellRing, Send, GripVertical, RefreshCw } from 'lucide-react';
import { useAppStore } from '../store';
import { withMinDelay } from '../lib/delay';
import { THEMES } from '../lib/themes';
import { api } from '../lib/tauri';
import { APP_TABS } from './Sidebar';
import { ICON_CHOICES, resolveAppIcon, DEFAULT_APP_ICONS } from '../lib/appIcons';
import { getOrderedApps } from '../lib/appVisibility';
import { cn } from '../lib/cn';
import type {
  Settings as SettingsType,
  NotifyConfig as NotifyConfigType,
  NotifyResult,
  AppKey,
} from '../types';

/** 通知渠道 kv key → 面板展示名（最近推送回显用，issue #222） */
const NOTIFY_CHANNEL_LABELS: Record<string, string> = {
  bark: 'Bark',
  // 键名对齐后端 notify.rs record_channel_result 写入的 webhook 键（原 wechat
  // 键恒 miss，Webhook 推送记录被误过滤）；后端无 pushplus 写入，仅兜底展示
  webhook: 'Webhook',
  serverchan: 'Server酱',
  pushplus: 'PushPlus',
};

/** 兼容旧主题值：light→graphite、dark→charcoal（挂载归一与保存比较共用同一映射） */
function normalizeLegacyTheme(theme: SettingsType['theme']): SettingsType['theme'] {
  return theme === 'light' ? 'graphite' : theme === 'dark' ? 'charcoal' : theme;
}

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
  // 侧边栏显示拖拽排序态：dragKey = 正在拖拽的行（半透明反馈）；listRef = 列表容器
  // （pointermove 据此定位指针所在行）；「侧边栏显示」列表行序即侧边栏应用 Tab 展示顺序
  const [dragKey, setDragKey] = useState<AppKey | null>(null);
  const listRef = useRef<HTMLDivElement | null>(null);
  // 当前拖拽的监听器清理函数（onUp）：拖拽中组件被卸载（如 Esc 关闭设置弹框）时兜底移除
  const dragCleanupRef = useRef<(() => void) | null>(null);

  // 卸载兜底：window 级 pointermove/pointerup 监听器不随组件销毁自动移除，必须显式清理
  useEffect(() => () => dragCleanupRef.current?.(), []);

  // 渠道最近一次推送结果（issue #222）：打开面板时加载，测试后刷新回显。
  // docker 适配：无 Tauri 事件总线（main 经 notify-result-updated 事件实时刷新），
  // 面板内真实推送不发生；但调度器定时推送可能发生在面板停留期间，加 30s 轮询
  // 拉取兜底回显（卸载时清除定时器）
  const [lastNotifyResult, setLastNotifyResult] = useState<
    Record<string, { ts: number; ok: boolean; err?: string; src?: string }>
  >({});
  const loadLastNotifyResult = useCallback(() => {
    api.notify
      .notifyLastResult()
      .then(setLastNotifyResult)
      .catch(() => {});
  }, []);
  useEffect(() => {
    loadLastNotifyResult();
    const timer = setInterval(loadLastNotifyResult, 30_000);
    return () => clearInterval(timer);
  }, [loadLastNotifyResult]);

  useEffect(() => {
    void refreshSettings();
  }, [refreshSettings]);

  // 审查修复（第五轮）：form 是挂载时的单向快照——面板停留期间 settings 被他端
  // 更新（另一浏览器 Tab / 系统弹框内其他面板保存 → refreshSettings 整体替换）时
  // form 停留旧值，打开即误报「有未保存的更改」，且此时保存会把旧表单值逐字段
  // 回写覆盖服务端新配置。以「上次同步来源快照」判断用户是否编辑过：form 仍等于
  // 来源快照（未编辑）或来源未知（保存完成后）→ 跟随最新 settings 重置；已编辑
  // 则尊重用户输入不动 form
  const formSourceRef = useRef<SettingsType | null>(null);
  useEffect(() => {
    if (!settings) return;
    const theme = normalizeLegacyTheme(settings.theme);
    const next = { ...settings, theme };
    setForm((prev) => {
      if (prev == null) {
        formSourceRef.current = next;
        return next;
      }
      const src = formSourceRef.current;
      if (src == null || JSON.stringify(prev) === JSON.stringify(src)) {
        formSourceRef.current = next;
        return next;
      }
      return prev;
    });
    // 仅在 settings 变化时跟随；form 置于依赖会破坏「编辑中不被覆盖」语义
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [settings]);

  // 审查修复（2026-11-21，脏标记误报）：form 挂载时对 theme 做旧值归一，而
  // settings 保持原值——legacy light/dark 下 JSON 比较恒不相等，打开面板即显示
  // 「有未保存的更改」。比较基线对 settings 做同款归一（dirty 与逐字段 patch 同源）
  const savedBaseline = settings != null ? { ...settings, theme: normalizeLegacyTheme(settings.theme) } : null;
  const dirty = form != null && savedBaseline != null && JSON.stringify(form) !== JSON.stringify(savedBaseline);

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

  // 键盘等价路径（可达性）：↑/↓ 与相邻行换位——拖拽为指针交互，键盘用户由此调整顺序
  const moveApp = (key: AppKey, dir: -1 | 1) => {
    if (!form) return;
    const order = getOrderedApps(form);
    const i = order.indexOf(key);
    const j = i + dir;
    if (i < 0 || j < 0 || j >= order.length) return;
    reorderApps(key, order[j]);
  };

  // 抓手按下启动拖拽：window 级 pointermove 实时检测指针所在行并换位（松开即止）。
  // Pointer Events 统一鼠标/触摸（docker Web 形态会被移动端浏览器访问，
  // 触摸端由抓手 touch-none 阻止滚动打断）；不用 HTML5 DnD——Tauri Windows 端
  // dragDropEnabled 默认开启，原生文件拖放处理器会拦截网页内 dragstart，
  // draggable 完全失效（实测无法拖动）
  const beginDrag = (key: AppKey) => (e: React.PointerEvent) => {
    if (e.button !== 0) return; // 仅左键触发（右键/中键拖拽属误触，右键还会被 preventDefault 抑制菜单）
    e.preventDefault(); // 防止拖动时选中文字
    let current = key;
    setDragKey(current);
    const onMove = (ev: PointerEvent) => {
      // 按键已全部抬起却没有收到 pointerup：焦点被抢走（alt-tab/系统弹窗）后
      // 松开的事件落在了别处。此时若继续处理移动会无按键持续换位、拖拽态
      // 卡死到下一次点击，且乱序会被「保存设置」持久化——立即结束拖拽。
      if (ev.buttons === 0) {
        onUp();
        return;
      }
      const rows = listRef.current?.querySelectorAll<HTMLElement>('[data-app-key]');
      if (!rows) return;
      rows.forEach((el) => {
        const r = el.getBoundingClientRect();
        // 水平边界同判（审查修复）：本区块为两列布局，指针移入左栏同高区域
        // 不得触发换位（仅按 Y 判定会静默打乱行序）
        if (ev.clientX >= r.left && ev.clientX < r.right && ev.clientY >= r.top && ev.clientY < r.bottom) {
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
    window.addEventListener('pointercancel', onUp); // 触摸端被浏览器接管滚动时兜底收尾
    dragCleanupRef.current = onUp;
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
    if (!form || !savedBaseline) return;
    // 审查修复（2026-11-21，L1）：无任何改动（设置 patch 为空且通知渠道未改）时
    // 不再弹「设置已保存」假成功提示
    const hasPatch = (Object.keys(form) as (keyof SettingsType)[]).some(
      (k) => JSON.stringify(form[k] ?? null) !== JSON.stringify(savedBaseline[k] ?? null),
    );
    if (!hasPatch && !notifyDirty) {
      toast('success', '设置无更改');
      return;
    }
    setSaving(true);
    // 审查修复（2026-11-21，M3）：设置保存与通知渠道保存解耦——原实现
    // saveSettings 抛错时直接进 catch，通知渠道保存被整体跳过且无任何提示，
    // 用户改的 Bark/Webhook 配置无声丢失
    let settingsErr = false;
    if (hasPatch) {
      try {
        // 审查修复（L35）：逐字段 diff，只把与已保存值不同的字段提交（saveSettings 即 patch 语义），不回传整份表单快照
        const patch: Partial<SettingsType> = {};
        (Object.keys(form) as (keyof SettingsType)[]).forEach((k) => {
          if (JSON.stringify(form[k] ?? null) !== JSON.stringify(savedBaseline[k] ?? null)) {
            (patch as Record<string, unknown>)[k] = form[k];
          }
        });
        await withMinDelay(saveSettings(patch));
        // 第五轮审查修复：form 已持久化，来源快照置空——refreshSettings 回读的
        // 最新 settings 到达时无条件重新对齐（含保存期间他端的并发更新）
        formSourceRef.current = null;
      } catch {
        settingsErr = true; // 失败 toast 已由 saveSettings 发出
      }
    }
    // 通知渠道（独立 kv）随底部保存统一持久化；两路失败分别报错不互相吞没
    let notifyErr: string | null = null;
    if (notifyForm && notifyDirty) {
      try {
        const saved = await withMinDelay(api.notify.setConfig(notifyForm));
        setNotifyForm(saved);
        setNotifyLoaded(saved);
        // 保存成功后补一次最近推送刷新：保存期间发生的调度推送立即回显对齐
        loadLastNotifyResult();
      } catch (e) {
        notifyErr = e instanceof Error ? e.message : '保存失败';
      }
    }
    if (settingsErr && notifyErr) toast('error', `设置保存失败；通知渠道保存失败：${notifyErr}`);
    else if (settingsErr) {
      /* saveSettings 已报错，通知渠道若保存成功无需再提示 */
    } else if (notifyErr) toast('error', `设置已保存，但通知渠道保存失败：${notifyErr}`);
    else toast('success', '设置已保存');
    setSaving(false);
  };

  const reset = () => {
    if (settings) {
      const next = { ...settings, theme: normalizeLegacyTheme(settings.theme) };
      setForm(next);
      // 撤销后 form 重新对齐当前 settings：回写来源快照，后续 settings 变化可继续跟随
      formSourceRef.current = next;
    }
    if (notifyLoaded) setNotifyForm(notifyLoaded);
  };

  // ---- 通知渠道（Trae / Buddy 全平台共用；独立 kv，随底部「保存设置」统一持久化）----
  const [notifyForm, setNotifyForm] = useState<NotifyConfigType | null>(null);
  const [notifyLoaded, setNotifyLoaded] = useState<NotifyConfigType | null>(null);
  const [notifyTesting, setNotifyTesting] = useState(false);
  // 读取失败可见化（2026-11-21 审查修复）：原先 catch 静默置 null，整个「通知渠道」
  // 区块不渲染且无提示——用户无从分辨「未配置」与「读取失败」，也无法修改渠道
  const [notifyLoadError, setNotifyLoadError] = useState<string | null>(null);

  const loadNotifyConfig = useCallback(() => {
    api.notify
      .getConfig()
      .then((c) => {
        setNotifyForm(c);
        setNotifyLoaded(c);
        setNotifyLoadError(null);
      })
      .catch((e) => {
        setNotifyForm(null);
        setNotifyLoaded(null);
        setNotifyLoadError(String(e));
      });
  }, []);

  useEffect(() => {
    loadNotifyConfig();
  }, [loadNotifyConfig]);
  const notifyDirty =
    notifyForm != null && notifyLoaded != null && JSON.stringify(notifyForm) !== JSON.stringify(notifyLoaded);

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
      loadLastNotifyResult();
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

      {/* 应用显示（侧边栏应用 Tab：固定应用单选 + 显示勾选 + 拖拽排序 + 行首图标点击换图标） */}
      <section className="card p-4">
        <h3 className="mb-1 font-medium">应用显示</h3>
        <p className="mb-3 text-xs text-slate-400">
          控制左下角侧边栏显示哪些应用。「固定应用」始终显示且不可隐藏（默认 Trae）；其余应用可自由勾选。
          「侧边栏显示」行可上下拖动调整展示顺序（保存后侧边栏应用 Tab 按新顺序排列）；
          隐藏仅收起入口，<b>不删除账号数据、不影响签到 / 保活等定时任务</b>，重新勾选即可恢复；
          若当前正在浏览的应用被隐藏，会自动跳回固定应用。按住行首抓手上下拖动调整顺序，图标可点击更换应用图标。
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
                  {/* 拖拽抓手：按住上下拖动实时换位（仅抓手触发，避免误碰勾选/图标按钮）；
                      键盘等价路径 = 聚焦后 ↑/↓ 换位（可达性），touch-none 防触摸端
                      滚动打断 pointer 拖拽 */}
                  <span
                    title="按住拖动或聚焦后按 ↑/↓ 调整显示顺序"
                    tabIndex={0}
                    aria-label={`调整「${tab.label}」显示顺序（上下方向键移动）`}
                    className="flex shrink-0 cursor-grab touch-none active:cursor-grabbing"
                    onPointerDown={beginDrag(tab.key)}
                    onKeyDown={(e) => {
                      if (e.key === 'ArrowUp' || e.key === 'ArrowDown') {
                        e.preventDefault();
                        moveApp(tab.key, e.key === 'ArrowUp' ? -1 : 1);
                      }
                    }}
                  >
                    <GripVertical
                      size={14}
                      className="text-slate-300 dark:text-zinc-600"
                      aria-hidden
                    />
                  </span>
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

      {/* 通知渠道读取失败态：错误占位 + 重试（对齐 poolList/账号列表同类修复范式） */}
      {!notifyForm && notifyLoadError && (
        <section className="card p-4">
          <div className="flex items-center gap-2">
            <BellRing size={16} className="text-brand-500" />
            <h3 className="font-medium">通知渠道</h3>
          </div>
          <p className="mt-2 text-sm text-red-500">通知渠道配置读取失败：{notifyLoadError}</p>
          <button onClick={loadNotifyConfig} className="btn-outline mt-2">
            <RefreshCw size={15} /> 重试
          </button>
        </section>
      )}

      {/* 通知渠道（全部应用共用：Bark / Server酱 / Webhook；随底部「保存设置」统一保存） */}
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
            签到完成 / 调度任务失败 / 定时任务完成（默认关，仅签到/成长类奖励任务，快照/续期/同步类高频任务不推；注意 Server酱 免费档每日仅 5 条）时推送到手机（Bark / Server酱）或自建
            Webhook，对全部应用（Trae、WorkBuddy、Qoder、Buddy 国际版）共用，未配置的渠道自动跳过；修改后点底部「保存设置」生效。
          </p>
          {/* 事件开关组：横排一条，与下方渠道输入视觉分离 */}
          <div className="mb-3 flex flex-wrap items-center gap-x-5 gap-y-2 rounded-lg border border-slate-200/70 bg-slate-50/60 px-3.5 py-2.5 text-sm dark:border-zinc-700/50 dark:bg-zinc-800/30">
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
            <label
              className="flex items-center gap-2"
              title="签到/成长类奖励任务成功完成时推送（快照/续期/目录同步类高频任务不推，避免刷屏并保护渠道额度）；Server酱 免费档每日仅 5 条"
            >
              <input
                type="checkbox"
                checked={notifyForm.on_task_success}
                onChange={(e) => setNotifyForm({ ...notifyForm, on_task_success: e.target.checked })}
              />
              定时任务完成时通知
            </label>
          </div>
          <div className="grid gap-x-4 gap-y-3 text-sm sm:grid-cols-2">
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
            <div className="sm:col-span-2">
              <label className="label">通用 Webhook 地址</label>
              <input
                value={notifyForm.webhook_url ?? ''}
                onChange={(e) => setNotifyForm({ ...notifyForm, webhook_url: e.target.value || null })}
                placeholder="https://…（POST JSON，兼容企业微信机器人等自建端）"
                className="input w-full"
              />
            </div>
          </div>
          {/* 最近推送状态（issue #222）：渠道失败此前只静默落 app.log，面板直接回显成败与原因。
              仅展示当前仍配置的渠道（审查修复 #3：清空渠道后的陈旧条目不再误导）；「测试」
              标记区分来源（审查修复 #7，src="test" 由 notify_test 写入）。docker 渠道集：
              bark / webhook / serverchan（无 pushplus） */}
          {Object.keys(lastNotifyResult).length > 0 && (
            <div className="mt-3 rounded-lg border border-slate-200/70 bg-slate-50/60 px-3.5 py-2.5 text-xs dark:border-zinc-700/50 dark:bg-zinc-800/30">
              <div className="mb-1 font-medium text-slate-500 dark:text-zinc-400">最近推送</div>
              <div className="space-y-1">
                {Object.entries(lastNotifyResult)
                  .filter(([k]) => {
                    const configured: Record<string, boolean> = {
                      bark: Boolean(notifyForm.bark_url?.trim()),
                      // 键名对齐后端写入的 webhook 键（原 wechat 键恒 miss，Webhook
                      // 推送记录被误过滤）；pushplus 后端不写入，恒 false
                      webhook: Boolean(notifyForm.webhook_url?.trim()),
                      serverchan: Boolean(notifyForm.serverchan_sendkey?.trim()),
                      pushplus: false,
                    };
                    return configured[k];
                  })
                  .map(([k, v]) => (
                    <div key={k} className="flex flex-wrap items-baseline gap-x-2">
                      <span className={v.ok ? 'text-emerald-600 dark:text-emerald-400' : 'text-red-600 dark:text-red-400'}>
                        {NOTIFY_CHANNEL_LABELS[k] ?? k}
                        {v.src === 'test' ? '（测试）' : ''} · {v.ok ? '成功' : '失败'}
                      </span>
                      <span className="text-slate-400 dark:text-zinc-500">{new Date(v.ts).toLocaleString()}</span>
                      {!v.ok && v.err && <span className="text-slate-400 dark:text-zinc-500">{v.err}</span>}
                    </div>
                  ))}
              </div>
            </div>
          )}
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
