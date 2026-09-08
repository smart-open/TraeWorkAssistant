import { useEffect, useState } from 'react';
import { FolderSearch, Save, Clock3, MapPin, ShieldCheck, RefreshCw, Timer } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { Badge } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import type { AppLocate, DoubaoRenewSummary } from '../../types';

/** 豆包环境配置：应用位置配置（app_locate + 手动路径持久化）+ 会话续期（P3：端点/定时任务/手动巡检） */
export default function DoubaoSettings() {
  const settings = useAppStore((s) => s.settings);
  const saveSettings = useAppStore((s) => s.saveSettings);
  const pushToast = useAppStore((s) => s.pushToast);

  const [locate, setLocate] = useState<AppLocate | null>(null);
  const [path, setPath] = useState('');
  const [detecting, setDetecting] = useState(false);
  const [saving, setSaving] = useState(false);

  // ── 续期 ──
  const [renewUrl, setRenewUrl] = useState('');
  const [taskTime, setTaskTime] = useState('09:00');
  const [taskState, setTaskState] = useState<'loading' | 'registered' | 'not_registered'>('loading');
  const [taskTimeShown, setTaskTimeShown] = useState('');
  const [running, setRunning] = useState(false);
  const [summary, setSummary] = useState<DoubaoRenewSummary | null>(null);

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

  useEffect(() => {
    if (settings?.doubao_path) setPath(settings.doubao_path);
    if (settings?.doubao_renew_url) setRenewUrl(settings.doubao_renew_url);
    void refreshTaskStatus();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [settings?.doubao_path, settings?.doubao_renew_url]);

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
    await saveSettings({ doubao_renew_url: v || null });
    pushToast('success', v ? '保活端点已保存' : '已恢复默认端点（doubao.com 首页滑动续期）');
  };

  const registerTask = async () => {
    if (!/^\d{2}:\d{2}$/.test(taskTime)) {
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
        desc="应用位置配置与会话续期（P3 已接入）"
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
          <button onClick={() => void save()} disabled={saving} className="btn-primary shrink-0">
            <Save size={15} /> {saving ? '保存中…' : '保存'}
          </button>
        </div>

        <p className="mt-2 text-xs text-slate-400">
          自动检测顺序：手动指定 → 注册表卸载键 → 默认路径（%LOCALAPPDATA%\Doubao\Application）→ 运行进程反查（doubao-trae-switch-plan.md §1.3）。
        </p>
      </div>

      {/* 会话续期（P3） */}
      <div className="mt-4 card p-4">
        <div className="mb-3 flex items-center gap-2">
          <Clock3 size={16} className="text-emerald-500" />
          <span className="text-sm font-medium">会话续期</span>
          <Badge tone="green">P3 已接入</Badge>
        </div>

        <div className="space-y-3 text-xs text-slate-500">
          <p>
            续期原理：字节 passport 为 <b>sid_guard 30 天滑动续期</b>。实测豆包桌面客户端 cookie 为客户端级加密
            （外部无法离线续写），因此<b>主路径为每日保活</b>——注册定时任务后自动「启动豆包 25 秒 → 优雅关闭」，
            由客户端自己联网刷新会话；运行中则自动跳过。此外可对手动录入的 sessionid（账号管理 → 编辑 → 会话凭证）做探活巡检。
          </p>

          {/* 保活端点（仅对手动录入凭证的探活生效） */}
          <div className="flex items-center gap-2">
            <span className="shrink-0 text-slate-500">保活端点</span>
            <input
              value={renewUrl}
              onChange={(e) => setRenewUrl(e.target.value)}
              placeholder="默认 https://www.doubao.com/（探活巡检用，KeepAlive 不依赖此项）"
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

          {/* 手动巡检 */}
          <div className="flex flex-wrap items-center gap-2">
            <span className="shrink-0 text-slate-500">手动巡检</span>
            <button onClick={() => void runRenew(false)} disabled={running} className="btn-primary shrink-0">
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
    </div>
  );
}
