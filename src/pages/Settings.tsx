import { useEffect, useState } from 'react';
import { Calendar, Trash2, Save, Search, RotateCcw, Fingerprint, Clock, AlertTriangle } from 'lucide-react';
import PageHeader from '../components/PageHeader';
import { Modal } from '../components/ui';
import { useAppStore } from '../store';
import { api } from '../lib/tauri';
import { withMinDelay } from '../lib/delay';
import type { Settings as SettingsType } from '../types';

/**
 * 环境配置页：应用安装路径、签到行为、定时任务与设备标识重置。
 * 外观 / 语言 / 通用与通知 / 代理配置已移至左下角系统图标的「系统设置」弹框。
 */
export default function Settings() {
  const settings = useAppStore((s) => s.settings);
  const saveSettings = useAppStore((s) => s.saveSettings);
  const refreshSettings = useAppStore((s) => s.refreshSettings);
  const resetDeviceIds = useAppStore((s) => s.resetDeviceIds);
  const deviceResetActive = useAppStore((s) => s.deviceResetActive);
  const deviceResetProgress = useAppStore((s) => s.deviceResetProgress);
  const toast = useAppStore((s) => s.pushToast);

  const [time, setTime] = useState('09:00');
  const [taskInfo, setTaskInfo] = useState<string>('');
  const [busyTask, setBusyTask] = useState(false);
  const [querying, setQuerying] = useState(false);
  const [deviceTarget, setDeviceTarget] = useState<'TraeWork' | 'Trae'>('TraeWork');
  const [detecting, setDetecting] = useState(false);
  const [detectingCn, setDetectingCn] = useState(false);

  // 确认弹窗状态
  const [confirmUnregister, setConfirmUnregister] = useState(false);
  const [confirmResetDevice, setConfirmResetDevice] = useState(false);

  // 本地表单状态：用户编辑后点击「保存」才持久化，避免每次按键都写文件
  const [form, setForm] = useState<SettingsType | null>(null);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    void refreshSettings();
    void query();
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

  const register = async () => {
    setBusyTask(true);
    try {
      await withMinDelay(api.misc.taskRegister(time));
      toast('success', `已注册每日 ${time} 自动签到`);
      await query();
    } catch (e) {
      const msg = String(e);
      // 长错误信息（含换行）在 taskInfo 区域展示，toast 只给简短提示
      if (msg.includes('\n')) {
        setTaskInfo(`❌ ${msg}`);
        toast('error', '注册失败：权限不足，请查看下方详细解决方案');
      } else {
        toast('error', `注册失败：${msg}`);
      }
    } finally {
      setBusyTask(false);
    }
  };

  const unregister = async () => {
    setConfirmUnregister(false);
    setBusyTask(true);
    try {
      await withMinDelay(api.misc.taskUnregister());
      toast('info', '计划任务已删除');
      setTaskInfo('');
    } catch (e) {
      toast('error', `删除失败：${String(e)}`);
    } finally {
      setBusyTask(false);
    }
  };

  const query = async () => {
    setQuerying(true);
    try {
      const r = await withMinDelay(api.misc.taskStatus());
      setTaskInfo(r);
      toast('success', '查询完成');
    } catch (e) {
      setTaskInfo(`查询失败：${String(e)}`);
      toast('error', `查询失败：${String(e)}`);
    } finally {
      setQuerying(false);
    }
  };

  // F-01 探测来源中文标签
  const LOCATE_SOURCE_LABEL: Record<string, string> = {
    settings: '手动指定',
    registry: '注册表',
    default: '默认路径',
    process: '运行进程',
  };

  const detectTrae = async () => {
    setDetecting(true);
    try {
      const r = await withMinDelay(api.env.locate('trae_work'));
      if (r.exe) {
        update('trae_path', r.exe);
        toast('success', `已自动定位并填入 Trae Work 路径（${LOCATE_SOURCE_LABEL[r.source] ?? r.source}${r.version ? `，版本 ${r.version}` : ''}）`);
      } else {
        toast('info', '未检测到 Trae Work，请手动指定 exe 路径');
      }
    } catch (e) {
      toast('error', `检测失败：${String(e)}`);
    } finally {
      setDetecting(false);
    }
  };

  const detectTraeCn = async () => {
    setDetectingCn(true);
    try {
      const r = await withMinDelay(api.env.locate('trae'));
      if (r.exe) {
        update('trae_cn_path', r.exe);
        toast('success', `已自动定位并填入 Trae 路径（${LOCATE_SOURCE_LABEL[r.source] ?? r.source}${r.version ? `，版本 ${r.version}` : ''}）`);
      } else {
        toast('info', '未检测到 Trae，请手动指定 exe 路径');
      }
    } catch (e) {
      toast('error', `检测失败：${String(e)}`);
    } finally {
      setDetectingCn(false);
    }
  };

  const handleResetDeviceIds = async () => {
    setConfirmResetDevice(false);
    await resetDeviceIds(deviceTarget);
  };

  if (!form) {
    return (
      <div className="animate-fade-in">
        <PageHeader title="环境配置" />
        <div className="text-sm text-slate-500">加载中…</div>
      </div>
    );
  }

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="环境配置"
        desc="应用安装路径、签到行为、定时任务与设备标识"
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
        {/* 左列：应用环境 + 设备标识重置 */}
        <section className="card p-4">
          <h3 className="mb-1 font-medium">应用环境</h3>
          <p className="mb-3 text-xs text-slate-400">
            两个 Trae 应用的安装路径，用于「打开应用」与「切换账号」时定位 exe；留空将自动探测。
          </p>
          <div className="space-y-3 text-sm">
            <div>
              <label className="label">Trae Work 安装路径</label>
              <div className="flex items-center gap-2">
                <input
                  type="text"
                  value={form.trae_path ?? ''}
                  onChange={(e) => update('trae_path', e.target.value.trim() || null)}
                  placeholder="默认 C:\Users\你\AppData\Local\Programs\TRAE SOLO CN\TRAE SOLO CN.exe"
                  className="input flex-1"
                />
                <button onClick={detectTrae} disabled={detecting} className="btn-outline shrink-0">
                  <Search size={15} /> {detecting ? '检测中…' : '自动检测'}
                </button>
              </div>
              <p className="mt-1 text-xs text-slate-400">
                TRAE SOLO CN（Trae Work）的 exe 路径，自定义安装目录时需填写。
              </p>
            </div>
            <div>
              <label className="label">Trae 安装路径</label>
              <div className="flex items-center gap-2">
                <input
                  type="text"
                  value={form.trae_cn_path ?? ''}
                  onChange={(e) => update('trae_cn_path', e.target.value.trim() || null)}
                  placeholder="默认 C:\Users\你\AppData\Local\Programs\Trae CN\Trae CN.exe"
                  className="input flex-1"
                />
                <button onClick={detectTraeCn} disabled={detectingCn} className="btn-outline shrink-0">
                  <Search size={15} /> {detectingCn ? '检测中…' : '自动检测'}
                </button>
              </div>
              <p className="mt-1 text-xs text-slate-400">Trae CN IDE 的 exe 路径。</p>
            </div>
          </div>

          <div className="my-4 border-t border-slate-100 dark:border-zinc-800" />

          <h3 className="mb-1 font-medium">6 层设备标识重置</h3>
          <p className="mb-3 text-xs text-slate-500">
            一次性重置所选应用的全部设备标识层：① machineid ② storage.json telemetry ③ storage.json aha.device ④
            TinyStorage ⑤ 注册表 MachineGuid ⑥ webview 追踪数据。用于账号隔离与防关联，执行前请先关闭对应应用。
          </p>
          <div className="flex flex-wrap items-center gap-2">
            <select
              value={deviceTarget}
              onChange={(e) => setDeviceTarget(e.target.value as 'TraeWork' | 'Trae')}
              disabled={deviceResetActive}
              className="input h-9 w-36 text-sm"
            >
              <option value="TraeWork">Trae Work</option>
              <option value="Trae">Trae</option>
            </select>
            <button
              onClick={() => setConfirmResetDevice(true)}
              disabled={deviceResetActive}
              className="btn-primary"
            >
              <Fingerprint size={15} /> {deviceResetActive ? '重置中…' : '执行 6 层重置'}
            </button>
            {deviceResetActive && (
              <span className="text-xs text-amber-500 animate-pulse">正在执行，请勿关闭应用…</span>
            )}
          </div>
          {deviceResetProgress.length > 0 && (
            <pre className="mt-3 max-h-40 overflow-auto whitespace-pre-wrap rounded-lg bg-slate-50 p-3 text-xs dark:bg-zinc-950">
              {deviceResetProgress.join('\n')}
            </pre>
          )}
        </section>

        {/* 右列：签到行为 + 每日定时签到（同一面板） */}
        <section className="card p-4">
          <h3 className="mb-1 font-medium">签到行为</h3>
          <p className="mb-3 text-xs text-slate-400">批量签到时的默认跳过策略与重试参数，对所有签到入口生效。</p>
          <div className="space-y-3 text-sm">
            <label className="flex items-center gap-2">
              <input
                type="checkbox"
                checked={form.checkin_skip_checked}
                onChange={(e) => update('checkin_skip_checked', e.target.checked)}
              />
              默认跳过今日已签账号
            </label>
            <label className="flex items-center gap-2">
              <input
                type="checkbox"
                checked={form.checkin_skip_expired}
                onChange={(e) => update('checkin_skip_expired', e.target.checked)}
              />
              默认跳过 JWT 过期账号
            </label>
            <div>
              <label className="label">失败重试次数（签到失败后的重试次数）</label>
              <input
                type="number"
                value={form.retry}
                onChange={(e) => update('retry', Math.min(5, Math.max(0, Number(e.target.value) || 0)))}
                className="input w-24"
                min={0}
                max={5}
              />
            </div>
          </div>

          <div className="my-4 border-t border-slate-100 dark:border-zinc-800" />

          <h3 className="mb-1 font-medium">每日定时签到</h3>
          <p className="mb-3 text-xs text-slate-400">
            通过 Windows 计划任务在指定时间自动运行签到脚本，无需启动应用界面。注册/删除需要管理员权限。
          </p>
          <div className="flex flex-wrap items-center gap-2">
            <div className="relative flex items-center">
              <Clock size={15} className="pointer-events-none absolute left-2.5 text-slate-400" />
              <input
                type="time"
                value={time}
                onChange={(e) => setTime(e.target.value)}
                className="input h-9 !w-32 pl-8 text-sm"
              />
            </div>
            <button onClick={register} disabled={busyTask} className="btn-primary">
              <Calendar size={15} /> {busyTask ? '注册中…' : '注册任务'}
            </button>
            <button onClick={query} disabled={querying} className="btn-outline">
              <Search size={15} /> {querying ? '查询中…' : '查询'}
            </button>
            <button onClick={() => setConfirmUnregister(true)} disabled={busyTask} className="btn-danger">
              <Trash2 size={15} /> {busyTask ? '删除中…' : '取消'}
            </button>
          </div>
          {taskInfo && (
            <pre
              className={`mt-3 overflow-auto whitespace-pre-wrap rounded-lg p-3 text-xs ${
                taskInfo.startsWith('❌')
                  ? 'max-h-80 border border-rose-200 bg-rose-50 text-rose-700 dark:border-rose-800 dark:bg-rose-900/20 dark:text-rose-300'
                  : 'max-h-40 bg-slate-50 dark:bg-zinc-950'
              }`}
            >
              {taskInfo}
            </pre>
          )}
        </section>
      </div>

      {/* 底部悬浮保存条 */}
      {dirty && (
        <div className="mt-4 flex items-center justify-end gap-2 rounded-lg border border-amber-300 bg-amber-50 p-3 dark:border-amber-700 dark:bg-amber-900/20">
          <span className="text-sm text-amber-600 dark:text-amber-400">有未保存的更改</span>
          <button onClick={reset} className="btn-outline">
            <RotateCcw size={15} /> 撤销更改
          </button>
          <button onClick={save} disabled={saving} className="btn-primary">
            <Save size={15} /> {saving ? '保存中…' : '保存设置'}
          </button>
        </div>
      )}

      {/* 确认删除计划任务 */}
      <Modal
        open={confirmUnregister}
        onClose={() => setConfirmUnregister(false)}
        title="确认删除计划任务"
        footer={
          <>
            <button className="btn-outline" onClick={() => setConfirmUnregister(false)}>取消</button>
            <button className="btn-danger" onClick={() => void unregister()}>确认删除</button>
          </>
        }
      >
        <div className="flex items-start gap-3">
          <AlertTriangle size={20} className="mt-0.5 shrink-0 text-amber-500" />
          <div>
            <p>确认删除「AIWorkAssistant_DailyCheckin」计划任务？</p>
            <p className="mt-2 text-xs text-slate-400">删除后将不再自动执行每日签到。</p>
          </div>
        </div>
      </Modal>

      {/* 确认执行设备标识重置 */}
      <Modal
        open={confirmResetDevice}
        onClose={() => setConfirmResetDevice(false)}
        title="确认执行 6 层设备标识重置"
        footer={
          <>
            <button className="btn-outline" onClick={() => setConfirmResetDevice(false)}>取消</button>
            <button className="btn-primary" onClick={() => void handleResetDeviceIds()}>确认重置</button>
          </>
        }
      >
        <div className="flex items-start gap-3">
          <AlertTriangle size={20} className="mt-0.5 shrink-0 text-amber-500" />
          <div>
            <p>
              将重置 <span className="font-semibold">{deviceTarget === 'Trae' ? 'Trae（Trae CN IDE）' : 'Trae Work（TRAE SOLO CN）'}</span> 的以下全部设备标识层：
            </p>
            <ul className="mt-2 space-y-0.5 text-xs text-slate-400">
              <li>① machineid</li>
              <li>② storage.json telemetry</li>
              <li>③ storage.json aha.device</li>
              <li>④ TinyStorage</li>
              <li>⑤ 注册表 MachineGuid</li>
              <li>⑥ webview 追踪数据</li>
            </ul>
            <p className="mt-2 text-xs text-amber-500">建议先关闭 TRAE 再执行。</p>
          </div>
        </div>
      </Modal>
    </div>
  );
}
