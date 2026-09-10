import { useCallback, useEffect, useState } from 'react';
import { Save, FolderOpen, RefreshCw } from 'lucide-react';
import { open } from '@tauri-apps/plugin-shell';
import { localDataDir } from '@tauri-apps/api/path';
import PageHeader from '../../components/PageHeader';
import { Badge, Spinner } from '../../components/ui';
import ExpiryCalendar from '../../components/ExpiryCalendar';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { withMinDelay } from '../../lib/delay';
import type { WorkBuddySettings, WorkBuddyEnvCheck, WorkBuddyAccountView, WbCreditsResult } from '../../types';

/**
 * buddy-settings 环境配置（§3.7.5，F-55/F-13）：
 * 环境卡 + 自动签到配置卡（F-55 参数化）+ 到期日历 + CLI 轮换占位（批次3）。
 */
export default function BuddySettings() {
  const pushToast = useAppStore((s) => s.pushToast);
  const [env, setEnv] = useState<WorkBuddyEnvCheck | null>(null);
  const [accounts, setAccounts] = useState<WorkBuddyAccountView[]>([]);
  const [credits, setCredits] = useState<WbCreditsResult | null>(null);
  const [settings, setSettings] = useState<WorkBuddySettings | null>(null);
  const [saving, setSaving] = useState(false);

  const refresh = useCallback(async () => {
    try {
      const [e, accs, st] = await Promise.all([
        api.workbuddy.envCheck(),
        api.workbuddy.accountsList().catch(() => [] as WorkBuddyAccountView[]),
        api.workbuddy.settingsGet().catch(() => null),
      ]);
      setEnv(e);
      setAccounts(accs);
      setSettings(st);
      api.workbuddy
        .creditsFetch()
        .then(setCredits)
        .catch(() => setCredits(null));
    } catch (err) {
      pushToast('error', `环境检测失败：${String(err)}`);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const save = async () => {
    if (!settings) return;
    setSaving(true);
    try {
      await withMinDelay(api.workbuddy.settingsSet(settings), 800);
      pushToast('success', '配置已保存');
    } catch (err) {
      pushToast('error', `保存失败：${String(err)}`);
    } finally {
      setSaving(false);
    }
  };

  const patch = (p: Partial<WorkBuddySettings>) => {
    if (settings) setSettings({ ...settings, ...p });
  };

  // 到期日历条目：token（access/refresh 双轨）+ 积分包
  const items = accounts.flatMap((a) => {
    const out = [] as Parameters<typeof ExpiryCalendar>[0]['items'];
    if (a.access_token_expires_at) {
      out.push({
        key: `acc-${a.id}`,
        label: `${a.nickname || a.id} · accessToken`,
        kind: 'token',
        expire_ts: a.access_token_expires_at,
        note: null,
      });
    }
    if (a.refresh_token_expires_at) {
      out.push({
        key: `ref-${a.id}`,
        label: `${a.nickname || a.id} · refreshToken`,
        kind: 'token',
        expire_ts: a.refresh_token_expires_at,
        note: null,
      });
    }
    return out;
  });
  for (const acc of credits?.accounts ?? []) {
    for (const p of acc.packages) {
      items.push({
        key: `pkg-${acc.user_id}-${p.name}-${p.expire_ts ?? 0}`,
        label: `${acc.name} · ${p.name}`,
        kind: '积分包',
        expire_ts: p.expire_ts,
        note: `剩余 ${p.remaining.toFixed(2)}`,
      });
    }
  }

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="WorkBuddy · 环境配置"
        desc="客户端环境 · 自动签到 · 到期日历"
        actions={
          <>
            <button className="btn-outline" onClick={() => void refresh()}>
              <RefreshCw size={15} /> 重新检测
            </button>
            <button className="btn-primary" onClick={() => void save()} disabled={saving || !settings}>
              {saving ? <Spinner className="text-white" /> : <Save size={15} />} 保存配置
            </button>
          </>
        }
      />

      {/* 环境卡 */}
      <div className="card p-4">
        <div className="mb-3 flex items-center justify-between">
          <span className="text-sm font-medium">环境</span>
          <div className="flex gap-2">
            <Badge tone={env?.installed ? 'green' : 'red'}>{env?.installed ? '已安装' : '未安装'}</Badge>
            <Badge tone={env?.running ? 'green' : 'slate'}>{env?.running ? '运行中' : '已停止'}</Badge>
            {env?.version && <Badge tone="slate">v{env.version}</Badge>}
          </div>
        </div>
        <div className="grid gap-3 lg:grid-cols-2">
          <div className="rounded-lg border border-slate-100 p-3 text-xs dark:border-zinc-800">
            <div className="mb-1 font-medium text-slate-500">客户端路径（自动检测）</div>
            <div className="break-all font-mono text-slate-600 dark:text-zinc-300">{env?.exe ?? '未检测到'}</div>
            <div className="mt-1 text-slate-400">手动路径覆盖可在 Trae 页「环境配置」的 workbuddy_path 设置（随后续批次开放）</div>
          </div>
          <div className="rounded-lg border border-slate-100 p-3 text-xs dark:border-zinc-800">
            <div className="mb-1 flex items-center justify-between font-medium text-slate-500">
              auth 文件路径
              <Badge tone={env?.auth_file_exists ? 'green' : 'amber'}>{env?.auth_file_exists ? '存在 ✓' : '不存在'}</Badge>
            </div>
            <div className="break-all font-mono text-slate-600 dark:text-zinc-300">
              %LOCALAPPDATA%\CodeBuddyExtension\Data\Public\auth\workbuddy-desktop.info
            </div>
            <button
              className="mt-2 flex items-center gap-1 text-xs text-brand-600 hover:underline dark:text-brand-400"
              onClick={() =>
                void localDataDir()
                  .then((d) => open(`file:///${d.replace(/\\/g, '/')}CodeBuddyExtension/Data/Public/auth`))
                  .catch((e) => pushToast('error', `打开目录失败：${String(e)}`))
              }
            >
              <FolderOpen size={13} /> 打开所在目录
            </button>
          </div>
        </div>
      </div>

      {/* 自动签到配置卡（F-55） */}
      <div className="mt-4 card p-4">
        <div className="mb-3 text-sm font-medium">自动签到</div>
        <div className="space-y-3">
          <label className="flex items-start gap-2">
            <input
              type="checkbox"
              className="mt-0.5"
              checked={settings?.auto_checkin ?? false}
              onChange={(e) => patch({ auto_checkin: e.target.checked })}
            />
            <span className="text-sm">
              启用自动签到（启动补签）
              <span className="block text-xs text-slate-400">应用启动时立即核验服务端状态，未签到账号会自动补签</span>
            </span>
          </label>
          <div className="grid gap-3 lg:grid-cols-2">
            <label className="block">
              <span className="mb-1 block text-xs font-medium text-slate-500">保活阈值（天）</span>
              <input
                type="number"
                min={0}
                className="input w-full"
                value={settings?.keepalive_days ?? 0}
                onChange={(e) => patch({ keepalive_days: Number(e.target.value) || 0 })}
              />
              <span className="mt-1 block text-xs text-slate-400">0 = 每天无条件刷新全部带 refreshToken 账号</span>
            </label>
            <label className="block">
              <span className="mb-1 block text-xs font-medium text-slate-500">惰性刷新（小时）</span>
              <input
                type="number"
                min={1}
                className="input w-full"
                value={settings?.lazy_refresh_hours ?? 24}
                onChange={(e) => patch({ lazy_refresh_hours: Number(e.target.value) || 24 })}
              />
              <span className="mt-1 block text-xs text-slate-400">剩余有效期低于该值才触发刷新（默认 24）</span>
            </label>
          </div>
        </div>
      </div>

      {/* CLI 自动轮换（批次3 占位） */}
      <div className="mt-4 card p-4">
        <div className="mb-2 flex items-center gap-2">
          <span className="text-sm font-medium">CLI 自动轮换（CodeBuddy）</span>
          <Badge tone="slate">批次 3 开放</Badge>
        </div>
        <p className="text-xs text-slate-400">
          将按五重防护（冷却期 / 到期差异阈值 / 到期紧迫阈值 / 活跃保护 / 最小剩余积分）自动切换
          ~/.codebuddy/settings.json 的 CODEBUDDY_AUTH_TOKEN，敬请期待。
        </p>
      </div>

      {/* 到期日历 */}
      <div className="mt-4 card p-4">
        <div className="mb-3 text-sm font-medium">到期日历</div>
        <ExpiryCalendar items={items} />
      </div>
    </div>
  );
}
