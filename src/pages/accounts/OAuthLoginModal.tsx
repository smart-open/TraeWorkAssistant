import { useEffect, useState } from 'react';
import { Copy, ExternalLink } from 'lucide-react';
import { Modal } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { copyText } from '../../lib/clipboard';
import type { GroupView } from '../../types';

/**
 * OAuth 登录弹窗（Web 版·纯粘贴模式，ADR-3）：
 * 浏览器无法监听本机回环回调，改为三步引导——
 * ① 打开 Trae 授权页 → ② 浏览器完成登录（回调页「无法连接」为预期现象）→ ③ 粘贴地址栏完整 URL 提交。
 * 服务端走既有 oauth.parse_callback + oauth_login 流程落库。
 */
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
  const [callbackUrl, setCallbackUrl] = useState('');
  const [accountName, setAccountName] = useState('');
  const [gid, setGid] = useState('');
  const [busy, setBusy] = useState(false);
  const [opening, setOpening] = useState(false);
  const [loginUrl, setLoginUrl] = useState('');
  const toast = useAppStore((s) => s.pushToast);

  useEffect(() => {
    if (!open) {
      setCallbackUrl('');
      setAccountName('');
      setGid('');
      setBusy(false);
      setOpening(false);
      setLoginUrl('');
    }
  }, [open]);

  // 步骤①：获取授权页链接并新窗口打开（链接保留在状态里供「复制链接」复用）
  const openLoginPage = async () => {
    setOpening(true);
    try {
      const { url } = await api.oauth.getLoginUrl();
      setLoginUrl(url);
      window.open(url, '_blank', 'noopener');
    } catch (err) {
      toast('error', `获取登录 URL 失败：${String(err)}`);
    } finally {
      setOpening(false);
    }
  };

  const copyLoginUrl = async () => {
    if (!loginUrl) return;
    if (await copyText(loginUrl)) {
      toast('success', '授权链接已复制到剪贴板');
    } else {
      toast('error', '复制失败，请手动选择文本复制');
    }
  };

  // 步骤③：粘贴浏览器地址栏完整回调 URL 提交（走既有 parse_callback + login 流程）
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
          <button onClick={onClose} className="btn-ghost" disabled={busy || opening}>
            取消
          </button>
          <button
            onClick={finish}
            disabled={busy || !callbackUrl.trim()}
            className="btn-primary"
          >
            {busy ? '登录中...' : '提交完成登录'}
          </button>
        </>
      }
    >
      <div className="space-y-4">
        {/* 三步说明 */}
        <ol className="space-y-2.5 text-sm text-slate-600 dark:text-zinc-300">
          <li className="flex items-start gap-2">
            <span className="mt-0.5 flex h-5 w-5 shrink-0 items-center justify-center rounded-full bg-brand-500 text-[11px] font-semibold text-white">1</span>
            <span className="flex-1">
              点击下方按钮在新窗口打开 Trae 授权页（或复制链接后自行打开）。
              <span className="mt-1.5 flex flex-wrap items-center gap-2">
                <button onClick={() => void openLoginPage()} disabled={opening} className="btn-outline !py-1 text-xs">
                  {opening ? '正在获取…' : '打开授权页'} <ExternalLink size={13} />
                </button>
                {loginUrl && (
                  <button onClick={() => void copyLoginUrl()} className="btn-ghost !py-1 text-xs">
                    <Copy size={13} /> 复制链接
                  </button>
                )}
              </span>
            </span>
          </li>
          <li className="flex items-start gap-2">
            <span className="mt-0.5 flex h-5 w-5 shrink-0 items-center justify-center rounded-full bg-brand-500 text-[11px] font-semibold text-white">2</span>
            <span>
              在浏览器中完成 Trae 账号登录授权。授权完成后回调页会显示
              <b className="text-amber-600 dark:text-amber-400">「无法连接 / 无法访问此网站」</b>
              ，这是预期现象，不影响登录结果。
            </span>
          </li>
          <li className="flex items-start gap-2">
            <span className="mt-0.5 flex h-5 w-5 shrink-0 items-center justify-center rounded-full bg-brand-500 text-[11px] font-semibold text-white">3</span>
            <span>复制浏览器地址栏的完整 URL，粘贴到下方输入框提交。</span>
          </li>
        </ol>

        {/* 账号备注与分组 */}
        <div className="grid gap-3 sm:grid-cols-2">
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

        {/* 回调 URL 粘贴框 */}
        <div>
          <label className="label">回调 URL</label>
          <textarea
            value={callbackUrl}
            onChange={(e) => setCallbackUrl(e.target.value)}
            className="input min-h-[100px] font-mono text-xs"
            placeholder="http://127.0.0.1:17388/authorize?refreshToken=..."
          />
          <p className="mt-2 text-xs text-slate-400">
            粘贴浏览器地址栏完整 URL（含 refreshToken 参数），点击「提交完成登录」后账号将自动录入
          </p>
        </div>
      </div>
    </Modal>
  );
}
