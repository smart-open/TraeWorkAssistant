import { useEffect, useState } from 'react';
import { Modal } from '../../components/ui';
import { api } from '../../lib/tauri';
import type { GroupView, JwtParseResult } from '../../types';

export function AddAccountModal({
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
        if (!cancel) setInfo({ user_id: null, exp_hours: null, exp_timestamp: null, status: 'unknown' });
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
          <label className="label">JWT（支持「Cloud-IDE-JWT 」/「Bearer 」前缀，或直接粘贴 token）</label>
          <textarea
            value={jwt}
            onChange={(e) => setJwt(e.target.value)}
            className="input min-h-[120px] font-mono text-xs"
            placeholder="eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9..."
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
          <div className="rounded-lg border border-slate-200 p-3 text-xs dark:border-zinc-700">
            <div>解析结果：</div>
            <div className="mt-1 grid grid-cols-2 gap-1">
              <span className="text-slate-500">UserID</span>
              <span className="font-mono">{info.user_id ?? '无法识别'}</span>
              <span className="text-slate-500">剩余</span>
              <span>{info.exp_hours != null ? `${info.exp_hours.toFixed(1)} 小时` : '-'}</span>
              <span className="text-slate-500">过期时间</span>
              <span>{info.exp_timestamp != null ? new Date(info.exp_timestamp * 1000).toLocaleString('zh-CN') : '-'}</span>
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
