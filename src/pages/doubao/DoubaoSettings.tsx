import { useEffect, useState } from 'react';
import { FolderSearch, Save, Clock3, MapPin, ShieldCheck, RefreshCw, Timer, Coins, Database, CalendarCheck } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { Badge } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import type { AppLocate, DoubaoRenewSummary } from '../../types';

/** 豆包环境配置：应用位置配置 + 会话保活 + 会员额度接口 */
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
  const [taskState, setTaskState] = useState<'loading' | 'registered' | 'not_registered'>('loading');
  const [taskTimeShown, setTaskTimeShown] = useState('');
  const [running, setRunning] = useState(false);
  const [summary, setSummary] = useState<DoubaoRenewSummary | null>(null);

  // ── 会员额度 ──
  const [quotaUrl, setQuotaUrl] = useState('');
  // ── 额度巡检定时任务 ──
  const [quotaTaskTime, setQuotaTaskTime] = useState('09:30');
  const [quotaTaskState, setQuotaTaskState] = useState<'loading' | 'registered' | 'not_registered'>('loading');
  const [quotaTaskTimeShown, setQuotaTaskTimeShown] = useState('');

  // ── 快照设置（C4：IndexedDB 可选纳入）──
  const [includeIdb, setIncludeIdb] = useState(false);

  // ── 应用内调度开关（issue #147，默认开）──
  const [keepaliveEnabled, setKeepaliveEnabled] = useState(true);
  const [quotaEnabled, setQuotaEnabled] = useState(true);

  // ── 每日签到任务 ──
  const [checkinTaskTime, setCheckinTaskTime] = useState('09:15');
  const [checkinTaskState, setCheckinTaskState] = useState<'loading' | 'registered' | 'not_registered'>('loading');
  const [checkinTaskTimeShown, setCheckinTaskTimeShown] = useState('');
  const [checkinRunning, setCheckinRunning] = useState(false);
  const [checkinEnabled, setCheckinEnabled] = useState(true);

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
      setTaskState('not_registered');
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
      setQuotaTaskState('not_registered');
    }
  };

  const refreshCheckinTaskStatus = async () => {
    try {
      const s = await api.doubao.checkinTaskStatus();
      if (s.startsWith('registered:')) {
        setCheckinTaskState('registered');
        setCheckinTaskTimeShown(s.slice('registered:'.length));
      } else {
        setCheckinTaskState('not_registered');
      }
    } catch {
      setCheckinTaskState('not_registered');
    }
  };

  useEffect(() => {
    if (settings?.doubao_path) setPath(settings.doubao_path);
    if (settings?.doubao_renew_url) setRenewUrl(settings.doubao_renew_url);
    if (settings?.doubao_quota_url) setQuotaUrl(settings.doubao_quota_url);
    if (settings) {
      setIncludeIdb(!!settings.doubao_snapshot_include_idb);
      // !== false：旧版后端缺字段时按默认开展示（issue #147 新字段向后兼容）
      setKeepaliveEnabled(settings.doubao_keepalive_enabled !== false);
      setQuotaEnabled(settings.doubao_quota_enabled !== false);
      setCheckinEnabled(settings.doubao_checkin_enabled !== false);
    }
    void refreshTaskStatus();
    void refreshQuotaTaskStatus();
    void refreshCheckinTaskStatus();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [settings?.doubao_path, settings?.doubao_renew_url, settings?.doubao_quota_url, settings?.doubao_snapshot_include_idb, settings?.doubao_keepalive_enabled, settings?.doubao_quota_enabled, settings?.doubao_checkin_enabled]);

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
      // 返回契约：batch → { ok, total, success, already, failed, reward_total, errors, finished_at }
      const r = (await api.doubao.checkinRun()) as {
        skipped?: string;
        total?: number;
        success?: number;
        already?: number;
        failed?: number;
        reward_total?: number;
      };
      if (r.skipped) {
        pushToast('info', `签到跳过：${r.skipped}`);
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
        desc="应用位置、快照设置、会话保活与会员额度接口"
      />

      {/* 应用位置配置 */}
      <div className="mt-5 card p-4">
        <div className="mb-3 flex items-center gap-2">
          <MapPin size={16} className="text-violet-500" />
          <span className="text-sm font-medium">应用位置配置</span>
          {locate?.exe && <Badge tone="green">已定位</Badge>}
        </div>

        <div className="space-y-2 text-xs text-slate-500">
          <p>
            数据目录：<code className="rounded bg-slate-100 px-1 font-mono dark:bg-zinc-800">{locate?.user_data_dir ?? '%LOCALAPPDATA%\\Doubao\\User Data'}</code>
            {locate?.version ? ` · 版本 ${locate.version}` : ''}
          </p>
        </div>

        <div className="mt-3 flex items-center gap-2">
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

        <p className="mt-2 text-xs text-slate-400">
          自动检测顺序：手动指定 → 注册表卸载键 → 默认路径 → 运行进程反查。
        </p>
      </div>

      {/* 会话保活 */}
      <div className="mt-4 card p-4">
        <div className="mb-3 flex items-center gap-2">
          <Clock3 size={16} className="text-emerald-500" />
          <span className="text-sm font-medium">会话保活</span>
        </div>

        <div className="space-y-3 text-xs text-slate-500">
          <p>
            续期原理：字节 passport 为 <b>sid_guard 30 天滑动续期</b>。实测豆包桌面客户端 cookie 为客户端级加密
            （外部无法离线续写），因此<b>主路径为每日保活</b>——注册定时任务后自动「启动豆包 8 秒 → 优雅关闭」，
            由客户端自己联网刷新会话；运行中则自动跳过。此外可对池内<b>明文 sessionid 凭证</b>（代理抓包自动写入或手动录入，账号管理 → 编辑 → 会话凭证可查看）做探活巡检。
          </p>

          {/* 保活端点（对明文凭证的探活生效：manual / proxy 来源均可） */}
          <div className="flex items-center gap-2">
            <span className="shrink-0 text-slate-500">保活端点</span>
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
      </div>

      {/* 快照设置（C3/C4） */}
      <div className="mt-4 card p-4">
        <div className="mb-3 flex items-center gap-2">
          <Database size={16} className="text-sky-500" />
          <span className="text-sm font-medium">快照设置</span>
        </div>

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
      </div>

      {/* 会员额度 */}
      <div className="mt-4 card p-4">
        <div className="mb-3 flex items-center gap-2">
          <Coins size={16} className="text-amber-500" />
          <span className="text-sm font-medium">会员额度</span>
        </div>

        <div className="space-y-3 text-xs text-slate-500">
          <p>
            会员额度接口已按代理抓包实测固化（<code className="rounded bg-slate-100 px-1 dark:bg-zinc-800">/alice/commerce/sale/subscription/quota/summary/</code>），
            默认即可用。豆包若更新接口，可在此修改。之后即可在「账号管理」中对已录入会话凭证的账号点击额度按钮查询
            （会员等级 / 到期时间 / 剩余额度条）。
          </p>

          <div className="flex items-center gap-2">
            <span className="shrink-0 text-slate-500">额度接口</span>
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
      </div>

      {/* 每日签到 */}
      <div className="mt-4 card p-4">
        <div className="mb-3 flex items-center gap-2">
          <CalendarCheck size={16} className="text-rose-500" />
          <span className="text-sm font-medium">每日签到</span>
        </div>

        <div className="space-y-3 text-xs text-slate-500">
          <p>
            签到端点已按抓包固化（<code className="rounded bg-slate-100 px-1 dark:bg-zinc-800">/alice/ug/campaign/landing/checkin</code>）。
            注册后每日定时对池内全部账号签到领额度，结果按日落库（概述页签到结果与额度看板的数据源）；
            当日已签的账号自动跳过（幂等）；未签成功的账号次日自动补签，同日也可在账号页再次手动签到，多次结果按最终状态合并。
          </p>

          {/* 每日签到任务 */}
          <div className="flex flex-wrap items-center gap-2">
            <span className="shrink-0 text-slate-500">每日任务</span>
            <input
              value={checkinTaskTime}
              onChange={(e) => setCheckinTaskTime(e.target.value)}
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
              <span className="font-medium text-slate-600 dark:text-zinc-200">应用内每日签到调度（每日 09:15）</span>
              <span className="mt-0.5 block leading-relaxed">
                关闭后应用内调度器不再执行每日自动签到；与上方「每日任务」（Windows 计划任务）相互独立。
                无豆包账号时自动跳过，不记失败、不推送通知。
              </span>
            </span>
          </label>

          {/* 手动签到 */}
          <div className="flex flex-wrap items-center gap-2">
            <span className="shrink-0 text-slate-500">手动签到</span>
            <button onClick={() => void runCheckinNow()} disabled={checkinRunning} className="btn-outline shrink-0">
              <CalendarCheck size={14} /> {checkinRunning ? '签到中…' : '立即签到全部'}
            </button>
            <span className="text-slate-400">单账号签到见「账号管理」各行签到按钮</span>
          </div>
        </div>
      </div>
    </div>
  );
}
