import { useEffect, useState } from 'react';
import { Copy, Calendar, Power, Trash2, Gift, Save } from 'lucide-react';
import PageHeader from '../components/PageHeader';
import { Badge } from '../components/ui';
import { useAppStore } from '../store';
import { api } from '../lib/tauri';

export default function Settings() {
  const settings = useAppStore((s) => s.settings);
  const saveSettings = useAppStore((s) => s.saveSettings);
  const refreshSettings = useAppStore((s) => s.refreshSettings);
  const toast = useAppStore((s) => s.pushToast);

  const [time, setTime] = useState('09:00');
  const [taskInfo, setTaskInfo] = useState<string>('');
  const [busyTask, setBusyTask] = useState(false);

  useEffect(() => {
    void refreshSettings();
  }, [refreshSettings]);

  const update = <K extends keyof NonNullable<typeof settings>>(
    key: K,
    val: NonNullable<typeof settings>[K],
  ) => {
    void saveSettings({ [key]: val } as Partial<NonNullable<typeof settings>>);
  };

  const copyInvite = async () => {
    try {
      const r = await api.misc.inviteLink();
      await navigator.clipboard.writeText(r.url);
      toast('success', '邀请链接已复制');
    } catch (e) {
      toast('error', `复制失败：${String(e)}`);
    }
  };

  const register = async () => {
    setBusyTask(true);
    try {
      await api.misc.taskRegister(time);
      toast('success', `已注册每日 ${time} 自动签到`);
      await query();
    } catch (e) {
      toast('error', `注册失败：${String(e)}`);
    } finally {
      setBusyTask(false);
    }
  };

  const unregister = async () => {
    if (!confirm('确认删除「TraeWorkAssistant_DailyCheckin」计划任务？')) return;
    setBusyTask(true);
    try {
      await api.misc.taskUnregister();
      toast('info', '计划任务已删除');
      setTaskInfo('');
    } catch (e) {
      toast('error', `删除失败：${String(e)}`);
    } finally {
      setBusyTask(false);
    }
  };

  const query = async () => {
    try {
      const r = await api.misc.taskStatus();
      setTaskInfo(r);
    } catch (e) {
      setTaskInfo(`查询失败：${String(e)}`);
    }
  };

  if (!settings) {
    return (
      <div className="animate-fade-in">
        <PageHeader title="设置" />
        <div className="text-sm text-slate-500">加载中…</div>
      </div>
    );
  }

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="设置"
        desc="主题、代理端口、定时任务与邀请链接"
      />

      <div className="grid gap-4 md:grid-cols-2">
        <section className="card p-4">
          <h3 className="mb-3 font-medium">外观</h3>
          <div className="space-y-3 text-sm">
            <div>
              <label className="label">主题</label>
              <select
                value={settings.theme}
                onChange={(e) => update('theme', e.target.value)}
                className="input"
              >
                <option value="system">跟随系统</option>
                <option value="light">浅色</option>
                <option value="dark">深色</option>
              </select>
            </div>
            <div>
              <label className="label">语言</label>
              <select
                value={settings.language}
                onChange={(e) => update('language', e.target.value)}
                className="input"
              >
                <option value="zh-CN">简体中文</option>
                <option value="en-US">English</option>
              </select>
            </div>
          </div>
        </section>

        <section className="card p-4">
          <h3 className="mb-3 font-medium">代理与签到</h3>
          <div className="space-y-3 text-sm">
            <div>
              <label className="label">代理端口</label>
              <input
                type="number"
                value={settings.proxy_port}
                onChange={(e) => update('proxy_port', Number(e.target.value) || 8899)}
                className="input w-32"
              />
            </div>
            <label className="flex items-center gap-2">
              <input
                type="checkbox"
                checked={settings.auto_start_proxy}
                onChange={(e) => update('auto_start_proxy', e.target.checked)}
              />
              启动时自动开启代理
            </label>
            <label className="flex items-center gap-2">
              <input
                type="checkbox"
                checked={settings.checkin_skip_checked}
                onChange={(e) => update('checkin_skip_checked', e.target.checked)}
              />
              签到默认跳过今日已签
            </label>
            <label className="flex items-center gap-2">
              <input
                type="checkbox"
                checked={settings.checkin_skip_expired}
                onChange={(e) => update('checkin_skip_expired', e.target.checked)}
              />
              签到默认跳过 JWT 过期
            </label>
            <div>
              <label className="label">失败重试次数</label>
              <input
                type="number"
                value={settings.retry}
                onChange={(e) => update('retry', Number(e.target.value) || 0)}
                className="input w-24"
                min={0}
                max={5}
              />
            </div>
          </div>
        </section>

        <section className="card p-4 md:col-span-2">
          <h3 className="mb-2 font-medium">每日定时签到</h3>
          <p className="mb-3 text-xs text-slate-500">
            通过 Windows 计划任务在指定时间自动运行 Python 签到脚本（无需启动应用界面）。
            需要管理员权限。
          </p>
          <div className="flex items-end gap-2">
            <div>
              <label className="label">时间</label>
              <input
                type="time"
                value={time}
                onChange={(e) => setTime(e.target.value)}
                className="input"
              />
            </div>
            <button onClick={register} disabled={busyTask} className="btn-primary">
              <Calendar size={15} /> 注册任务
            </button>
            <button onClick={query} className="btn-outline">
              <Save size={15} /> 查询
            </button>
            <button onClick={unregister} disabled={busyTask} className="btn-danger">
              <Trash2 size={15} /> 取消
            </button>
          </div>
          {taskInfo && (
            <pre className="mt-3 max-h-40 overflow-auto whitespace-pre-wrap rounded-lg bg-slate-50 p-3 text-xs dark:bg-slate-950">
              {taskInfo}
            </pre>
          )}
        </section>

        <section className="card p-4">
          <h3 className="mb-2 font-medium">邀请得积分</h3>
          <p className="text-sm text-slate-500">分享链接，双方均可获 5000 积分。</p>
          <button onClick={copyInvite} className="mt-3 w-full bg-amber-500 btn text-white hover:bg-amber-400">
            <Gift size={15} /> 复制邀请链接
          </button>
          <div className="mt-2 break-all rounded bg-slate-100 p-2 text-xs text-slate-500 dark:bg-slate-800">
            https://www.trae.cn/work-fission/4CP3KDBT5W9A
          </div>
        </section>

        <section className="card p-4">
          <h3 className="mb-2 font-medium">关于</h3>
          <div className="space-y-1 text-xs text-slate-500">
            <div>应用版本：v1.0.0</div>
            <div>数据目录：<span className="font-mono">%APPDATA%\TraeWorkAssistant\</span></div>
            <div>代理 Python：内置 device_proxy.py / auto_checkin.py</div>
            <div className="flex items-center gap-2 pt-1">
              <Badge tone="brand">MIT 友好</Badge>
              <Badge tone="slate">仅本地运行</Badge>
            </div>
          </div>
        </section>
      </div>
    </div>
  );
}