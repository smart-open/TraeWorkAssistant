import { useCallback, useEffect, useRef, useState } from 'react';
import { ListChecks, RefreshCw, Save, Search, ShieldAlert, SlidersHorizontal } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { Badge, Spinner } from '../../components/ui';
import { withMinDelay } from '../../lib/delay';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import type { QoderEnvCheck, QoderSettings } from '../../types';

/**
 * qoder-settings 环境配置（F-80 §5.8，布局对齐 BuddySettings）：
 * 左列 通用配置（应用环境路径 + 签到行为）；右列 任务配置（每日自动签到 + Token 定时续期 + 积分数据同步）。
 * 顶部「重新检测」+ 右上角「保存配置」统一提交（路径 / 时刻 / 开关一处生效）；
 * 路径自动检测为独立即时动作；每日定时统一走应用内 Rust 调度器（无系统计划任务）。
 * 合规提示（条款风险固定展示，不可跳过）。
 */

const isValidHHMM = (s: string) => /^([01]\d|2[0-3]):[0-5]\d$/.test(s.trim());

export default function QoderSettings() {
  const pushToast = useAppStore((s) => s.pushToast);
  const platform = useAppStore((s) => s.platform);
  const settings = useAppStore((s) => s.settings);
  const saveSettings = useAppStore((s) => s.saveSettings);
  const [qoderSettings, setQoderSettings] = useState<QoderSettings | null>(null);
  const [settingsErr, setSettingsErr] = useState(false);
  const [env, setEnv] = useState<QoderEnvCheck | null>(null);
  const [idePath, setIdePath] = useState('');
  const [workPath, setWorkPath] = useState('');
  const [checkinHhmm, setCheckinHhmm] = useState('10:15');
  const [creditsHhmm, setCreditsHhmm] = useState('23:40');
  const [creditsSyncEnabled, setCreditsSyncEnabled] = useState(true);
  const [tokenRenewEnabled, setTokenRenewEnabled] = useState(true);
  // number | ''：'' 为「清空重输中」暂存态，保存侧 1~24 校验兜底
  const [renewHours, setRenewHours] = useState<number | ''>(6);
  const [refreshing, setRefreshing] = useState(false);
  const [saving, setSaving] = useState(false);

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
      const e = await api.qoder.envCheck().catch(() => null);
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
    creditsHhmm: '23:40',
    creditsSyncEnabled: true,
    tokenRenewEnabled: true,
    renewHours: 6,
  });

  useEffect(() => {
    if (!settings) return;
    const s = lastSynced.current;
    const untouched =
      idePath === s.idePath &&
      workPath === s.workPath &&
      checkinHhmm === s.checkinHhmm &&
      creditsHhmm === s.creditsHhmm &&
      creditsSyncEnabled === s.creditsSyncEnabled &&
      tokenRenewEnabled === s.tokenRenewEnabled &&
      renewHours === s.renewHours;
    if (!untouched) return;
    const next = {
      idePath: settings.qoder_ide_path ?? '',
      workPath: settings.qoderwork_path ?? '',
      checkinHhmm: settings.qoder_checkin_hhmm || '10:15',
      creditsHhmm: settings.qoder_credits_sync_hhmm || '23:40',
      creditsSyncEnabled: settings.qoder_credits_sync_enabled ?? true,
      tokenRenewEnabled: settings.qoder_token_renew_enabled ?? true,
      renewHours: settings.qoder_token_renew_interval_hours || 6,
    };
    setIdePath(next.idePath);
    setWorkPath(next.workPath);
    setCheckinHhmm(next.checkinHhmm);
    setCreditsHhmm(next.creditsHhmm);
    setCreditsSyncEnabled(next.creditsSyncEnabled);
    setTokenRenewEnabled(next.tokenRenewEnabled);
    setRenewHours(next.renewHours);
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
    if (!isValidHHMM(creditsHhmm)) {
      pushToast('error', `快照时刻格式无效：${creditsHhmm}（应为 HH:MM）`);
      return;
    }
    // Token 续期间隔：整数 1~24（越界钳制 + 提示，不静默拦截保存）
    const hours = Math.round(Number(renewHours));
    if (!Number.isFinite(hours) || hours < 1 || hours > 24) {
      pushToast('error', `续期间隔无效：${renewHours}（应为 1~24 的整数小时）`);
      return;
    }
    setSaving(true);
    try {
      await withMinDelay(
        Promise.all([
          saveSettings({
            qoder_ide_path: idePath.trim() || null,
            qoderwork_path: workPath.trim() || null,
            qoder_checkin_hhmm: checkinHhmm.trim(),
            qoder_credits_sync_hhmm: creditsHhmm.trim(),
            qoder_credits_sync_enabled: creditsSyncEnabled,
            qoder_token_renew_enabled: tokenRenewEnabled,
            qoder_token_renew_interval_hours: hours,
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
        creditsHhmm: creditsHhmm.trim(),
        creditsSyncEnabled,
        tokenRenewEnabled,
        renewHours: hours,
      };
      setRenewHours(hours);
      await refresh();
    } catch (err) {
      pushToast('error', `保存失败：${String(err)}`);
    } finally {
      setSaving(false);
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
      pushToast('warn', `未检测到客户端，请人工填写${platform === 'windows' ? ' exe' : ' .app'}路径`);
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
                  {platform === 'windows' ? 'Qoder CN IDE exe 路径' : 'Qoder CN IDE 路径（.app）'}
                  <Badge tone={env?.ide_exe ? 'green' : 'amber'}>{env?.ide_exe ? '已检测到' : '未检测到'}</Badge>
                </div>
                <div className="flex items-center gap-2">
                  <input
                    className="input flex-1 font-mono text-xs"
                    value={idePath}
                    onChange={(e) => setIdePath(e.target.value)}
                    placeholder={
                      env?.ide_exe ??
                      (platform === 'windows'
                        ? 'C:\\Users\\...\\AppData\\Local\\Programs\\Qoder CN IDE\\Qoder CN IDE.exe'
                        : '/Applications/Qoder CN IDE.app')
                    }
                  />
                  <button className="btn-outline shrink-0 !px-2 !py-1" onClick={() => detectPath('ide')}>
                    <Search size={13} /> 自动检测
                  </button>
                </div>
                <p className="mt-1.5 text-slate-400">M3 切换功能使用 · 留空 = 自动检测 · 随右上角「保存配置」生效</p>
              </div>
              <div className="rounded-lg border border-slate-100 p-3 dark:border-zinc-800">
                <div className="mb-2 flex items-center justify-between font-medium text-slate-500">
                  {platform === 'windows' ? 'QoderWork CN exe 路径' : 'QoderWork CN 路径（.app）'}
                  <Badge tone={env?.qoderwork_exe ? 'green' : 'amber'}>{env?.qoderwork_exe ? '已检测到' : '未检测到'}</Badge>
                </div>
                <div className="flex items-center gap-2">
                  <input
                    className="input flex-1 font-mono text-xs"
                    value={workPath}
                    onChange={(e) => setWorkPath(e.target.value)}
                    placeholder={
                      env?.qoderwork_exe ??
                      (platform === 'windows'
                        ? 'C:\\Users\\...\\AppData\\Local\\Programs\\Qoder CN\\Qoder CN.exe'
                        : '/Applications/Qoder CN.app')
                    }
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
            <span className="text-xs text-slate-400">应用内调度器（全平台一致）</span>
          </div>

          {/* Token 定时续期（qoder-refresh 内置调度任务；2026-10-05 间隔可配） */}
          <div className="flex items-center justify-between">
            <h3 className="font-medium">Token 定时续期</h3>
            <span className="text-xs text-slate-400">开关与间隔随右上角「保存配置」生效</span>
          </div>
          <p className="mb-3 mt-1 text-xs text-slate-400">
            应用内调度器每 N 小时为全部含刷新凭证的账号自动续期登录凭证（如 JWT Token）；
            客户端令牌惰性窗 7 小时，间隔不超过 7 小时即可保证过期令牌被续上。关闭后凭证仅在
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
            <div className="flex items-center gap-2">
              <span className="text-xs font-medium text-slate-500">执行间隔</span>
              <input
                type="number"
                min={1}
                max={24}
                step={1}
                className="input h-9 !w-20 text-sm"
                value={renewHours}
                onChange={(e) =>
                  // 空串原样暂存（允许清空重输，终审修复「清空即跳 6 造成 6→61 粘连」）；
                  // 保存时由 1~24 fail-closed 校验兜底
                  setRenewHours(e.target.value === '' ? '' : Number(e.target.value))
                }
              />
              <span className="text-xs text-slate-400">小时（1~24，默认 6）</span>
            </div>
          </div>

          <div className="my-4 border-t border-slate-100 dark:border-zinc-800" />

          {/* 每日自动签到（应用内 Rust 调度器单轨：执行时刻随输入框 + 右上角「保存配置」生效） */}
          <div className="rounded-lg border border-slate-100 p-3 dark:border-zinc-800">
            <h3 className="mb-2 font-medium">每日自动签到</h3>
            <div className="mb-2 text-xs text-slate-400">
              应用内调度器到点自动执行（默认 10:15，同时覆盖「0 点签到」与「10:00 登录奖励」双活动），
              应用启动时自动补跑当日已过时刻；执行时刻随下方输入框保存后生效。
            </div>
            <div className="flex flex-wrap items-center justify-between gap-2">
              <div>
                <div className="text-sm font-medium">每日执行时刻</div>
                <div className="text-xs text-slate-400">应用关闭期间不执行，启动后自动补跑</div>
              </div>
              <input
                type="time"
                className="input h-9 !w-28 text-sm"
                value={checkinHhmm}
                onChange={(e) => setCheckinHhmm(e.target.value || '10:15')}
              />
            </div>
          </div>

          <div className="my-4 border-t border-slate-100 dark:border-zinc-800" />

          {/* 积分数据同步（原「积分快照」改名：定时同步积分看板的服务端数据） */}
          <div className="flex items-center justify-between">
            <h3 className="font-medium">积分数据同步</h3>
            <span className="text-xs text-slate-400">开关与时刻随右上角「保存配置」生效</span>
          </div>
          <p className="mb-3 mt-1 text-xs text-slate-400">
            定时同步积分看板的服务端数据：每日拉取全部账号余额写入快照（积分看板趋势数据源）；
            无账号时空转不计失败。应用关闭期间不执行。
          </p>
          <div className="flex flex-wrap items-center gap-4 rounded-lg border border-slate-100 p-3 dark:border-zinc-800">
            <label className="flex items-center gap-2 text-sm font-medium">
              <input
                type="checkbox"
                checked={creditsSyncEnabled}
                onChange={(e) => setCreditsSyncEnabled(e.target.checked)}
              />
              启用
            </label>
            <div className="flex items-center gap-2">
              <span className="text-xs font-medium text-slate-500">每日执行时刻</span>
              <input
                type="time"
                className="input h-9 !w-28 text-sm"
                value={creditsHhmm}
                onChange={(e) => setCreditsHhmm(e.target.value || '23:40')}
              />
              <span className="text-xs text-slate-400">默认 23:40</span>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}
