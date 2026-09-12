import { useEffect, useState } from 'react';
import { Modal } from '../../components/ui';
import { api } from '../../lib/tauri';
import type { AccountView, JwtParseResult } from '../../types';

export function EditAccountModal({
  account,
  onClose,
  onSubmit,
}: {
  account: AccountView | null;
  onClose: () => void;
  onSubmit: (name?: string, jwt?: string) => Promise<void>;
}) {
  const [name, setName] = useState('');
  const [jwt, setJwt] = useState('');
  const [info, setInfo] = useState<JwtParseResult | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (account) {
      setName(account.name);
      setJwt('');
      setInfo(null);
      setBusy(false);
    }
  }, [account]);

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
    if (!account) return;
    const nameChanged = name.trim() && name.trim() !== account.name;
    const jwtChanged = jwt.trim().length > 0;
    if (!nameChanged && !jwtChanged) {
      onClose();
      return;
    }
    setBusy(true);
    try {
      await onSubmit(
        nameChanged ? name.trim() : undefined,
        jwtChanged ? jwt.trim() : undefined,
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      open={!!account}
      onClose={onClose}
      title="编辑账号"
      footer={
        <>
          <button onClick={onClose} className="btn-ghost">取消</button>
          <button onClick={submit} disabled={busy} className="btn-primary">
            {busy ? '保存中…' : '保存'}
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
          <label className="label">新 JWT（留空则不修改）</label>
          <textarea
            value={jwt}
            onChange={(e) => setJwt(e.target.value)}
            className="input min-h-[120px] font-mono text-xs"
            placeholder="粘贴新 JWT 以替换（支持 Cloud-IDE-JWT / Bearer 前缀或直接粘贴 token）"
          />
        </div>
        {info && (
          <div className="rounded-lg border border-slate-200 p-3 text-xs dark:border-zinc-700">
            <div>新 JWT 解析结果：</div>
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
        {jwt.trim() && info && info.user_id && account && info.user_id !== account.user_id && (
          <div className="rounded-lg border border-amber-300 bg-amber-50 p-3 text-xs text-amber-700 dark:border-amber-700 dark:bg-amber-900/20 dark:text-amber-300">
            ⚠️ 新 JWT 的 UserID（{info.user_id}）与当前账号（{account.user_id}）不同，保存后 user_id 将更新。
          </div>
        )}
      </div>
    </Modal>
  );
}
