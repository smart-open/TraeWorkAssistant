import { useEffect, useState } from 'react';
import { Save, RotateCcw, Bell, Send } from 'lucide-react';
import { useAppStore } from '../store';
import { withMinDelay } from '../lib/delay';
import { api } from '../lib/tauri';
import { THEMES } from '../lib/themes';
import type { Settings as SettingsType } from '../types';

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
            <p className="text-xs text-slate-400">托盘与最小化设置变更后需重启应用生效。</p>
          </div>
        </div>
      </section>

      {/* 通知渠道（F-19，Trae/Buddy 全平台共用） */}
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
          签到完成 / 调度任务失败时推送到手机（Bark / Server酱）或自建 Webhook，Trae 与 Buddy 全平台共用，
          未配置的渠道自动跳过；修改后点底部「保存设置」生效。
        </p>
        <div className="grid gap-x-8 gap-y-3 text-sm sm:grid-cols-2">
          <div className="space-y-3">
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
          <div className="space-y-3">
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
            <div>
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
