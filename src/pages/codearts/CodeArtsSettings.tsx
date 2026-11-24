import { useCallback, useEffect, useState } from 'react';
import { AlertTriangle, ListChecks, RefreshCw, Save, ShieldAlert } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { Badge, Modal, Spinner } from '../../components/ui';
import { withMinDelay } from '../../lib/delay';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import type { CodeartsSettings } from '../../types';

/**
 * codearts-settings 环境配置：签到时刻 + 应用内调度开关 + schtasks 任务注册/注销。
 * 精简自 QoderSettings（去掉客户端路径 / 积分同步 / 会话合并 / Token 续期）。
 */
const isValidHHMM = (s: string) => /^([01]\d|2[0-3]):[0-5]\d$/.test(s.trim());

export default function CodeArtsSettings() {
  const pushToast = useAppStore((s) => s.pushToast);
  const [codeartsSettings, setCodeartsSettings] = useState<CodeartsSettings | null>(null);
  const [settingsErr, setSettingsErr] = useState(false);
  const [taskTimes, setTaskTimes] = useState<string[]>([]);
  const [checkinHhmm, setCheckinHhmm] = useState('10:15');
  const [refreshing, setRefreshing] = useState(false);
  const [saving, setSaving] = useState(false);
  const [taskBusy, setTaskBusy] = useState(false);
  const [confirmUnregister, setConfirmUnregister] = useState(false);

  const loadSettings = useCallback(async () => {
    try {
      const s = await api.codearts.settingsGet();
      setCodeartsSettings(s);
      setCheckinHhmm(s.checkin_hhmm || '10:15');
      setSettingsErr(false);
    } catch {
      setCodeartsSettings(null);
      setSettingsErr(true);
    }
  }, []);

  const refresh = useCallback(async () => {
    setRefreshing(true);
    try {
      const ts = await api.codearts.checkinTaskStatus().catch(() => [] as string[]);
      setTaskTimes(ts);
      await loadSettings();
    } catch (err) {
      pushToast('error', `检测失败：${String(err)}`);
    } finally {
      setRefreshing(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [loadSettings]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const save = async () => {
    if (settingsErr || !codeartsSettings) {
      pushToast('error', 'codearts-settings 未就绪，暂不可保存；请点「重新检测」后重试');
      return;
    }
    if (!isValidHHMM(checkinHhmm)) {
      pushToast('error', `签到时刻格式无效：${checkinHhmm}（应为 HH:MM）`);
      return;
    }
    setSaving(true);
    try {
      await withMinDelay(
        api.codearts.settingsSet({ ...codeartsSettings, checkin_hhmm: checkinHhmm.trim() }),
        500,
      );
      pushToast('success', '配置已保存');
      await refresh();
    } catch (err) {
      pushToast('error', `保存失败：${String(err)}`);
    } finally {
      setSaving(false);
    }
  };

  const registerTask = async () => {
    if (taskBusy) return;
    if (!isValidHHMM(checkinHhmm)) {
      pushToast('error', `签到时刻格式无效：${checkinHhmm}（应为 HH:MM）`);
      return;
    }
    setTaskBusy(true);
    try {
      await api.codearts.checkinTaskRegister([checkinHhmm.trim()]);
      setTaskTimes(await api.codearts.checkinTaskStatus());
      pushToast('info', `计划任务已按 ${checkinHhmm.trim()} 注册；应用内定时需点击「保存配置」后生效`);
    } catch (err) {
      pushToast('error', `注册失败：${String(err)}`);
    } finally {
      setTaskBusy(false);
    }
  };

  const unregisterTask = async () => {
    if (taskBusy) return;
    setTaskBusy(true);
    try {
      await api.codearts.checkinTaskUnregister();
      setTaskTimes(await api.codearts.checkinTaskStatus());
      pushToast('success', 'Windows 计划任务已卸载');
    } catch (err) {
      pushToast('error', `卸载失败：${String(err)}`);
    } finally {
      setTaskBusy(false);
    }
  };

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="CodeArts · 环境配置"
        desc="签到时刻 · 应用内调度开关 · Windows 计划任务"
        actions={
          <>
            <button className="btn-outline" disabled={refreshing} onClick={() => void refresh()}>
              <RefreshCw size={15} className={refreshing ? 'animate-spin' : ''} /> 重新检测
            </button>
            <button className="btn-outline" disabled={saving} onClick={() => void save()}>
              {saving ? <Spinner /> : <Save size={15} />} 保存配置
            </button>
          </>
        }
      />

      {/* 合规提示 */}
      <div className="mb-4 flex items-start gap-2 rounded-lg border border-amber-200 bg-amber-50 p-3 text-xs text-amber-700 dark:border-amber-500/30 dark:bg-amber-500/10 dark:text-amber-300">
        <ShieldAlert size={15} className="mt-0.5 shrink-0" />
        <span>
          合规提示：请通过华为云码道官方渠道录入 AK/SK，并自行评估自动化签到相关的条款风险。
          SK 仅在录入时提交一次、加密落库，前端不回显。
        </span>
      </div>

      <div className="grid items-start gap-4 lg:grid-cols-2">
        {/* 左列：签到行为 */}
        <div className="card p-4">
          <div className="mb-3 flex items-center gap-2">
            <ListChecks size={16} className="text-violet-500" />
            <h2 className="font-medium">签到行为</h2>
            <span className="text-xs text-slate-400">随右上角「保存配置」生效</span>
          </div>

          <label
            className={`flex items-start gap-2 rounded-lg border p-3 ${
              settingsErr
                ? 'border-amber-200 bg-amber-50/50 opacity-70 dark:border-amber-500/30 dark:bg-amber-500/5'
                : 'border-slate-100 dark:border-zinc-800'
            }`}
          >
            <input
              type="checkbox"
              className="mt-0.5"
              disabled={settingsErr}
              checked={codeartsSettings?.auto_checkin ?? true}
              onChange={(e) =>
                setCodeartsSettings((s) => (s ? { ...s, auto_checkin: e.target.checked } : s))
              }
            />
            <span className="text-sm">
              自动签到（默认开）
              <span className="block text-xs text-slate-400">
                应用内调度器到点自动执行签到，启动时自动补跑当日已过时刻；多账号串行签到已领账号自动跳过
              </span>
              {settingsErr && (
                <span className="mt-1 block text-xs text-amber-600 dark:text-amber-400">
                  codearts-settings 加载失败，当前显示为默认值且不可修改；请点「重新检测」重试
                </span>
              )}
            </span>
          </label>

          <div className="my-4 border-t border-slate-100 dark:border-zinc-800" />

          <div className="flex items-center justify-between">
            <h3 className="font-medium">签到时刻</h3>
            <Badge tone="slate">HH:MM</Badge>
          </div>
          <p className="mb-3 mt-1 text-xs text-slate-400">
            应用内调度器每日到点自动签到；下方 Windows 计划任务作为兜底，应用未启动时直接运行。
          </p>
          <div className="flex flex-wrap items-center gap-2 rounded-lg border border-slate-100 p-3 dark:border-zinc-800">
            <input
              type="time"
              className="input h-9 !w-28 text-sm"
              value={checkinHhmm}
              onChange={(e) => setCheckinHhmm(e.target.value || '10:15')}
            />
            <span className="text-xs text-slate-400">每天 {checkinHhmm || '10:15'} 自动执行</span>
          </div>
        </div>

        {/* 右列：任务配置 */}
        <div className="card p-4">
          <div className="mb-3 flex items-center gap-2">
            <ListChecks size={16} className="text-violet-500" />
            <h2 className="font-medium">任务配置</h2>
            <span className="text-xs text-slate-400">Windows 计划任务兜底</span>
          </div>
          <div className="rounded-lg border border-slate-100 p-3 dark:border-zinc-800">
            <h3 className="mb-2 font-medium">每日自动签到（兜底）</h3>
            <div className="mb-2 text-xs text-slate-400">
              注册后应用未启动时也会到点执行签到；时刻取上方输入框当前值，注册即生效。
            </div>
            <div className="mb-2 text-xs text-slate-500">
              状态：{taskTimes.length > 0 ? `已注册：${taskTimes.join('、')}` : '未注册'}
            </div>
            <div className="flex flex-wrap items-center gap-2">
              {taskTimes.length > 0 ? (
                <button
                  className="btn-ghost !px-3 !py-1 text-xs text-rose-500"
                  disabled={taskBusy}
                  onClick={() => setConfirmUnregister(true)}
                >
                  {taskBusy ? <Spinner /> : null} 卸载任务
                </button>
              ) : (
                <button
                  className="btn-outline !px-3 !py-1 text-xs"
                  disabled={taskBusy}
                  onClick={() => void registerTask()}
                >
                  {taskBusy ? <Spinner /> : null} 注册任务
                </button>
              )}
            </div>
          </div>
        </div>
      </div>

      {/* 卸载确认弹框 */}
      <Modal
        open={confirmUnregister}
        onClose={() => setConfirmUnregister(false)}
        title="确认卸载定时签到任务"
        footer={
          <>
            <button className="btn-outline" onClick={() => setConfirmUnregister(false)}>
              取消
            </button>
            <button
              className="btn-danger"
              onClick={() => {
                setConfirmUnregister(false);
                void unregisterTask();
              }}
            >
              确认卸载
            </button>
          </>
        }
      >
        <div className="flex items-start gap-3">
          <AlertTriangle size={20} className="mt-0.5 shrink-0 text-amber-500" />
          <div>
            <p>
              确认卸载 Windows 定时签到计划任务
              {taskTimes.length > 0 ? `（${taskTimes.join('、')}）` : ''}？
            </p>
            <p className="mt-2 text-xs text-slate-400">
              卸载后应用未启动时将不再有签到兜底；应用内置调度不受影响，之后可随时重新注册。
            </p>
          </div>
        </div>
      </Modal>
    </div>
  );
}
