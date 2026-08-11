import { useMemo, useState } from 'react';
import { PlayCircle, Square, CheckCircle2, XCircle, Clock, AlertCircle } from 'lucide-react';
import PageHeader from '../components/PageHeader';
import { Badge, Progress } from '../components/ui';
import { useAppStore } from '../store';

export default function Checkin() {
  const accounts = useAppStore((s) => s.accounts);
  const groups = useAppStore((s) => s.groups);
  const settings = useAppStore((s) => s.settings);
  const checkin = useAppStore((s) => s.checkin);
  const startCheckin = useAppStore((s) => s.startCheckin);

  const [scope, setScope] = useState<'all' | 'group' | 'selected'>('all');
  const [groupId, setGroupId] = useState<string>(groups[0]?.id ?? '');
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [skipChecked, setSkipChecked] = useState(true);
  const [skipExpired, setSkipExpired] = useState(true);

  const candidateIds = useMemo(() => {
    if (scope === 'all') return accounts.map((a) => a.user_id);
    if (scope === 'group') return accounts.filter((a) => a.group_id === groupId).map((a) => a.user_id);
    return [...selected];
  }, [scope, groupId, selected, accounts]);

  const toggle = (uid: string) => {
    setSelected((prev) => {
      const n = new Set(prev);
      if (n.has(uid)) n.delete(uid);
      else n.add(uid);
      return n;
    });
  };

  const start = async () => {
    if (candidateIds.length === 0) return;
    let scopeArg: string = 'all';
    if (scope === 'group') scopeArg = `group:${groupId}`;
    if (scope === 'selected') scopeArg = 'selected';
    await startCheckin({
      scope: scopeArg,
      user_ids: scope === 'selected' ? candidateIds : undefined,
      skip_checked_in: skipChecked,
      skip_expired: skipExpired,
    });
  };

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="一键签到"
        desc="按账号范围与跳过规则发起批量签到，实时查看进度"
      />

      <div className="card mb-5 p-4">
        <div className="grid gap-3 md:grid-cols-2">
          <div>
            <label className="label">签到范围</label>
            <div className="flex flex-wrap gap-2">
              {([
                { k: 'all', t: '全部账号' },
                { k: 'group', t: '指定分组' },
                { k: 'selected', t: '手动勾选' },
              ] as const).map((o) => (
                <button
                  key={o.k}
                  onClick={() => setScope(o.k)}
                  className={`chip border ${scope === o.k ? 'border-brand-500 bg-brand-50 text-brand-700 dark:bg-brand-500/15 dark:text-brand-300' : 'border-slate-300 text-slate-500'}`}
                >
                  {o.t}
                </button>
              ))}
            </div>
          </div>
          {scope === 'group' && (
            <div>
              <label className="label">选择分组</label>
              <select className="input" value={groupId} onChange={(e) => setGroupId(e.target.value)}>
                {groups.map((g) => (
                  <option key={g.id} value={g.id}>{g.name} ({g.count})</option>
                ))}
              </select>
            </div>
          )}
          {scope === 'selected' && (
            <div className="md:col-span-2">
              <label className="label">勾选账号（{selected.size}/{accounts.length}）</label>
              <div className="max-h-32 overflow-auto rounded-lg border border-slate-200 p-2 text-sm dark:border-slate-700">
                {accounts.map((a) => (
                  <label key={a.user_id} className="flex items-center gap-2 px-1 py-0.5 hover:bg-slate-50 dark:hover:bg-slate-800">
                    <input type="checkbox" checked={selected.has(a.user_id)} onChange={() => toggle(a.user_id)} />
                    <span>{a.name}</span>
                    <span className="text-xs text-slate-400">{a.user_id}</span>
                  </label>
                ))}
              </div>
            </div>
          )}
        </div>
        <div className="mt-3 flex flex-wrap items-center gap-x-5 gap-y-2 text-sm">
          <label className="flex items-center gap-2">
            <input type="checkbox" checked={skipChecked} onChange={(e) => setSkipChecked(e.target.checked)} />
            跳过今日已签
          </label>
          <label className="flex items-center gap-2">
            <input type="checkbox" checked={skipExpired} onChange={(e) => setSkipExpired(e.target.checked)} />
            跳过 JWT 过期
          </label>
          <span className="ml-auto text-xs text-slate-500">
            目标：<b>{candidateIds.length}</b> 个账号
          </span>
        </div>
        <div className="mt-4 flex justify-end">
          <button
            onClick={start}
            disabled={checkin.active || candidateIds.length === 0}
            className="btn-primary"
          >
            <PlayCircle size={16} /> {checkin.active ? '签到进行中…' : '开始签到'}
          </button>
        </div>
      </div>

      {checkin.active || checkin.total > 0 ? (
        <div className="card p-4">
          <div className="mb-3 flex items-center justify-between">
            <h3 className="font-medium">实时进度</h3>
            {checkin.active ? (
              <Badge tone="blue">运行中</Badge>
            ) : checkin.done ? (
              <Badge tone={checkin.done.failed > 0 ? 'amber' : 'green'}>
                完成：成功 {checkin.done.ok}，已签 {checkin.done.already}，失败 {checkin.done.failed}
              </Badge>
            ) : null}
          </div>
          <div className="mb-3">
            <Progress value={checkin.index} max={checkin.total || 1} />
            <div className="mt-1 text-xs text-slate-500">
              {checkin.index}/{checkin.total}
            </div>
          </div>
          <div className="max-h-80 space-y-1 overflow-auto">
            {Array.from({ length: checkin.total }).map((_, i) => {
              const r = checkin.results[i];
              if (!r) {
                return (
                  <div key={i} className="flex items-center gap-2 rounded border border-slate-200 px-3 py-2 text-sm dark:border-slate-700">
                    <Clock size={14} className="text-slate-400" />
                    <span className="text-slate-400">等待中…</span>
                  </div>
                );
              }
              const tone =
                r.status === 'success'
                  ? 'text-emerald-600 dark:text-emerald-300'
                  : r.status === 'already'
                  ? 'text-sky-600 dark:text-sky-300'
                  : r.status === 'fail'
                  ? 'text-rose-600 dark:text-rose-300'
                  : 'text-slate-500';
              const Icon =
                r.status === 'success'
                  ? CheckCircle2
                  : r.status === 'already'
                  ? CheckCircle2
                  : r.status === 'fail'
                  ? XCircle
                  : AlertCircle;
              return (
                <div key={i} className="flex items-center gap-2 rounded border border-slate-200 px-3 py-2 text-sm dark:border-slate-700">
                  <Icon size={14} className={tone} />
                  <span className="w-8 text-right text-xs text-slate-400">{r.index}</span>
                  <span className="flex-1 truncate">{r.name}</span>
                  <span className={`text-xs ${tone}`}>
                    {r.status === 'success' && `+${r.delta ?? 0} (余额 ${r.credits ?? '?'})`}
                    {r.status === 'already' && `已签 (余额 ${r.credits ?? '?'})`}
                    {r.status === 'fail' && (r.message ?? '失败')}
                  </span>
                  {r.elapsed != null && <span className="text-xs text-slate-400">{r.elapsed.toFixed(1)}s</span>}
                </div>
              );
            })}
          </div>
        </div>
      ) : null}

      <div className="mt-5 text-xs text-slate-400">
        提示：如跳过规则默认开启，可在「设置」中调整。
        开关项遵循 settings.checkin_skip_checked 与 settings.checkin_skip_expired（{settings ? '已生效' : '加载中'}）。
      </div>
    </div>
  );
}