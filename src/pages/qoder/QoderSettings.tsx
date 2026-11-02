import { useCallback, useEffect, useRef, useState } from 'react';
import { ListChecks, RefreshCw, Save, Search, ShieldAlert, SlidersHorizontal, AlertTriangle } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { Badge, Spinner, Modal } from '../../components/ui';
import { withMinDelay } from '../../lib/delay';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import type { QoderEnvCheck, QoderSettings } from '../../types';

/**
 * qoder-settings 环境配置（F-80 §5.8，布局对齐 BuddySettings）：
 * 左列 通用配置（应用环境路径 + 签到行为）；右列 任务配置（每日自动签到 + Token 定时续期 + 积分数据同步）。
 * 顶部「重新检测」+ 右上角「保存配置」统一提交（路径 / 时刻 / 开关一处生效）；
 * Windows 计划任务注册、路径自动检测为独立即时动作。
 * 合规提示（条款风险固定展示，不可跳过）。
 */

const isValidHHMM = (s: string) => /^([01]\d|2[0-3]):[0-5]\d$/.test(s.trim());

export default function QoderSettings() {
  const pushToast = useAppStore((s) => s.pushToast);
  const settings = useAppStore((s) => s.settings);
  const saveSettings = useAppStore((s) => s.saveSettings);
  const [qoderSettings, setQoderSettings] = useState<QoderSettings | null>(null);
  const [settingsErr, setSettingsErr] = useState(false);
  const [env, setEnv] = useState<QoderEnvCheck | null>(null);
  const [taskTimes, setTaskTimes] = useState<string[]>([]);
  const [idePath, setIdePath] = useState('');
  const [workPath, setWorkPath] = useState('');
  const [checkinHhmm, setCheckinHhmm] = useState('10:15');
  const [checkinGap, setCheckinGap] = useState(3);
  // 积分数据同步（对齐 BuddySettings 四模式）：mode + daily 时刻 + every_hours 间隔
  const [creditsForm, setCreditsForm] = useState({ mode: 'every_hours', hhmm: '23:40', hours: '2' });
  const [tokenRenewEnabled, setTokenRenewEnabled] = useState(true);
  // 会话流水定时合并（qoder-sessions-compact：每 6 小时合并早于保留窗的流水为会话行）
  const [compactEnabled, setCompactEnabled] = useState(true);
  const [compactDays, setCompactDays] = useState('3');
  const [refreshing, setRefreshing] = useState(false);
  const [saving, setSaving] = useState(false);
  // 计划任务注册/卸载互斥（防连点重复提交）
  const [taskBusy, setTaskBusy] = useState(false);
  // 卸载确认（删除类操作二次确认，对齐 Settings/BuddySettings 口径）
  const [confirmUnregister, setConfirmUnregister] = useState(false);

  /** qoder-settings 加载（签到开关数据源）：失败置错误态——null 时 checkbox 恒显默认 true
   *  且点击 no-op、保存会静默跳过 settingsSet 仍报「已保存」，必须显式拦截 */
  const loadQoderSettings = useCallback(async () => {
    try {
      setQoderSettings(await api.qoder.settingsGet());
      setSettingsErr(false);
    } catch {
      setQoderSettings(null);
      setSettingsErr(true);
    }
  }, []);

  const refresh = useCallback(async () => {
    setRefreshing(true);
    try {
      const [ts, e] = await Promise.all([
        api.qoder.checkinTaskStatus().catch(() => [] as string[]),
        api.qoder.envCheck().catch(() => null),
      ]);
      setTaskTimes(ts);
      setEnv(e);
      await loadQoderSettings();
    } catch (err) {
      pushToast('error', `检测失败：${String(err)}`);
    } finally {
      setRefreshing(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [loadQoderSettings]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // 全局 settings 任何刷新（保存后 refresh / 其他页面触发）都会触发本 effect；
  // 仅当输入框当前值仍等于上次同步的已保存值（用户未本地编辑）时才应用新 settings，
  // 避免覆盖输入框中未保存的修改
  const lastSynced = useRef({
    idePath: '',
    workPath: '',
    checkinHhmm: '10:15',
    checkinGap: 3,
    creditsForm: { mode: 'every_hours', hhmm: '23:40', hours: '2' },
    tokenRenewEnabled: true,
    compactEnabled: true,
    compactDays: '3',
  });

  useEffect(() => {
    if (!settings) return;
    const s = lastSynced.current;
    const untouched =
      idePath === s.idePath &&
      workPath === s.workPath &&
      checkinHhmm === s.checkinHhmm &&
      checkinGap === s.checkinGap &&
      creditsForm.mode === s.creditsForm.mode &&
      creditsForm.hhmm === s.creditsForm.hhmm &&
      creditsForm.hours === s.creditsForm.hours &&
      tokenRenewEnabled === s.tokenRenewEnabled &&
      compactEnabled === s.compactEnabled &&
      compactDays === s.compactDays;
    if (!untouched) return;
    const next = {
      idePath: settings.qoder_ide_path ?? '',
      workPath: settings.qoderwork_path ?? '',
      checkinHhmm: settings.qoder_checkin_hhmm || '10:15',
      checkinGap: settings.qoder_checkin_gap_secs ?? 3,
      creditsForm: {
        mode: settings.qoder_credits_sync_mode || 'every_hours',
        hhmm: settings.qoder_credits_sync_hhmm || '23:40',
        hours: settings.qoder_credits_sync_hours || '2',
      },
      tokenRenewEnabled: settings.qoder_token_renew_enabled ?? true,
      compactEnabled: settings.qoder_sessions_compact_enabled ?? true,
      compactDays: settings.qoder_sessions_compact_days || '3',
    };
    setIdePath(next.idePath);
    setWorkPath(next.workPath);
    setCheckinHhmm(next.checkinHhmm);
    setCheckinGap(next.checkinGap);
    setCreditsForm(next.creditsForm);
    setTokenRenewEnabled(next.tokenRenewEnabled);
    setCompactEnabled(next.compactEnabled);
    setCompactDays(next.compactDays);
    lastSynced.current = next;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [settings]);

  /** 右上角统一保存（对齐 BuddySettings）：路径 + 时刻 + 快照开关 + 签到行为，一次提交 */
  const save = async () => {
    // qoder-settings 未就绪（加载失败或仍在初始加载）时禁止保存：
    // 否则签到开关静默跳过 settingsSet，误报「配置已保存」而实际开关丢失
    if (settingsErr || !qoderSettings) {
      pushToast('error', 'qoder-settings 未就绪，签到开关暂不可保存；请点「重新检测」后重试');
      return;
    }
    if (!isValidHHMM(checkinHhmm)) {
      pushToast('error', `签到时刻格式无效：${checkinHhmm}（应为 HH:MM）`);
      return;
    }
    if (creditsForm.mode === 'daily' && !isValidHHMM(creditsForm.hhmm)) {
      pushToast('error', `同步时刻格式无效：${creditsForm.hhmm}（应为 HH:MM）`);
      return;
    }
    const compactDaysNum = String(
      Math.min(90, Math.max(1, Math.round(Number(compactDays) || 3))),
    );
    if (compactEnabled && !/^\d+$/.test(compactDays.trim())) {
      // 审查修复（L37）：空串时报错文案缺值，空值显示「（空）」
      pushToast('error', `合并保留窗天数无效：${compactDays.trim() || '（空）'}（应为 1-90 的整数）`);
      return;
    }
    const creditsHours = String(
      Math.min(24, Math.max(1, Math.round(Number(creditsForm.hours) || 2))),
    );
    setSaving(true);
    try {
      await withMinDelay(
        Promise.all([
          saveSettings({
            qoder_ide_path: idePath.trim() || null,
            qoderwork_path: workPath.trim() || null,
            qoder_checkin_hhmm: checkinHhmm.trim(),
            qoder_checkin_gap_secs: checkinGap,
            qoder_credits_sync_mode: creditsForm.mode,
            qoder_credits_sync_hhmm: creditsForm.hhmm.trim() || '23:40',
            qoder_credits_sync_hours: creditsHours,
            // 旧两态开关双写兼容：mode≠off 即启用（调度器已改按 mode 判定，此处仅保历史读取方）
            qoder_credits_sync_enabled: creditsForm.mode !== 'off',
            qoder_token_renew_enabled: tokenRenewEnabled,
            qoder_sessions_compact_enabled: compactEnabled,
            qoder_sessions_compact_days: compactDaysNum,
          }),
          api.qoder.settingsSet(qoderSettings),
        ]),
        800,
      );
      pushToast('success', '配置已保存');
      // 同步基线（审查 F1）：lastSynced 更新为本次保存的值（trim 口径与保存一致），
      // 否则后续全局 settings 刷新会因「输入框 ≠ 上次同步值」误判为用户已编辑而永不应用
      lastSynced.current = {
        idePath: idePath.trim(),
        workPath: workPath.trim(),
        checkinHhmm: checkinHhmm.trim(),
        checkinGap,
        creditsForm: { mode: creditsForm.mode, hhmm: creditsForm.hhmm.trim(), hours: creditsHours },
        tokenRenewEnabled,
        compactEnabled,
        compactDays: compactDaysNum,
      };
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
      await api.qoder.checkinTaskRegister([checkinHhmm.trim()]);
      setTaskTimes(await api.qoder.checkinTaskStatus());
      // 双轨提示：计划任务按输入框当前值注册即生效；应用内调度器仍以「保存配置」提交的值为准
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
      await api.qoder.checkinTaskUnregister();
      setTaskTimes(await api.qoder.checkinTaskStatus());
      pushToast('success', 'Windows 计划任务已卸载');
    } catch (err) {
      pushToast('error', `卸载失败：${String(err)}`);
    } finally {
      setTaskBusy(false);
    }
  };

  /** 自动检测：取本次环境检测结果填入输入框（对齐 Buddy「自动检测」交互），随右上角「保存配置」生效 */
  const detectPath = (target: 'ide' | 'work') => {
    const exe = target === 'ide' ? env?.ide_exe : env?.qoderwork_exe;
    if (exe) {
      if (target === 'ide') setIdePath(exe);
      else setWorkPath(exe);
      pushToast('info', `已定位：${exe}`);
    } else {
      pushToast('warn', '未检测到客户端，请人工填写 exe 路径');
    }
  };

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="Qoder · 环境配置"
        desc="通用配置 · 任务配置"
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

      {/* 双列布局（对齐 BuddySettings：左列 通用配置（应用环境+签到行为），右列 任务配置） */}
      <div className="grid items-start gap-4 lg:grid-cols-2">
        {/* 左列：通用配置（应用环境 + 签到行为） */}
        <div className="space-y-4">
          <div className="card p-4">
            <div className="mb-3 flex items-center gap-2">
              <SlidersHorizontal size={16} className="text-violet-500" />
              <h2 className="font-medium">通用配置</h2>
              <span className="text-xs text-slate-400">路径与时刻随右上角「保存配置」生效</span>
            </div>

            {/* 应用环境（原「客户端路径」改名：自动检测预填 + 人工可改，随「保存配置」生效） */}
            <h3 className="mb-2 font-medium">应用环境</h3>
            <div className="space-y-3">
              <div className="rounded-lg border border-slate-100 p-3 dark:border-zinc-800">
                <div className="mb-2 flex items-center justify-between font-medium text-slate-500">
                  Qoder CN IDE exe 路径
                  <Badge tone={env?.ide_exe ? 'green' : 'amber'}>{env?.ide_exe ? '已检测到' : '未检测到'}</Badge>
                </div>
                <div className="flex items-center gap-2">
                  <input
                    className="input flex-1 font-mono text-xs"
                    value={idePath}
                    onChange={(e) => setIdePath(e.target.value)}
                    placeholder={env?.ide_exe ?? 'C:\\Users\\...\\AppData\\Local\\Programs\\Qoder CN IDE\\Qoder CN IDE.exe'}
                  />
                  <button className="btn-outline shrink-0 !px-2 !py-1" onClick={() => detectPath('ide')}>
                    <Search size={13} /> 自动检测
                  </button>
                </div>
                <p className="mt-1.5 text-slate-400">M3 切换功能使用 · 留空 = 自动检测 · 随右上角「保存配置」生效</p>
              </div>
              <div className="rounded-lg border border-slate-100 p-3 dark:border-zinc-800">
                <div className="mb-2 flex items-center justify-between font-medium text-slate-500">
                  Qoder CN exe 路径
                  <Badge tone={env?.qoderwork_exe ? 'green' : 'amber'}>{env?.qoderwork_exe ? '已检测到' : '未检测到'}</Badge>
                </div>
                <div className="flex items-center gap-2">
                  <input
                    className="input flex-1 font-mono text-xs"
                    value={workPath}
                    onChange={(e) => setWorkPath(e.target.value)}
                    placeholder={env?.qoderwork_exe ?? 'C:\\Users\\...\\AppData\\Local\\Programs\\Qoder CN\\Qoder CN.exe'}
                  />
                  <button className="btn-outline shrink-0 !px-2 !py-1" onClick={() => detectPath('work')}>
                    <Search size={13} /> 自动检测
                  </button>
                </div>
                <p className="mt-1.5 text-slate-400">Work 本体拉起与已安装检测使用 · 留空 = 自动检测 · 随右上角「保存配置」生效</p>
              </div>
            </div>
            {/* 签到行为（并入通用配置卡内部分节） */}
            <div className="my-4 border-t border-slate-100 dark:border-zinc-800" />
            <h3 className="mb-2 font-medium">签到行为</h3>
            <div className="grid gap-3">
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
                  checked={qoderSettings?.auto_checkin ?? true}
                  onChange={(e) =>
                    setQoderSettings((s) => (s ? { ...s, auto_checkin: e.target.checked } : s))
                  }
                />
                <span className="text-sm">
                  自动签到（默认开）
                  <span className="block text-xs text-slate-400">
                    启动补签 + 应用内调度器 qoder-checkin 启用判定；多账号并发签到，
                    每账号绑定一份稳定设备指纹（账号页可查看），不同账号以不同设备身份请求，互不影响
                  </span>
                  {settingsErr && (
                    <span className="mt-1 block text-xs text-amber-600 dark:text-amber-400">
                      qoder-settings 加载失败，当前显示为默认值且不可修改；请点右上角「重新检测」重试
                    </span>
                  )}
                </span>
              </label>
            </div>
            {/* 网关上游开关已收口至 Qoder「资源调度」页（本页不再重复配置） */}
          </div>
        </div>

        {/* 右列：任务配置（原「调度」卡改名，对齐 BuddySettings TaskConfigCard 分节样式） */}
        <div className="card p-4">
          <div className="mb-3 flex items-center gap-2">
            <ListChecks size={16} className="text-violet-500" />
            <h2 className="font-medium">任务配置</h2>
            <span className="text-xs text-slate-400">应用内调度器 + Windows 计划任务双轨</span>
          </div>

          {/* Token 定时续期（qoder-refresh 内置调度任务，2026-10-02 从「无 UI 恒开」升级为可配置） */}
          <div className="flex items-center justify-between">
            <h3 className="font-medium">Token 定时续期</h3>
            <span className="text-xs text-slate-400">开关随右上角「保存配置」生效</span>
          </div>
          <p className="mb-3 mt-1 text-xs text-slate-400">
            应用内调度器每 6 小时为全部含刷新凭证的账号自动续期登录凭证（如 JWT Token）；
            客户端令牌惰性窗 7 小时大于 6 小时调度间隔，过期前必被续上。关闭后凭证仅在
            实际使用（余额刷新 / 签到 / 网关调用）时惰性刷新。无账号时空转不计失败。
          </p>
          <div className="flex flex-wrap items-center gap-4 rounded-lg border border-slate-100 p-3 dark:border-zinc-800">
            <label className="flex items-center gap-2 text-sm font-medium">
              <input
                type="checkbox"
                checked={tokenRenewEnabled}
                onChange={(e) => setTokenRenewEnabled(e.target.checked)}
              />
              启用
            </label>
            <span className="text-xs text-slate-400">内置任务 · 每 6 小时 · 无需注册计划任务</span>
          </div>

          <div className="my-4 border-t border-slate-100 dark:border-zinc-800" />

          {/* 每日自动签到（对齐 Buddy「Windows 计划任务」盒子样式） */}
          <div className="rounded-lg border border-slate-100 p-3 dark:border-zinc-800">
            <h3 className="mb-2 font-medium">每日自动签到</h3>
            <div className="mb-2 text-xs text-slate-400">
              应用内调度器到点自动执行（默认 10:15，同时覆盖「0 点签到」与「10:00 登录奖励」双活动），
              应用启动时自动补跑当日已过时刻；下方 Windows 计划任务作为兜底，应用未启动时直接运行。
            </div>
            <div className="mb-2 flex flex-wrap items-center gap-2">
              <span className="text-xs font-medium text-slate-500">账号间隔</span>
              <input
                type="number"
                min={0}
                max={600}
                value={checkinGap}
                onChange={(e) => {
                  if (e.target.value === '') return; // 清空输入中间态不落值
                  const n = Math.min(600, Math.max(0, Math.floor(Number(e.target.value))));
                  setCheckinGap(Number.isFinite(n) ? n : 3);
                }}
                className="input h-9 !w-20 text-sm"
              />
              <span className="text-xs text-slate-400">
                秒（多账号串行签到的间隔，默认 3 秒防频控，0 = 关闭）
              </span>
            </div>
            <div className="flex flex-wrap items-center justify-between gap-2">
              <div>
                <div className="text-sm font-medium">Windows 计划任务（兜底）</div>
                <div className="text-xs text-slate-400">
                  {taskTimes.length > 0 ? `已注册：${taskTimes.join('、')}` : '未注册'}
                </div>
              </div>
              <div className="flex items-center gap-2">
                <input
                  type="time"
                  className="input h-9 !w-28 text-sm"
                  value={checkinHhmm}
                  onChange={(e) => setCheckinHhmm(e.target.value || '10:15')}
                />
                {taskTimes.length > 0 ? (
                  <button
                    className="btn-ghost !px-3 !py-1 text-xs text-rose-500"
                    disabled={taskBusy}
                    onClick={() => setConfirmUnregister(true)}
                  >
                    {taskBusy ? <Spinner /> : null} 卸载
                  </button>
                ) : (
                  <button
                    className="btn-outline !px-3 !py-1 text-xs"
                    disabled={taskBusy}
                    onClick={() => void registerTask()}
                  >
                    {taskBusy ? <Spinner /> : null} 注册
                  </button>
                )}
              </div>
            </div>
          </div>

          <div className="my-4 border-t border-slate-100 dark:border-zinc-800" />

          {/* 积分数据同步（对齐 Buddy 设计）：四模式定时同步看板服务端数据 */}
          <div className="flex items-center justify-between">
            <h3 className="font-medium">积分数据同步</h3>
            <span className="text-xs text-slate-400">模式与时刻随右上角「保存配置」生效</span>
          </div>
          <p className="mb-3 mt-1 text-xs text-slate-400">
            应用运行期间按所选模式自动同步积分看板的服务端数据（每日拉取全部账号余额写入快照，积分看板趋势数据源）；
            无账号时静默跳过不计失败，失败 30 分钟后自动重试；应用关闭期间不执行。
          </p>
          <div className="flex flex-wrap items-center gap-4 rounded-lg border border-slate-100 p-3 dark:border-zinc-800">
            <label className="flex items-center gap-2 text-sm">
              <span className="text-xs font-medium text-slate-500">同步模式</span>
              <select
                className="input h-9 !w-32 text-sm"
                value={creditsForm.mode}
                onChange={(e) => setCreditsForm({ ...creditsForm, mode: e.target.value })}
              >
                <option value="every_hours">每N小时</option>
                <option value="daily">每日定时</option>
                <option value="hourly">每小时</option>
                <option value="off">关闭</option>
              </select>
            </label>
            {creditsForm.mode === 'every_hours' && (
              <div className="flex items-center gap-2">
                <span className="text-xs font-medium text-slate-500">同步间隔</span>
                <input
                  type="number"
                  min={1}
                  max={24}
                  value={creditsForm.hours}
                  onChange={(e) => setCreditsForm({ ...creditsForm, hours: e.target.value })}
                  onBlur={() => {
                    // 清空为合法中间态（onChange 不强制回填），失焦时归一到 1-24 落值
                    const n = Math.min(24, Math.max(1, Math.round(Number(creditsForm.hours) || 2)));
                    setCreditsForm({ ...creditsForm, hours: String(n) });
                  }}
                  className="input h-9 !w-20 text-sm"
                />
                <span className="text-xs text-slate-400">
                  小时（1-24）；距上次成功同步满 {creditsForm.hours || '2'} 小时自动执行一次
                </span>
              </div>
            )}
            {creditsForm.mode === 'daily' && (
              <div className="flex items-center gap-2">
                <span className="text-xs font-medium text-slate-500">每日执行时刻</span>
                <input
                  type="time"
                  value={creditsForm.hhmm}
                  onChange={(e) => setCreditsForm({ ...creditsForm, hhmm: e.target.value || '23:40' })}
                  className="input h-9 !w-28 text-sm"
                />
                <span className="text-xs text-slate-400">每天 {creditsForm.hhmm || '23:40'} 执行（应用关闭期间不执行）</span>
              </div>
            )}
            {creditsForm.mode === 'hourly' && (
              <span className="text-xs text-slate-400">应用运行期间每小时同步一次</span>
            )}
          </div>

          <div className="my-4 border-t border-slate-100 dark:border-zinc-800" />

          {/* 会话流水定时合并：每 6 小时把「当天之前」的请求级流水按会话合并为单行 */}
          <div className="flex items-center justify-between">
            <h3 className="font-medium">会话流水定时合并</h3>
            <span className="text-xs text-slate-400">开关随右上角「保存配置」生效</span>
          </div>
          <p className="mb-3 mt-1 text-xs text-slate-400">
            会话明细按请求级落库（一个会话会产生数十条请求记录）。应用内调度器每 6 小时将
            早于保留窗的流水按「日期 + 账号 + 会话说明」合并为单行（积分与 token 合计不变），
            压缩流水表体积；保留窗内保留请求级明细，看板展示由查询即时聚合，无需等待合并。
            合并幂等，重复执行不会重复累计。
          </p>
          <div className="flex flex-wrap items-center gap-4 rounded-lg border border-slate-100 p-3 dark:border-zinc-800">
            <label className="flex items-center gap-2 text-sm font-medium">
              <input
                type="checkbox"
                checked={compactEnabled}
                onChange={(e) => setCompactEnabled(e.target.checked)}
              />
              启用
            </label>
            <div className="flex items-center gap-2">
              <span className="text-xs font-medium text-slate-500">保留窗</span>
              <input
                type="number"
                min={1}
                max={90}
                value={compactDays}
                onChange={(e) => setCompactDays(e.target.value)}
                className="input h-9 !w-20 text-sm"
              />
              <span className="text-xs text-slate-400">
                天（1-90）；近 {compactDays || '3'} 天保留请求级明细，更早的合并为会话行
              </span>
            </div>
            <span className="text-xs text-slate-400">内置任务 · 每 6 小时 · 无需注册计划任务</span>
          </div>
        </div>
      </div>

      {/* 卸载确认弹窗（删除类操作二次确认） */}
      <Modal
        open={confirmUnregister}
        onClose={() => setConfirmUnregister(false)}
        title="确认卸载定时签到任务"
        footer={
          <>
            <button className="btn-outline" onClick={() => setConfirmUnregister(false)}>取消</button>
            <button className="btn-danger" onClick={() => { setConfirmUnregister(false); void unregisterTask(); }}>确认卸载</button>
          </>
        }
      >
        <div className="flex items-start gap-3">
          <AlertTriangle size={20} className="mt-0.5 shrink-0 text-amber-500" />
          <div>
            <p>确认卸载 Windows 定时签到计划任务{taskTimes.length > 0 ? `（${taskTimes.join('、')}）` : ''}？</p>
            <p className="mt-2 text-xs text-slate-400">
              卸载后应用未启动时将不再有签到兜底；应用内置调度不受影响，之后可随时重新注册。
            </p>
          </div>
        </div>
      </Modal>
    </div>
  );
}
