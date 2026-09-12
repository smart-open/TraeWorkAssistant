import { useEffect, useState } from 'react';
import { Copy } from 'lucide-react';
import { Modal } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import type { AccountView } from '../../types';

export function JwtViewModal({
  account,
  onClose,
  onCopy,
}: {
  account: AccountView | null;
  onClose: () => void;
  onCopy: (jwt: string) => void;
}) {
  const toast = useAppStore((s) => s.pushToast);
  // 列表接口只回掩码 JWT：打开弹窗时按需取完整值（失败 toast，可关闭重开重试）
  const [jwt, setJwt] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    if (!account) {
      setJwt(null);
      return;
    }
    let cancel = false;
    setLoading(true);
    setJwt(null);
    api.accounts
      .getJwt(account.user_id)
      .then((v) => {
        if (!cancel) setJwt(v);
      })
      .catch((err) => {
        if (cancel) return;
        setJwt(null);
        toast('error', `读取 JWT 失败：${String(err)}`);
      })
      .finally(() => {
        if (!cancel) setLoading(false);
      });
    return () => {
      cancel = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [account?.user_id]);

  return (
    <Modal
      open={!!account}
      onClose={onClose}
      title={`JWT - ${account?.name ?? ''}`}
      footer={
        <>
          <button onClick={onClose} className="btn-ghost">关闭</button>
          <button
            onClick={() => jwt && onCopy(jwt)}
            disabled={loading || !jwt}
            className="btn-primary disabled:cursor-not-allowed disabled:opacity-50"
          >
            <Copy size={14} /> 复制 JWT
          </button>
        </>
      }
    >
      <div className="space-y-3">
        <div className="grid grid-cols-2 gap-2 text-xs">
          <span className="text-slate-500">UserID</span>
          <span className="font-mono">{account?.user_id}</span>
          <span className="text-slate-500">JWT 剩余</span>
          <span>
            {account?.jwt_exp_hours != null
              ? `${account.jwt_exp_hours.toFixed(1)} 小时`
              : '未知'}
          </span>
          <span className="text-slate-500">过期时间</span>
          <span>
            {account?.jwt_exp_timestamp != null
              ? new Date(account.jwt_exp_timestamp * 1000).toLocaleString('zh-CN')
              : '未知'}
          </span>
        </div>
        <div>
          <label className="label">JWT 原文</label>
          <textarea
            readOnly
            value={jwt ?? ''}
            placeholder={loading ? '加载中…' : '加载失败，请关闭后重试'}
            className="input min-h-[160px] font-mono text-xs"
            onClick={(e) => (e.target as HTMLTextAreaElement).select()}
          />
        </div>
      </div>
    </Modal>
  );
}
