import { useEffect, useRef, useState } from 'react';
import { FolderSearch, Save, ShieldCheck, RefreshCw, Timer, Coins, CalendarCheck, SlidersHorizontal, ListChecks } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { Badge } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import type { AppLocate, DoubaoRenewSummary } from '../../types';

/** 豆包环境配置：两列布局（对齐 Trae 环境配置页）——
 *  左列「通用配置」（应用位置 / 快照设置 / 接口端点），右列「任务配置」（会话保活 / 额度巡检 / 每日签到）。 */
export default function DoubaoSettings() {
  const settings = useAppStore((s) => s.settings);
  const saveSettings = useAppStore((s) => s.saveSettings);
  const pushToast = useAppStore((s) => s.pushToast);

  const [locate, setLocate] = useState<AppLocate | null>(null);
  const [path, setPath] = useState('');
  const [detecting, setDetecting] = useState(false);
  const [saving, setSaving] = useState(false);

  // ── 会话保活 ──
  const [renewUrl, setRenewUrl] = useState('');
  const [taskTime, setTaskTime] = useState('09:00');
  const [taskState, setTaskState] = useState<'loading' | 'registered' | 'not_registered' | 'error'>('loading');
  const [taskTimeShown, setTaskTimeShown] = useState('');
  const [running, setRunning] = useState(false);
  const [summary, setSummary] = useState<DoubaoRenewSummary | null>(null);

  // ── 会员额度 ──
  const [quotaUrl, setQuotaUrl] = useState('');
  // ── 额度巡检定时任务 ──
  const [quotaTaskTime, setQuotaTaskTime] = useState('09:30');
  const [quotaTaskState, setQuotaTaskState] = useState<'loading' | 'registered' | 'not_registered' | 'error'>('loading');
  const [quotaTaskTimeShown, setQuotaTaskTimeShown] = useState('');

  // ── 快照设置（C4：IndexedDB 可选纳入）──
  const [includeIdb, setIncludeIdb] = useState(false);

  // ── 应用内调度开关（issue #147，默认开）──
  const [keepaliveEnabled, setKeepaliveEnabled] = useState(true);
  const [quotaEnabled, setQuotaEnabled] = useState(true);

  // ── 每日签到任务 ──
  const [checkinTaskTime, setCheckinTaskTime] = useState('09:15');
  const [checkinTaskState, setCheckinTaskState] = useState<'loading' | 'registered' | 'not_registered' | 'error'>('loading');
  const [checkinTaskTimeShown, setCheckinTaskTimeShown] = useState('');
  // 应用内每日签到的签到时刻（单源 settings.doubao_checkin_hhmm）：纯应用内调度（不注册 schtasks）用户的修改入口，
  // 与上方 checkinTaskTime（Windows 计划任务注册时刻）是两个独立输入框
  const [checkinHhmm, setCheckinHhmm] = useState('09:15');
  // 回写打回防护：checkinTaskTime 编辑期间（脏标记或输入框聚焦中）refresh 不回填覆盖，注册成功后复位恢复收敛
  const checkinTimeDirtyRef = useRef(false);
  const checkinTaskTimeInputRef = useRef<HTMLInputElement>(null);
  // ── 豆包数据同步（额度巡检定时远端拉取 → 池回写；页面读本地） ──
  const [syncMode, setSyncMode] = useState('every_hours');
  const [syncHours, setSyncHours] = useState('2');
  const [syncHhmm, setSyncHhmm] = useState('23:50');
  const [checkinRunning, setCheckinRunning] = useState(false);
  const [checkinEnabled, setCheckinEnabled] = useState(true);
  const [probeAutoKeepalive, setProbeAutoKeepalive] = useState(true);

  const toggleIncludeIdb = async (v: boolean) => {
    setIncludeIdb(v);
    try {
      await saveSettings({ doubao_snapshot_include_idb: v });
      pushToast(
        'success',
        v
          ? '已开启：之后的保存/切换会纳入 IndexedDB（对话历史随账号迁移，快照体积会明显增大）；已有快照需重新保存登录态后生效'
          : '已关闭：快照恢复默认白名单（不含 IndexedDB）',
      );
    } catch (err) {
      setIncludeIdb(!v);
      pushToast('error', `保存设置失败：${String(err)}`);
    }
  };

  // 应用内调度器 doubao-keepalive 开关（issue #147）：不影响已注册的 Windows 计划任务
  const toggleKeepalive = async (v: boolean) => {
    setKeepaliveEnabled(v);
    try {
      await saveSettings({ doubao_keepalive_enabled: v });
      pushToast(
        'success',
        v ? '已开启：应用内调度恢复每日会话续期' : '已关闭：应用内调度不再每日拉起豆包执行续期（不影响已注册的 Windows 计划任务）',
      );
    } catch (err) {
      setKeepaliveEnabled(!v);
      pushToast('error', `保存设置失败：${String(err)}`);
    }
  };

  // 应用内调度器 doubao-quota 开关（issue #147）
  const toggleQuotaEnabled = async (v: boolean) => {
    setQuotaEnabled(v);
    try {
      await saveSettings({ doubao_quota_enabled: v });
      pushToast(
        'success',
        v ? '已开启：应用内调度恢复每日额度巡检' : '已关闭：应用内调度不再执行每日额度巡检（不影响已注册的 Windows 计划任务）',
      );
    } catch (err) {
      setQuotaEnabled(!v);
      pushToast('error', `保存设置失败：${String(err)}`);
    }
  };

  // 应用内调度器 doubao-checkin 开关（默认开）
  const toggleCheckinEnabled = async (v: boolean) => {
    setCheckinEnabled(v);
    try {
      await saveSettings({ doubao_checkin_enabled: v });
      pushToast(
        'success',
        v ? '已开启：应用内调度恢复每日自动签到' : '已关闭：应用内调度不再执行每日自动签到（不影响已注册的 Windows 计划任务）',
      );
    } catch (err) {
      setCheckinEnabled(!v);
      pushToast('error', `保存设置失败：${String(err)}`);
    }
  };

  // probe 自动保活开关（方案 A，2026-11-06，默认开）：签到前 probe 缺失/过期时自动
  // 开代理 + 切客户端抓包对齐（710010702 设备指纹缺失的自愈链）
  const toggleProbeAutoKeepalive = async (v: boolean) => {
    setProbeAutoKeepalive(v);
    try {
      await saveSettings({ doubao_probe_auto_keepalive: v });
      pushToast(
        'success',
        v
          ? '已开启：签到前自动抓包对齐（代理未运行会自动启动；豆包客户端会被短暂切换账号）'
          : '已关闭：probe 缺失/过期时签到将回退内置参数（可能被上游风控拒绝 710010702）',
      );
    } catch (err) {
      setProbeAutoKeepalive(!v);
      pushToast('error', `保存设置失败：${String(err)}`);
    }
  };

  // 豆包数据同步（doubao-sync）：模式 + 间隔/时刻一并保存；后端归一化（间隔 clamp 1..24）。
  // 审查 #18 补齐（对齐 changeCheckinHhmm 范式）：三个 sync 孪生 handler 共用
  // 并发序号守卫（失焦连发时仅最后请求可提示成功/回滚）+ 失败回滚输入框
  const syncConfigSeqRef = useRef(0);
  const saveSyncConfig = async (patch: { doubao_sync_mode?: string; doubao_sync_hours?: string; doubao_sync_hhmm?: string }) => {
    try {
      await saveSettings(patch);
      return true;
    } catch {
      // store.saveSettings 失败时已 pushToast 错误详情，此处不再双弹
      return false;
    }
  };
  const changeSyncMode = async (v: string) => {
    setSyncMode(v);
    const seq = ++syncConfigSeqRef.current;
    if (await saveSyncConfig({ doubao_sync_mode: v })) {
      if (seq === syncConfigSeqRef.current) pushToast('success', v === 'off' ? '已关闭豆包数据同步' : '同步模式已保存');
    } else if (seq === syncConfigSeqRef.current) {
      setSyncMode(settings?.doubao_sync_mode || 'every_hours');
    }
  };
  const changeSyncHours = async (v: string) => {
    setSyncHours(v);
    // 严格 /^\d+$/ 校验（审查 M-7⑤）：parseInt 会放行 "2x" 类输入（静默按 2 保存）。
    // 校验失败回弹已保存值（store.settings 即最后成功落库值，审查 #18）——输入框
    // 不驻留非法草稿，与「已保持原值」toast 口径一致
    const n = /^\d+$/.test(v.trim()) ? Number.parseInt(v.trim(), 10) : NaN;
    if (!Number.isFinite(n) || n < 1 || n > 24) {
      pushToast('warn', '同步间隔应为 1-24 的整数（已保持原值）');
      setSyncHours(settings?.doubao_sync_hours || '2');
      return;
    }
    const seq = ++syncConfigSeqRef.current;
    if (await saveSyncConfig({ doubao_sync_hours: String(n) })) {
      if (seq === syncConfigSeqRef.current) pushToast('success', `同步间隔已保存（每 ${n} 小时）`);
    } else if (seq === syncConfigSeqRef.current) {
      setSyncHours(settings?.doubao_sync_hours || '2');
    }
  };
  const changeSyncHhmm = async (v: string) => {
    setSyncHhmm(v);
    if (!/^([01]\d|2[0-3]):([0-5]\d)$/.test(v)) {
      pushToast('warn', '时间格式应为 HH:MM（如 23:50，已保持原值）');
      // 校验失败回弹已保存值：输入框不驻留非法草稿（审查 M-7①）
      setSyncHhmm(settings?.doubao_sync_hhmm || '23:50');
      return;
    }
    const seq = ++syncConfigSeqRef.current;
    if (await saveSyncConfig({ doubao_sync_hhmm: v })) {
      if (seq === syncConfigSeqRef.current) pushToast('success', `同步时刻已保存（每日 ${v}）`);
    } else if (seq === syncConfigSeqRef.current) {
      setSyncHhmm(settings?.doubao_sync_hhmm || '23:50');
    }
  };
  // 应用内签到时刻（单源 doubao_checkin_hhmm）：交互对齐 syncHhmm（onChange 暂存、失焦校验后保存）。
  // 审查 #18：失败回滚 UI 时刻（对齐同文件开关的回滚模式）+ 并发序号守卫（失焦连发时
  // 仅最后请求可提示成功/回滚，避免慢请求迟到覆盖新值）。
  // 回滚目标 = store.settings（最后成功落库值；saveSettings 失败时已先回滚 store
  // 再 rethrow，此处读到的是落库值而非 UI 草稿，连续失败不漂移，审查 M-7④）
  const checkinHhmmSeqRef = useRef(0);
  const changeCheckinHhmm = async (v: string) => {
    setCheckinHhmm(v);
    if (!/^([01]\d|2[0-3]):([0-5]\d)$/.test(v)) {
      pushToast('warn', '时间格式应为 HH:MM（如 09:15，已保持原值）');
      // 校验失败回弹已保存值：输入框不驻留非法草稿（审查 M-7①）
      setCheckinHhmm(settings?.doubao_checkin_hhmm || '09:15');
      return;
    }
    const seq = ++checkinHhmmSeqRef.current;
    try {
      await saveSettings({ doubao_checkin_hhmm: v });
      if (seq === checkinHhmmSeqRef.current) pushToast('success', `应用内签到时刻已保存（每日 ${v}）`);
    } catch {
      // store.saveSettings 失败时已 pushToast 错误详情；此处回滚 UI 时刻并阻断成功提示
      if (seq === checkinHhmmSeqRef.current) setCheckinHhmm(settings?.doubao_checkin_hhmm || '09:15');
    }
  };

  const refreshTaskStatus = async () => {
    try {
      const s = await api.doubao.taskStatus();
      if (s.startsWith('registered:')) {
        setTaskState('registered');
        setTaskTimeShown(s.slice('registered:'.length));
      } else {
        setTaskState('not_registered');
      }
    } catch {
      setTaskState('error'); // 查询失败 ≠ 未注册：置 error 态避免误导用户重复注册
    }
  };

  const refreshQuotaTaskStatus = async () => {
    try {
      const s = await api.doubao.quotaTaskStatus();
      if (s.startsWith('registered:')) {
        setQuotaTaskState('registered');
        setQuotaTaskTimeShown(s.slice('registered:'.length));
      } else {
        setQuotaTaskState('not_registered');
      }
    } catch {
      setQuotaTaskState('error'); // 同上：后端故障与未注册区分展示
    }
  };

  const refreshCheckinTaskStatus = async () => {
    try {
      const s = await api.doubao.checkinTaskStatus();
      if (s.startsWith('registered:')) {
        const shown = s.slice('registered:'.length);
        setCheckinTaskState('registered');
        setCheckinTaskTimeShown(shown);
        // 输入框对齐真实注册时刻（2026-10-31 修复：此前读 settings 默认值 09:15，
        // 与 schtasks 实际注册时刻（如 15:00）各说各话）；settings 漂移时一并收敛
        if (/^([01]\d|2[0-3]):([0-5]\d)$/.test(shown)) {
          // 用户正在编辑注册时刻（脏标记或输入框聚焦中）时不打回；注册成功复位脏标记后恢复收敛
          if (!checkinTimeDirtyRef.current && document.activeElement !== checkinTaskTimeInputRef.current) {
            setCheckinTaskTime(shown);
            if (settings?.doubao_checkin_hhmm !== shown) {
              void saveSettings({ doubao_checkin_hhmm: shown });
            }
          }
        }
      } else {
        setCheckinTaskState('not_registered');
      }
    } catch {
      setCheckinTaskState('error'); // 同上：后端故障与未注册区分展示
    }
  };

  // 审查修复：effect 体内回填的文本草稿仅在对应 settings 字段实际变化时才写回——
  // 依赖数组里的开关（本页改动即保存）变化触发 effect 重跑时，不得清掉其他输入框
  // 未保存的草稿（如「额度接口」打字未保存时翻动保活开关，无条件回填会打回旧值）
  const prevTextSettingsRef = useRef<Record<string, string | null>>({});

  useEffect(() => {
    const prev = prevTextSettingsRef.current;
    const cur: Record<string, string | null> = {
      path: settings?.doubao_path ?? null,
      renew_url: settings?.doubao_renew_url ?? null,
      quota_url: settings?.doubao_quota_url ?? null,
      sync_mode: settings?.doubao_sync_mode ?? null,
      sync_hours: settings?.doubao_sync_hours ?? null,
      sync_hhmm: settings?.doubao_sync_hhmm ?? null,
      checkin_hhmm: settings?.doubao_checkin_hhmm ?? null,
    };
    if (cur.path && cur.path !== prev.path) setPath(cur.path);
    if (cur.renew_url && cur.renew_url !== prev.renew_url) setRenewUrl(cur.renew_url);
    if (cur.quota_url && cur.quota_url !== prev.quota_url) setQuotaUrl(cur.quota_url);
    if (settings) {
      setIncludeIdb(!!settings.doubao_snapshot_include_idb);
      // !== false：旧版后端缺字段时按默认开展示（issue #147 新字段向后兼容）
      setKeepaliveEnabled(settings.doubao_keepalive_enabled !== false);
      setQuotaEnabled(settings.doubao_quota_enabled !== false);
      setCheckinEnabled(settings.doubao_checkin_enabled !== false);
      setProbeAutoKeepalive(settings.doubao_probe_auto_keepalive !== false);
      if (cur.sync_mode && cur.sync_mode !== prev.sync_mode) setSyncMode(cur.sync_mode);
      if (cur.sync_hours && cur.sync_hours !== prev.sync_hours) setSyncHours(cur.sync_hours);
      if (cur.sync_hhmm && cur.sync_hhmm !== prev.sync_hhmm) setSyncHhmm(cur.sync_hhmm);
      if (cur.checkin_hhmm && cur.checkin_hhmm !== prev.checkin_hhmm) setCheckinHhmm(cur.checkin_hhmm);
    }
    prevTextSettingsRef.current = cur;
    void refreshTaskStatus();
    void refreshQuotaTaskStatus();
    void refreshCheckinTaskStatus();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [settings?.doubao_path, settings?.doubao_renew_url, settings?.doubao_quota_url, settings?.doubao_snapshot_include_idb, settings?.doubao_keepalive_enabled, settings?.doubao_quota_enabled, settings?.doubao_checkin_enabled, settings?.doubao_probe_auto_keepalive]);

  const detect = async () => {
    setDetecting(true);
    try {
      const r = await api.env.locate('doubao');
      setLocate(r);
      if (r.exe) {
        setPath(r.exe);
        await saveSettings({ doubao_path: r.exe });
        pushToast('success', `已自动定位豆包（${r.source === 'settings' ? '手动指定' : r.source === 'registry' ? '注册表' : r.source === 'default' ? '默认路径' : '进程反查'}）`);
      } else {
        pushToast('info', '未检测到豆包安装，请手动指定 Doubao.exe 路径');
      }
    } catch (err) {
      pushToast('error', `检测失败：${String(err)}`);
    } finally {
      setDetecting(false);
    }
  };

  const save = async () => {
    setSaving(true);
    try {
      await saveSettings({ doubao_path: path.trim() || null });
      pushToast('success', '豆包应用位置已保存');
    } catch (err) {
      pushToast('error', `保存失败：${String(err)}`);
    } finally {
      setSaving(false);
    }
  };

  const saveRenewUrl = async () => {
    const v = renewUrl.trim();
    if (v && !v.startsWith('http')) {
      pushToast('warn', '端点需以 http(s):// 开头');
      return;
    }
    try {
      await saveSettings({ doubao_renew_url: v || null });
    } catch {
      return; // store.saveSettings 失败时已 pushToast 错误详情，此处仅阻断成功提示
    }
    pushToast('success', v ? '保活端点已保存' : '已恢复默认端点（info/v2 轻量探活）');
  };

  const saveQuotaUrl = async () => {
    const v = quotaUrl.trim();
    if (v && !v.startsWith('http')) {
      pushToast('warn', '接口地址需以 http(s):// 开头');
      return;
    }
    try {
      await saveSettings({ doubao_quota_url: v || null });
    } catch {
      return; // store.saveSettings 失败时已 pushToast 错误详情，此处仅阻断成功提示
    }
    pushToast('success', v ? '会员额度接口已保存' : '已恢复默认会员额度接口');
  };

  const registerTask = async () => {
    // 审查修复（L39）：收紧为合法 HH:MM（小时 00-23、分钟 00-59），防止 99:99 之类通过校验后注册成功却永不触发
    if (!/^([01]\d|2[0-3]):([0-5]\d)$/.test(taskTime)) {
      pushToast('warn', '时间格式应为 HH:MM');
      return;
    }
    try {
      await api.doubao.taskRegister(taskTime);
      await refreshTaskStatus();
      pushToast('success', `豆包续期任务已注册（每日 ${taskTime}）`);
    } catch (err) {
      pushToast('error', `注册失败：${String(err)}`);
    }
  };

  const unregisterTask = async () => {
    try {
      await api.doubao.taskUnregister();
      await refreshTaskStatus();
      pushToast('info', '豆包续期任务已注销');
    } catch (err) {
      pushToast('error', `注销失败：${String(err)}`);
    }
  };

  const registerQuotaTask = async () => {
    // 审查修复（L39）：同上收紧为合法 HH:MM
    if (!/^([01]\d|2[0-3]):([0-5]\d)$/.test(quotaTaskTime)) {
      pushToast('warn', '时间格式应为 HH:MM');
      return;
    }
    try {
      await api.doubao.quotaTaskRegister(quotaTaskTime);
      await refreshQuotaTaskStatus();
      pushToast('success', `额度巡检任务已注册（每日 ${quotaTaskTime}）`);
    } catch (err) {
      pushToast('error', `注册失败：${String(err)}`);
    }
  };

  const unregisterQuotaTask = async () => {
    try {
      await api.doubao.quotaTaskUnregister();
      await refreshQuotaTaskStatus();
      pushToast('info', '额度巡检任务已注销');
    } catch (err) {
      pushToast('error', `注销失败：${String(err)}`);
    }
  };

  const registerCheckinTask = async () => {
    if (!/^([01]\d|2[0-3]):([0-5]\d)$/.test(checkinTaskTime)) {
      pushToast('warn', '时间格式应为 HH:MM');
      return;
    }
    try {
      await api.doubao.checkinTaskRegister(checkinTaskTime);
      checkinTimeDirtyRef.current = false; // 注册成功：复位脏标记，让随后的 refresh 恢复「按真实注册时刻回填收敛」
      await refreshCheckinTaskStatus();
      pushToast('success', `每日签到任务已注册（每日 ${checkinTaskTime}）`);
    } catch (err) {
      pushToast('error', `注册失败：${String(err)}`);
    }
  };

  const unregisterCheckinTask = async () => {
    try {
      await api.doubao.checkinTaskUnregister();
      await refreshCheckinTaskStatus();
      pushToast('info', '每日签到任务已注销');
    } catch (err) {
      pushToast('error', `注销失败：${String(err)}`);
    }
  };

  const runCheckinNow = async () => {
    setCheckinRunning(true);
    try {
      // 返回契约：batch → { ok, total, success, already, failed, reward_total, errors, finished_at }；
      // 空池 → { ok, skipped: "无豆包账号", skipped_empty_pool: true }（奖励型任务豁免，
      // 调度器对后者不 mark_run；前端双字段统一判定，只判 skipped 会漏掉豁免语义）
      const r = (await api.doubao.checkinRun()) as {
        skipped?: string;
        skipped_empty_pool?: boolean;
        total?: number;
        success?: number;
        already?: number;
        failed?: number;
        reward_total?: number;
      };
      if (r.skipped ?? r.skipped_empty_pool) {
        pushToast('info', `签到跳过：${r.skipped ?? '无豆包账号'}`);
      } else {
        pushToast(
          'success',
          `签到完成：共 ${r.total ?? 0} 个账号，成功 ${r.success ?? 0} · 已签 ${r.already ?? 0} · 失败 ${r.failed ?? 0}，新增额度 +${r.reward_total ?? 0}`,
        );
      }
    } catch (err) {
      pushToast('error', `签到失败：${String(err)}`);
    } finally {
      setCheckinRunning(false);
    }
  };

  const runRenew = async (syncOnly: boolean) => {
    setRunning(true);
    setSummary(null);
    try {
      const s = await api.doubao.renewRun(syncOnly);
      setSummary(s);
      if (s.mode === 'full' && s.renew) {
        const { ok, expired, error } = s.renew;
        pushToast(expired > 0 ? 'warn' : 'success', `巡检完成：有效 ${ok}，过期 ${expired}，异常 ${error}`);
      } else {
        pushToast('success', `Cookie 同步完成（${s.sync.synced} 个账号）`);
      }
    } catch (err) {
      pushToast('error', `巡检失败：${String(err)}`);
    } finally {
      setRunning(false);
    }
  };

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="豆包 · 环境配置"
        desc="通用配置（应用位置 / 快照 / 接口端点）与任务配置（保活 / 额度巡检 / 每日签到）"
      />

      <div className="mt-5 grid items-start gap-4 md:grid-cols-2">
        {/* 左列：通用配置（应用位置 + 快照设置 + 接口端点） */}
        <section className="card p-4">
          <div className="mb-3 flex items-center gap-2">
            <SlidersHorizontal size={16} className="text-emerald-500" />
            <h2 className="font-medium">通用配置</h2>
          </div>

          <h3 className="mb-1 font-medium">应用位置</h3>
          <div className="space-y-2 text-xs text-slate-500">
            <p>
              数据目录：<code className="rounded bg-slate-100 px-1 font-mono dark:bg-zinc-800">{locate?.user_data_dir ?? '%LOCALAPPDATA%\\Doubao\\User Data'}</code>
              {locate?.version ? ` · 版本 ${locate.version}` : ''}
              {locate?.exe && <span className="ml-2"><Badge tone="green">已定位</Badge></span>}
            </p>
            <div className="flex items-center gap-2">
              <input
                value={path}
                onChange={(e) => setPath(e.target.value)}
                placeholder="Doubao.exe 完整路径（如 C:\\Users\\<user>\\AppData\\Local\\Doubao\\Application\\Doubao.exe）"
                className="input flex-1 font-mono text-xs"
              />
              <button onClick={() => void detect()} disabled={detecting} className="btn-outline shrink-0">
                <FolderSearch size={15} /> {detecting ? '检测中…' : '自动检测'}
              </button>
              <button onClick={() => void save()} disabled={saving} className="btn-outline shrink-0">
                <Save size={15} /> {saving ? '保存中…' : '保存'}
              </button>
            </div>
            <p className="text-slate-400">自动检测顺序：手动指定 → 注册表卸载键 → 默认路径 → 运行进程反查。</p>
          </div>

          <h3 className="mb-1 mt-4 font-medium">快照设置</h3>
          <div className="space-y-3 text-xs text-slate-500">
            <p>
              快照自带<b>版本元数据</b>（schemaVersion + 豆包内核版本），恢复前自动校验完整性（leveldb 结构），
              豆包升级后若旧快照不兼容会中止并提示重新保存；在「账号管理」悬停快照列可查看版本信息。
            </p>
            <label className="flex cursor-pointer items-start gap-2.5">
              <input
                type="checkbox"
                checked={includeIdb}
                onChange={(e) => void toggleIncludeIdb(e.target.checked)}
                className="mt-0.5 h-4 w-4 shrink-0 accent-sky-500"
              />
              <span>
                <span className="font-medium text-slate-600 dark:text-zinc-200">快照纳入 IndexedDB（对话历史等完整状态）</span>
                <span className="mt-0.5 block leading-relaxed">
                  默认排除以控制体积。开启后，保存/切换会把 Default/IndexedDB 一并纳入快照，
                  对话历史等完整状态随账号迁移；代价是快照体积明显增大（可达数百 MB），保存/切换耗时也会变长。
                  已有快照需重新「保存当前登录态」后才会纳入。
                </span>
              </span>
            </label>
          </div>

          <h3 className="mb-1 mt-4 font-medium">接口端点</h3>
          <div className="space-y-3 text-xs text-slate-500">
            {/* 保活端点（对明文凭证的探活生效：manual / proxy 来源均可） */}
            <div className="flex items-center gap-2">
              <span className="w-16 shrink-0 text-slate-500">保活端点</span>
              <input
                value={renewUrl}
                onChange={(e) => setRenewUrl(e.target.value)}
                placeholder="https://www.doubao.com/info/v2/（默认，探活巡检用；KeepAlive 不依赖此项）"
                className="input flex-1 font-mono text-xs"
              />
              <button onClick={() => void saveRenewUrl()} className="btn-outline shrink-0">
                <Save size={14} /> 保存
              </button>
            </div>
            <div className="flex items-center gap-2">
              <span className="w-16 shrink-0 text-slate-500">额度接口</span>
              <input
                value={quotaUrl}
                onChange={(e) => setQuotaUrl(e.target.value)}
                placeholder="https://www.doubao.com/alice/commerce/sale/subscription/quota/summary/（默认）"
                className="input flex-1 font-mono text-xs"
              />
              <button onClick={() => void saveQuotaUrl()} className="btn-outline shrink-0">
                <Save size={14} /> 保存
              </button>
            </div>
            <p>
              会员额度接口已按代理抓包实测固化（<code className="rounded bg-slate-100 px-1 dark:bg-zinc-800">/alice/commerce/sale/subscription/quota/summary/</code>），
              默认即可用。豆包若更新接口，可在此修改。之后即可在「账号管理」中对已录入会话凭证的账号点击额度按钮查询
              （会员等级 / 到期时间 / 剩余额度条）。
            </p>
          </div>
        </section>

        {/* 右列：任务配置（会话保活 + 额度巡检 + 每日签到） */}
        <section className="card p-4">
          <div className="mb-3 flex items-center gap-2">
            <ListChecks size={16} className="text-emerald-500" />
            <h2 className="font-medium">任务配置</h2>
          </div>

          <h3 className="mb-1 font-medium">会话保活（每日续期）</h3>
          <div className="space-y-3 text-xs text-slate-500">
            <p>
              续期原理：字节 passport 为 <b>sid_guard 30 天滑动续期</b>。实测豆包桌面客户端 cookie 为客户端级加密
              （外部无法离线续写），因此<b>主路径为每日保活</b>——注册定时任务后自动「启动豆包 8 秒 → 优雅关闭」，
              由客户端自己联网刷新会话；运行中则自动跳过。此外可对池内<b>明文 sessionid 凭证</b>（代理抓包自动写入或手动录入，账号管理 → 编辑 → 会话凭证可查看）做探活巡检。
            </p>

            {/* 定时任务 */}
            <div className="flex flex-wrap items-center gap-2">
              <span className="shrink-0 text-slate-500">每日任务</span>
              <input
                value={taskTime}
                onChange={(e) => setTaskTime(e.target.value)}
                placeholder="HH:MM"
                className="input w-24 text-xs"
              />
              <button onClick={() => void registerTask()} className="btn-outline shrink-0">
                <Timer size={14} /> 注册
              </button>
              {taskState === 'registered' && (
                <>
                  <Badge tone="green">已注册 {taskTimeShown}</Badge>
                  <button onClick={() => void unregisterTask()} className="btn-ghost text-rose-500">
                    注销
                  </button>
                </>
              )}
              {taskState === 'not_registered' && <Badge tone="slate">未注册</Badge>}
              {taskState === 'error' && (
                <>
                  <Badge tone="amber">状态未知</Badge>
                  <button onClick={() => void refreshTaskStatus()} className="btn-ghost">
                    重试
                  </button>
                </>
              )}
            </div>

            {/* 应用内每日调度开关（issue #147）：关闭后不再每日拉起客户端；无账号/客户端缺失时自动跳过 */}
            <label className="flex cursor-pointer items-start gap-2.5">
              <input
                type="checkbox"
                checked={keepaliveEnabled}
                onChange={(e) => void toggleKeepalive(e.target.checked)}
                className="mt-0.5 h-4 w-4 shrink-0 accent-emerald-500"
              />
              <span>
                <span className="font-medium text-slate-600 dark:text-zinc-200">应用内每日调度（每日 09:20 启动豆包续期）</span>
                <span className="mt-0.5 block leading-relaxed">
                  关闭后应用内调度器不再执行会话每日续期（不再拉起客户端）；与上方「每日任务」（Windows 计划任务）相互独立。
                  无豆包账号或未找到豆包客户端时自动跳过，不记失败、不推送通知。
                </span>
              </span>
            </label>

            {/* 手动巡检 */}
            <div className="flex flex-wrap items-center gap-2">
              <span className="shrink-0 text-slate-500">手动巡检</span>
              <button onClick={() => void runRenew(false)} disabled={running} className="btn-outline shrink-0">
                <ShieldCheck size={14} /> {running ? '巡检中…' : '探活巡检'}
              </button>
              <button onClick={() => void runRenew(true)} disabled={running} className="btn-outline shrink-0">
                <RefreshCw size={14} /> Cookie 诊断
              </button>
              <span className="text-slate-400">每日保活任务见上方「每日任务」</span>
            </div>

            {/* 巡检结果 */}
            {summary && (
              <div className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
                <div className="mb-1 text-xs font-medium text-slate-600 dark:text-zinc-300">
                  {summary.finished_at} · {summary.mode === 'full' ? `巡检（${summary.renew_url}）` : '仅同步'}
                  {summary.sync ? ` · Cookie 同步 ${summary.sync.synced} 个` : ''}
                </div>
                {summary.renew && (
                  <div className="mb-1 text-xs text-slate-500">
                    有效 {summary.renew.ok} · 过期 {summary.renew.expired} · 异常 {summary.renew.error} · 跳过 {summary.renew.skipped}
                  </div>
                )}
                {summary.accounts && summary.accounts.length > 0 && (
                  <div className="max-h-40 space-y-0.5 overflow-auto font-mono text-xs text-slate-500 dark:text-zinc-400">
                    {summary.accounts.map((a) => (
                      <div key={a.user_id}>
                        {a.user_id}: {a.status}
                        {a.detail ? `（${a.detail}）` : ''}
                        {a.renewed ? ' · 已续期' : ''}
                      </div>
                    ))}
                  </div>
                )}
                {summary.logs && summary.logs.length > 0 && (
                  <div className="mt-1 max-h-24 space-y-0.5 overflow-auto text-xs text-slate-400">
                    {summary.logs.map((l, i) => (
                      <div key={i}>{l}</div>
                    ))}
                  </div>
                )}
              </div>
            )}
          </div>

          <h3 className="mb-1 mt-4 font-medium">会员额度巡检</h3>
          <div className="space-y-3 text-xs text-slate-500">
            {/* 每日额度巡检任务 */}
            <div className="flex flex-wrap items-center gap-2">
              <span className="shrink-0 text-slate-500">每日巡检</span>
              <input
                value={quotaTaskTime}
                onChange={(e) => setQuotaTaskTime(e.target.value)}
                placeholder="HH:MM"
                className="input w-24 text-xs"
              />
              <button onClick={() => void registerQuotaTask()} className="btn-outline shrink-0">
                <Timer size={14} /> 注册
              </button>
              {quotaTaskState === 'registered' && (
                <>
                  <Badge tone="green">已注册 {quotaTaskTimeShown}</Badge>
                  <button onClick={() => void unregisterQuotaTask()} className="btn-ghost text-rose-500">
                    注销
                  </button>
                </>
              )}
              {quotaTaskState === 'not_registered' && <Badge tone="slate">未注册</Badge>}
              {quotaTaskState === 'error' && (
                <>
                  <Badge tone="amber">状态未知</Badge>
                  <button onClick={() => void refreshQuotaTaskStatus()} className="btn-ghost">
                    重试
                  </button>
                </>
              )}
            </div>

            {/* 应用内每日巡检调度开关（issue #147） */}
            <label className="flex cursor-pointer items-start gap-2.5">
              <input
                type="checkbox"
                checked={quotaEnabled}
                onChange={(e) => void toggleQuotaEnabled(e.target.checked)}
                className="mt-0.5 h-4 w-4 shrink-0 accent-amber-500"
              />
              <span>
                <span className="font-medium text-slate-600 dark:text-zinc-200">应用内每日巡检调度（每日 09:30）</span>
                <span className="mt-0.5 block leading-relaxed">
                  关闭后应用内调度器不再定时批量巡检额度；与上方「每日巡检」（Windows 计划任务）相互独立。
                  无豆包账号时自动跳过。
                </span>
              </span>
            </label>

            <p>
              注册后每日定时批量查询池内全部有凭证账号的会员额度：自动回写缓存与历史（概述页趋势图/健康度卡的数据源），
              额度用完的账号会在应用内提醒。
            </p>
          </div>

          <h3 className="mb-1 mt-4 font-medium">每日签到</h3>
          <div className="space-y-3 text-xs text-slate-500">
            <p>
              签到端点已按抓包固化（<code className="rounded bg-slate-100 px-1 dark:bg-zinc-800">/alice/ug/campaign/landing/checkin</code>）。
              注册后每日定时对池内全部账号签到领额度，结果按日落库（概述页签到结果与额度看板的数据源）；
              当日已签的账号自动跳过（幂等）；未签成功的账号次日自动补签，同日也可在「一键签到」页再次手动签到，多次结果按最终状态合并。
            </p>

            {/* 每日签到任务 */}
            <div className="flex flex-wrap items-center gap-2">
              <span className="shrink-0 text-slate-500">每日任务</span>
              <input
                ref={checkinTaskTimeInputRef}
                value={checkinTaskTime}
                onChange={(e) => {
                  setCheckinTaskTime(e.target.value);
                  checkinTimeDirtyRef.current = true; // 编辑中：refresh 回填暂不打回
                }}
                placeholder="HH:MM"
                className="input w-24 text-xs"
              />
              <button onClick={() => void registerCheckinTask()} className="btn-outline shrink-0">
                <Timer size={14} /> 注册
              </button>
              {checkinTaskState === 'registered' && (
                <>
                  <Badge tone="green">已注册 {checkinTaskTimeShown}</Badge>
                  <button onClick={() => void unregisterCheckinTask()} className="btn-ghost text-rose-500">
                    注销
                  </button>
                </>
              )}
              {checkinTaskState === 'not_registered' && <Badge tone="slate">未注册</Badge>}
              {checkinTaskState === 'error' && (
                <>
                  <Badge tone="amber">状态未知</Badge>
                  <button onClick={() => void refreshCheckinTaskStatus()} className="btn-ghost">
                    重试
                  </button>
                </>
              )}
            </div>

            {/* 应用内每日签到调度开关 */}
            <label className="flex cursor-pointer items-start gap-2.5">
              <input
                type="checkbox"
                checked={checkinEnabled}
                onChange={(e) => void toggleCheckinEnabled(e.target.checked)}
                className="mt-0.5 h-4 w-4 shrink-0 accent-rose-500"
              />
              <span>
                <span className="font-medium text-slate-600 dark:text-zinc-200">
                  应用内每日签到调度（每日 {settings?.doubao_checkin_hhmm || '09:15'}）
                </span>
                {/* 可编辑签到时刻（单源 doubao_checkin_hhmm）：纯应用内调度用户修改时刻的入口；交互对齐 syncHhmm（onChange 暂存、失焦校验保存） */}
                <input
                  value={checkinHhmm}
                  onChange={(e) => setCheckinHhmm(e.target.value)}
                  onBlur={(e) => void changeCheckinHhmm(e.target.value)}
                  placeholder="HH:MM"
                  className="input ml-2 w-20 text-xs"
                />
                <span className="mt-0.5 block leading-relaxed">
                  关闭后应用内调度器不再执行每日自动签到；与上方「每日任务」（Windows 计划任务）相互独立。
                  无豆包账号时自动跳过，不记失败、不推送通知。
                </span>
              </span>
            </label>

            {/* probe 自动保活开关（方案 A） */}
            <label className="flex cursor-pointer items-start gap-2.5">
              <input
                type="checkbox"
                checked={probeAutoKeepalive}
                onChange={(e) => void toggleProbeAutoKeepalive(e.target.checked)}
                className="mt-0.5 h-4 w-4 shrink-0 accent-rose-500"
              />
              <span>
                <span className="font-medium text-slate-600 dark:text-zinc-200">
                  签到前自动抓包对齐（probe 自动保活）
                </span>
                <span className="mt-0.5 block leading-relaxed">
                  开启后：签到时若抓包参数缺失/过期，自动启动代理（未运行时）→ 以账号打开豆包注入代理 →
                  等客户端自动抓包入库（每账号约 30-60 秒）→ 继续签到。注意：豆包客户端会被短暂切换账号，
                  结束停留在最后处理的账号；代理由保活启动的会在结束后自动停止还原。
                </span>
              </span>
            </label>

            {/* 手动签到 */}
            <div className="flex flex-wrap items-center gap-2">
              <span className="shrink-0 text-slate-500">手动签到</span>
              <button onClick={() => void runCheckinNow()} disabled={checkinRunning} className="btn-outline shrink-0">
                <CalendarCheck size={14} /> {checkinRunning ? '签到中…' : '立即签到全部'}
              </button>
              <span className="text-slate-400">逐账号签到见「一键签到」页</span>
            </div>
          </div>

          {/* 额度数据同步（对齐 Trae「积分数据同步」交互）：置于任务配置底部 */}
          <div className="mt-4 border-t border-slate-100 pt-3 dark:border-zinc-800">
          <h3 className="mb-1 font-medium">额度数据同步</h3>
          <div className="space-y-3 text-xs text-slate-500">
            <p>
              应用运行期间按所选模式自动拉取账号额度与累计额度（含额度窗口、会员等级、总额度）并回写本地账号池；
              账号管理 / 额度看板等页面只读本地数据，不再每次打开都请求远端。无账号时静默跳过。
            </p>
            <div className="flex flex-wrap items-center gap-2">
              <span className="shrink-0 text-slate-500">同步模式</span>
              <select value={syncMode} onChange={(e) => void changeSyncMode(e.target.value)} className="input w-32 text-xs">
                <option value="off">关闭</option>
                <option value="hourly">每小时</option>
                <option value="every_hours">每N小时</option>
                <option value="daily">每天 HH:MM</option>
              </select>
              {syncMode === 'every_hours' && (
                <>
                  <input
                    value={syncHours}
                    onChange={(e) => setSyncHours(e.target.value)}
                    onBlur={(e) => void changeSyncHours(e.target.value)}
                    inputMode="numeric"
                    className="input w-16 text-xs"
                  />
                  <span>小时（1-24）；距上次成功同步满 N 小时自动执行一次</span>
                </>
              )}
              {syncMode === 'daily' && (
                <>
                  <input
                    value={syncHhmm}
                    onChange={(e) => setSyncHhmm(e.target.value)}
                    onBlur={(e) => void changeSyncHhmm(e.target.value)}
                    placeholder="HH:MM"
                    className="input w-20 text-xs"
                  />
                  <span>每日该时刻执行一次</span>
                </>
              )}
            </div>
            <p className="text-slate-400">
              修改后立即生效（保存即写库）；手动单账号查询仍可在账号管理行内「查询额度」触发。
            </p>
          </div>
          </div>
        </section>
      </div>
    </div>
  );
}
