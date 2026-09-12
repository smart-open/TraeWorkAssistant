import { useEffect, useState } from 'react';
import { ArrowRight, ExternalLink } from 'lucide-react';
import { Modal } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import type { GroupView } from '../../types';

export function OAuthLoginModal({
  open,
  onClose,
  groups,
  onLogin,
}: {
  open: boolean;
  onClose: () => void;
  groups: GroupView[];
  onLogin: (
    callbackUrl: string,
    accountName?: string,
    groupId?: string,
  ) => Promise<void>;
}) {
  const [step, setStep] = useState(1);
  const [callbackUrl, setCallbackUrl] = useState('');
  const [accountName, setAccountName] = useState('');
  const [gid, setGid] = useState('');
  const [busy, setBusy] = useState(false);
  const [opening, setOpening] = useState(false);
  const toast = useAppStore((s) => s.pushToast);

  useEffect(() => {
    if (!open) {
      setStep(1);
      setCallbackUrl('');
      setAccountName('');
      setGid('');
      setBusy(false);
      setOpening(false);
    }
  }, [open]);

  const openLoginPage = async () => {
    setOpening(true);
    try {
      const { url } = await api.oauth.getLoginUrl();
      const { open } = await import('@tauri-apps/plugin-shell');
      await open(url);
      setStep(2);
    } catch (err) {
      toast('error', `获取登录 URL 失败：${String(err)}`);
    } finally {
      setOpening(false);
    }
  };

  const finish = async () => {
    if (!callbackUrl.trim()) return;
    setBusy(true);
    try {
      await onLogin(
        callbackUrl.trim(),
        accountName.trim() || undefined,
        gid || undefined,
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      open={open}
      onClose={onClose}
      title="OAuth 登录"
      footer={
        <>
          {step > 1 && (
            <button
              onClick={() => setStep((s) => Math.max(1, s - 1))}
              className="btn-ghost"
              disabled={busy || opening}
            >
              上一步
            </button>
          )}
          <button onClick={onClose} className="btn-ghost" disabled={busy || opening}>
            取消
          </button>
          {step === 1 && (
            <button
              onClick={openLoginPage}
              disabled={opening}
              className="btn-primary"
            >
              {opening ? '正在打开...' : '打开登录页'}
              {!opening && <ExternalLink size={14} />}
            </button>
          )}
          {step === 2 && (
            <button
              onClick={() => setStep(3)}
              disabled={!callbackUrl.trim()}
              className="btn-primary"
            >
              下一步 <ArrowRight size={14} />
            </button>
          )}
          {step === 3 && (
            <button
              onClick={finish}
              disabled={busy || !callbackUrl.trim()}
              className="btn-primary"
            >
              {busy ? '登录中...' : '完成登录'}
            </button>
          )}
        </>
      }
    >
      <div className="space-y-4">
        {/* 步骤指示器 */}
        <div className="flex items-center gap-2">
          <div
            className={`flex h-7 w-7 items-center justify-center rounded-full text-xs font-semibold ${
              step >= 1
                ? 'bg-brand-500 text-white'
                : 'bg-slate-200 text-slate-500 dark:bg-zinc-700'
            }`}
          >
            1
          </div>
          <div
            className={`h-0.5 w-8 ${step > 1 ? 'bg-brand-500' : 'bg-slate-200 dark:bg-zinc-700'}`}
          />
          <div
            className={`flex h-7 w-7 items-center justify-center rounded-full text-xs font-semibold ${
              step >= 2
                ? 'bg-brand-500 text-white'
                : 'bg-slate-200 text-slate-500 dark:bg-zinc-700'
            }`}
          >
            2
          </div>
          <div
            className={`h-0.5 w-8 ${step > 2 ? 'bg-brand-500' : 'bg-slate-200 dark:bg-zinc-700'}`}
          />
          <div
            className={`flex h-7 w-7 items-center justify-center rounded-full text-xs font-semibold ${
              step >= 3
                ? 'bg-brand-500 text-white'
                : 'bg-slate-200 text-slate-500 dark:bg-zinc-700'
            }`}
          >
            3
          </div>
        </div>

        {step === 1 && (
          <div className="text-sm text-slate-600 dark:text-zinc-300">
            点击「打开登录页」在浏览器中发起 OAuth 登录，完成后将自动进入下一步。
          </div>
        )}

        {step === 2 && (
          <div>
            <label className="label">回调 URL</label>
            <textarea
              value={callbackUrl}
              onChange={(e) => setCallbackUrl(e.target.value)}
              className="input min-h-[100px] font-mono text-xs"
              placeholder="http://127.0.0.1:17388/authorize?code=..."
            />
            <p className="mt-2 text-xs text-slate-400">
              登录完成后，浏览器会跳转到 http://127.0.0.1:17388/authorize?... 页面（页面可能显示无法访问），请将地址栏完整 URL 复制粘贴到此处
            </p>
          </div>
        )}

        {step === 3 && (
          <div className="space-y-3">
            <div>
              <label className="label">账号备注名（可选）</label>
              <input
                value={accountName}
                onChange={(e) => setAccountName(e.target.value)}
                className="input"
                placeholder="例如：me_1676"
              />
            </div>
            <div>
              <label className="label">分组（可选）</label>
              <select
                value={gid}
                onChange={(e) => setGid(e.target.value)}
                className="input"
              >
                <option value="">不分组</option>
                {groups.map((g) => (
                  <option key={g.id} value={g.id}>
                    {g.name}
                  </option>
                ))}
              </select>
            </div>
          </div>
        )}
      </div>
    </Modal>
  );
}
