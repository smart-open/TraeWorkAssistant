import { useEffect, useMemo, useState } from 'react';
import {
  Plus,
  Trash2,
  RefreshCw,
  RotateCcw,
  LogIn,
  CheckCircle2,
  XCircle,
  AlertTriangle,
  HelpCircle,
} from 'lucide-react';
import PageHeader from '../components/PageHeader';
import { Badge, EmptyState, Modal } from '../components/ui';
import { useAppStore } from '../store';
import { api } from '../lib/tauri';
import type { AccountView, GroupView, JwtParseResult } from '../types';

const PRESET_COLORS = [
  '#6366f1', '#22c55e', '#f59e0b', '#ef4444', '#0ea5e9', '#a855f7', '#14b8a6',
];

function JwtStatusBadge({ hours }: { hours: number | null }) {
  if (hours === null) return <Badge tone="slate"><HelpCircle size={12} /> 未知</Badge>;
  if (hours <= 0) return <Badge tone="red"><XCircle size={12} /> 已过期</Badge>;
  if (hours <= 24) return <Badge tone="amber"><AlertTriangle size={12} /> {hours.toFixed(1)}h</Badge>;
  return <Badge tone="green"><CheckCircle2 size={12} /> {hours.toFixed(0)}h</Badge>;
}

export default function Accounts() {
  const accounts = useAppStore((s) => s.accounts);
  const groups = useAppStore((s) => s.groups);
  const refreshAccounts = useAppStore((s) => s.refreshAccounts);
  const refreshGroups = useAppStore((s) => s.refreshGroups);
  const addAccount = useAppStore((s) => s.addAccount);
  const deleteAccount = useAppStore((s) => s.deleteAccount);
  const createGroup = useAppStore((s) => s.createGroup);
  const updateGroup = useAppStore((s) => s.updateGroup);
  const removeGroup = useAppStore((s) => s.removeGroup);
  const moveAccount = useAppStore((s) => s.moveAccount);
  const resetDevice = useAppStore((s) => s.resetDevice);
  const switchTo = useAppStore((s) => s.switchTo);
  const toast = useAppStore((s) => s.pushToast);

  const [filter, setFilter] = useState<string>('all');
  const [addOpen, setAddOpen] = useState(false);
  const [groupOpen, setGroupOpen] = useState(false);

  const filtered = useMemo(() => {
    if (filter === 'all') return accounts;
    if (filter === 'ungrouped') return accounts.filter((a) => !a.group_id);
    return accounts.filter((a) => a.group_id === filter);
  }, [accounts, filter]);

  useEffect(() => {
    void refreshAccounts();
    void refreshGroups();
  }, [refreshAccounts, refreshGroups]);

  const onDelete = async (a: AccountView) => {
    if (!confirm(`确认删除账号「${a.name}」？${a.device_id_masked ? '（会一并清理设备 ID）' : ''}`)) return;
    await deleteAccount(a.user_id, true);
  };

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="账号管理"
        desc="维护账号、调整分组、重置设备 ID 与登录态切换"
        actions={
          <>
            <button onClick={() => { void refreshAccounts(); void refreshGroups(); }} className="btn-outline">
              <RefreshCw size={15} /> 刷新
            </button>
            <button onClick={() => setGroupOpen(true)} className="btn-outline">
              分组管理
            </button>
            <button onClick={() => setAddOpen(true)} className="btn-primary">
              <Plus size={15} /> 添加账号
            </button>
          </>
        }
      />

      <div className="mb-3 flex flex-wrap items-center gap-2 text-sm">
        <button
          onClick={() => setFilter('all')}
          className={`chip border ${filter === 'all' ? 'border-brand-500 text-brand-600' : 'border-slate-300 text-slate-500'}`}
        >
          全部 ({accounts.length})
        </button>
        <button
          onClick={() => setFilter('ungrouped')}
          className={`chip border ${filter === 'ungrouped' ? 'border-brand-500 text-brand-600' : 'border-slate-300 text-slate-500'}`}
        >
          未分组 ({accounts.filter((a) => !a.group_id).length})
        </button>
        {groups.map((g) => (
          <button
            key={g.id}
            onClick={() => setFilter(g.id)}
            className={`chip border ${filter === g.id ? 'border-brand-500 text-brand-600' : 'border-slate-300 text-slate-500'}`}
            style={{ borderColor: filter === g.id ? g.color : undefined }}
          >
            <span className="inline-block h-2 w-2 rounded-full" style={{ background: g.color }} />
            {g.name} ({g.count})
          </button>
        ))}
      </div>

      <div className="card overflow-hidden">
        {filtered.length === 0 ? (
          <div className="p-6">
            <EmptyState
              icon={<Plus size={28} />}
              title={filter === 'all' ? '还没有账号' : '此分组下没有账号'}
              hint="点击右上角「添加账号」粘贴 JWT，或先启动代理让 TRAE 自动捕获。"
            />
          </div>
        ) : (
          <table className="w-full text-sm">
            <thead className="bg-slate-50 text-xs uppercase text-slate-500 dark:bg-slate-900">
              <tr>
                <th className="px-4 py-2 text-left">账号</th>
                <th className="px-4 py-2 text-left">分组</th>
                <th className="px-4 py-2 text-left">JWT</th>
                <th className="px-4 py-2 text-left">设备 ID</th>
                <th className="px-4 py-2 text-left">今日</th>
                <th className="px-4 py-2 text-right">积分</th>
                <th className="px-4 py-2 text-right">操作</th>
              </tr>
            </thead>
            <tbody>
              {filtered.map((a) => {
                const g = groups.find((x) => x.id === a.group_id);
                return (
                  <tr key={a.user_id} className="border-t border-slate-200 dark:border-slate-800">
                    <td className="px-4 py-3">
                      <div className="font-medium">{a.name}</div>
                      <div className="text-xs text-slate-400">{a.user_id}</div>
                    </td>
                    <td className="px-4 py-3">
                      <GroupSelect
                        value={a.group_id}
                        groups={groups}
                        onChange={(gid) => void moveAccount(a.user_id, gid)}
                      />
                    </td>
                    <td className="px-4 py-3">
                      <JwtStatusBadge hours={a.jwt_exp_hours} />
                    </td>
                    <td className="px-4 py-3 font-mono text-xs text-slate-500">{a.device_id_masked ?? '—'}</td>
                    <td className="px-4 py-3">
                      {a.checked_today ? (
                        <Badge tone="green">已签</Badge>
                      ) : (
                        <Badge tone="slate">未签</Badge>
                      )}
                    </td>
                    <td className="px-4 py-3 text-right tabular-nums">
                      {a.credits != null ? a.credits.toLocaleString() : '—'}
                    </td>
                    <td className="px-4 py-3">
                      <div className="flex justify-end gap-1">
                        <button title="切换到此账号" onClick={() => void switchTo(a.user_id)} className="btn-ghost !p-2">
                          <LogIn size={14} />
                        </button>
                        <button title="重置设备 ID" onClick={() => void resetDevice(a.user_id)} className="btn-ghost !p-2">
                          <RotateCcw size={14} />
                        </button>
                        <button title="删除" onClick={() => void onDelete(a)} className="btn-ghost !p-2 text-rose-500 hover:bg-rose-50 dark:hover:bg-rose-500/10">
                          <Trash2 size={14} />
                        </button>
                      </div>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        )}
      </div>

      <AddAccountModal
        open={addOpen}
        onClose={() => setAddOpen(false)}
        groups={groups}
        onSubmit={async (name, jwt, gid) => {
          try {
            await addAccount(name, jwt, gid);
            setAddOpen(false);
          } catch {
            /* toast 已发出 */
          }
        }}
      />
      <GroupsModal
        open={groupOpen}
        onClose={() => setGroupOpen(false)}
        groups={groups}
        onCreate={async (name, color) => {
          await createGroup(name, color);
        }}
        onRename={async (id, name) => {
          await updateGroup(id, { name });
        }}
        onRecolor={async (id, color) => {
          await updateGroup(id, { color });
        }}
        onDelete={async (id) => {
          await removeGroup(id);
        }}
      />
    </div>
  );
}

function GroupSelect({
  value,
  groups,
  onChange,
}: {
  value: string | null;
  groups: GroupView[];
  onChange: (gid: string | null) => void;
}) {
  return (
    <select
      value={value ?? ''}
      onChange={(e) => onChange(e.target.value || null)}
      className="input !py-1 !text-xs w-32"
    >
      <option value="">未分组</option>
      {groups.map((g) => (
        <option key={g.id} value={g.id}>
          {g.name}
        </option>
      ))}
    </select>
  );
}

function AddAccountModal({
  open,
  onClose,
  groups,
  onSubmit,
}: {
  open: boolean;
  onClose: () => void;
  groups: GroupView[];
  onSubmit: (name: string, jwt: string, groupId?: string) => Promise<void>;
}) {
  const [name, setName] = useState('');
  const [jwt, setJwt] = useState('');
  const [gid, setGid] = useState<string>('');
  const [info, setInfo] = useState<JwtParseResult | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) {
      setName('');
      setJwt('');
      setGid('');
      setInfo(null);
      setBusy(false);
    }
  }, [open]);

  // 输入 JWT 时自动解析
  useEffect(() => {
    const v = jwt.trim();
    if (!v) {
      setInfo(null);
      return;
    }
    let cancel = false;
    const t = setTimeout(async () => {
      try {
        const r = await api.misc.jwtParse(v);
        if (!cancel) setInfo(r);
      } catch {
        if (!cancel) setInfo({ user_id: null, exp_hours: null, status: 'unknown' });
      }
    }, 200);
    return () => {
      cancel = true;
      clearTimeout(t);
    };
  }, [jwt]);

  const submit = async () => {
    if (!name.trim() || !jwt.trim()) return;
    setBusy(true);
    try {
      await onSubmit(name.trim(), jwt.trim(), gid || undefined);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      open={open}
      onClose={onClose}
      title="添加账号"
      footer={
        <>
          <button onClick={onClose} className="btn-ghost">取消</button>
          <button onClick={submit} disabled={busy || !name || !jwt} className="btn-primary">
            {busy ? '添加中…' : '添加'}
          </button>
        </>
      }
    >
      <div className="space-y-3">
        <div>
          <label className="label">账号备注名</label>
          <input
            value={name}
            onChange={(e) => setName(e.target.value)}
            className="input"
            placeholder="例如：me_1676"
          />
        </div>
        <div>
          <label className="label">JWT（支持「Cloud-IDE-JWT 」前缀）</label>
          <textarea
            value={jwt}
            onChange={(e) => setJwt(e.target.value)}
            className="input min-h-[120px] font-mono text-xs"
            placeholder="Cloud-IDE-JWT eyJ..."
          />
        </div>
        <div>
          <label className="label">分组（可选）</label>
          <select value={gid} onChange={(e) => setGid(e.target.value)} className="input">
            <option value="">不分组</option>
            {groups.map((g) => (
              <option key={g.id} value={g.id}>{g.name}</option>
            ))}
          </select>
        </div>
        {info && (
          <div className="rounded-lg border border-slate-200 p-3 text-xs dark:border-slate-700">
            <div>解析结果：</div>
            <div className="mt-1 grid grid-cols-2 gap-1">
              <span className="text-slate-500">UserID</span>
              <span className="font-mono">{info.user_id ?? '无法识别'}</span>
              <span className="text-slate-500">剩余</span>
              <span>{info.exp_hours != null ? `${info.exp_hours.toFixed(1)} 小时` : '—'}</span>
              <span className="text-slate-500">状态</span>
              <span>
                {info.status === 'ok' && '✅ 健康'}
                {info.status === 'warn' && '⚠️ 即将过期'}
                {info.status === 'expired' && '❌ 已过期'}
                {info.status === 'unknown' && '❓ 无法解析'}
              </span>
            </div>
          </div>
        )}
      </div>
    </Modal>
  );
}

function GroupsModal({
  open,
  onClose,
  groups,
  onCreate,
  onRename,
  onRecolor,
  onDelete,
}: {
  open: boolean;
  onClose: () => void;
  groups: GroupView[];
  onCreate: (name: string, color: string) => Promise<void>;
  onRename: (id: string, name: string) => Promise<void>;
  onRecolor: (id: string, color: string) => Promise<void>;
  onDelete: (id: string) => Promise<void>;
}) {
  const [name, setName] = useState('');
  const [color, setColor] = useState(PRESET_COLORS[0]);
  return (
    <Modal
      open={open}
      onClose={onClose}
      title="分组管理"
      footer={
        <>
          <button onClick={onClose} className="btn-ghost">关闭</button>
          <button
            onClick={async () => {
              if (!name.trim()) return;
              await onCreate(name.trim(), color);
              setName('');
            }}
            className="btn-primary"
          >
            <Plus size={14} /> 新建
          </button>
        </>
      }
    >
      <div className="mb-4 space-y-3">
        <div>
          <label className="label">新分组名称</label>
          <input value={name} onChange={(e) => setName(e.target.value)} className="input" placeholder="例如：工作 / 私人" />
        </div>
        <div>
          <label className="label">颜色</label>
          <div className="flex flex-wrap gap-2">
            {PRESET_COLORS.map((c) => (
              <button
                key={c}
                type="button"
                onClick={() => setColor(c)}
                className={`h-6 w-6 rounded-full border-2 ${color === c ? 'border-slate-900 dark:border-white' : 'border-transparent'}`}
                style={{ background: c }}
                aria-label={c}
              />
            ))}
          </div>
        </div>
      </div>
      <div className="max-h-64 space-y-2 overflow-auto">
        {groups.length === 0 && <div className="text-xs text-slate-400">暂无分组</div>}
        {groups.map((g) => (
          <div key={g.id} className="flex items-center gap-2 rounded-lg border border-slate-200 p-2 dark:border-slate-700">
            <span className="inline-block h-4 w-4 rounded-full" style={{ background: g.color }} />
            <input
              defaultValue={g.name}
              onBlur={(e) => {
                if (e.target.value && e.target.value !== g.name) void onRename(g.id, e.target.value);
              }}
              className="input !py-1 flex-1 !text-xs"
            />
            <select
              defaultValue={g.color}
              onChange={(e) => void onRecolor(g.id, e.target.value)}
              className="input !py-1 !text-xs w-24"
            >
              {PRESET_COLORS.map((c) => (
                <option key={c} value={c}>{c}</option>
              ))}
            </select>
            <span className="text-xs text-slate-400">{g.count}</span>
            <button
              onClick={() => {
                if (confirm(`删除分组「${g.name}」？该分组下账号会回落为「未分组」。`))
                  void onDelete(g.id);
              }}
              className="btn-ghost !p-2 text-rose-500"
              aria-label="删除"
            >
              <Trash2 size={14} />
            </button>
          </div>
        ))}
      </div>
    </Modal>
  );
}