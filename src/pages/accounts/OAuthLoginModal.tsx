import { useEffect, useRef, useState } from 'react';
import { CheckCircle2, ExternalLink } from 'lucide-react';
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
  /** 回环监听是否可用（端口被占用等场景降级为手动粘贴兜底） */
  const [autoReady, setAutoReady] = useState(true);
  const toast = useAppStore((s) => s.pushToast);
  const refreshAccounts = useAppStore((s) => s.refreshAccounts);
  const refreshGroups = useAppStore((s) => s.refreshGroups);
  // onClose 为父组件内联函数（每次渲染变化），经 ref 引用避免监听器反复重建
  const onCloseRef = useRef(onClose);
  onCloseRef.current = onClose;

  useEffect(() => {
    if (!open) {
      setStep(1);
      setCallbackUrl('');
      setAccountName('');
      setGid('');
      setBusy(false);
      setOpening(false);
      setAutoReady(true);
    }
  }, [open]);

  // F-78 批次 1：监听回环监听器的 oauth-login-done 事件，自动收尾落库结果。
  // 后端已在本机回调时完成 oauth_login 落库，这里只刷新列表并关闭弹窗
  //（不能重复调 onLogin，refresh_token 已轮换时二次提交会失败）
  useEffect(() => {
    if (!open) return;
    let disposed = false;
    let un: (() => void) | null = null;
    void api.oauth
      .onLoginDone((e) => {
        if (e.ok) {
          void api.oauth.stopLoopback().catch(() => {});
          void refreshAccounts();
          void refreshGroups();
          // 同 uid 合并场景下后端 message 已如实描述「更新已有账号而非新增」，直接透传（issue #80）
          if (e.merged) {
            toast('success', e.message);
          } else {
            toast('success', e.account ? `账号已添加：${e.account}` : '账号已添加');
          }
          onCloseRef.current();
        } else {
          toast('error', `自动登录失败：${e.message}，可改用下方手动粘贴兜底`);
        }
      })
      .then((fn) => {
        if (disposed) fn();
        else un = fn;
      })
      .catch(() => {});
    return () => {
      disposed = true;
      un?.();
      // 弹窗关闭/组件卸载：停止回环监听并还原代理豁免（未启动时后端幂等成功）
      void api.oauth.stopLoopback().catch(() => {});
    };
  }, [open, toast, refreshAccounts, refreshGroups]);

  const openLoginPage = async () => {
    setOpening(true);
    try {
      // 先起本机回环监听（自动收尾回调），再打开浏览器登录页；
      // 端口被占用等失败不阻断流程，降级为手动粘贴兜底
      try {
        await api.oauth.startLoopback(accountName.trim() || undefined, gid || undefined);
        setAutoReady(true);
      } catch (err) {
        setAutoReady(false);
        toast('warn', `自动回调不可用：${String(err)}`);
      }
      const { url } = await api.oauth.getLoginUrl();
      const { open: openUrl } = await import('@tauri-apps/plugin-shell');
      await openUrl(url);
      setStep(2);
    } catch (err) {
      toast('error', `获取登录 URL 失败：${String(err)}`);
    } finally {
      setOpening(false);
    }
  };

  // 手动粘贴兜底：浏览器地址栏复制完整回调 URL 提交（与自动收尾互为补充）
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
              onClick={finish}
              disabled={busy || !callbackUrl.trim()}
              className="btn-primary"
            >
              {busy ? '登录中...' : '手动完成登录'}
            </button>
          )}
        </>
      }
    >
      <div className="space-y-4">
        {/* 步骤指示器（两步：填写并打开登录页 → 完成登录） */}
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
        </div>

        {step === 1 && (
          <div className="space-y-3">
            <p className="text-sm text-slate-600 dark:text-zinc-300">
              在浏览器完成 Trae 账号登录授权后，账号将自动添加并关闭本窗口（也可在下一步手动粘贴回调 URL 兜底）。
            </p>
            <p className="rounded-md bg-emerald-50 px-3 py-2 text-xs text-emerald-700 dark:bg-emerald-500/10 dark:text-emerald-400">
              OAuth 登录不走 MITM 代理（已自动直连豁免），浏览器不会出现证书告警。
            </p>
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

        {step === 2 && (
          <div className="space-y-3">
            {autoReady ? (
              <div className="flex items-start gap-2 rounded-md bg-sky-50 px-3 py-2 text-sm text-sky-700 dark:bg-sky-500/10 dark:text-sky-400">
                <CheckCircle2 size={16} className="mt-0.5 shrink-0" />
                <span>
                  已在浏览器打开登录页，完成授权后账号将自动添加并关闭本窗口，无需手动操作。
                </span>
              </div>
            ) : (
              <div className="rounded-md bg-amber-50 px-3 py-2 text-sm text-amber-700 dark:bg-amber-500/10 dark:text-amber-400">
                自动回调不可用（监听端口被占用），请在浏览器完成登录后，将地址栏完整 URL 粘贴到下方手动完成。
              </div>
            )}
            <div>
              <label className="label">回调 URL（手动兜底）</label>
              <textarea
                value={callbackUrl}
                onChange={(e) => setCallbackUrl(e.target.value)}
                className="input min-h-[100px] font-mono text-xs"
                placeholder="http://127.0.0.1:17388/authorize?refreshToken=..."
              />
              <p className="mt-2 text-xs text-slate-400">
                若自动收尾失败（或浏览器跳转页面未自动完成），复制浏览器地址栏完整 URL 粘贴到此处，点击「手动完成登录」兜底
              </p>
            </div>
          </div>
        )}
      </div>
    </Modal>
  );
}
