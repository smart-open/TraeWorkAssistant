import { useState } from 'react';
import { Eye, EyeOff, KeyRound, Loader2, LogIn } from 'lucide-react';
import { login, ApiError } from '../lib/tauri';
import { useAppStore } from '../store';
import { APP_NAME } from '../lib/about';
import { BrandMark } from '../components/BrandMark';

/**
 * 管理面登录页（ADR-4）：输入 admin token 换取 HttpOnly 会话 cookie。
 * token 来源：服务器 `conf/admin_token` 文件（首启自动生成）或 `AIWORK_ADMIN_TOKEN` 环境变量。
 */
export default function Login() {
  const afterLogin = useAppStore((s) => s.afterLogin);
  const [token, setToken] = useState('');
  const [showToken, setShowToken] = useState(false);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    const t = token.trim();
    if (!t || busy) return;
    setBusy(true);
    setErr(null);
    try {
      await login(t);
      useAppStore.setState({ authed: true });
      await afterLogin();
    } catch (e2) {
      setErr(e2 instanceof ApiError ? e2.message : String(e2));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex min-h-screen items-center justify-center bg-slate-100 px-4 dark:bg-zinc-950">
      <div className="w-full max-w-sm">
        <div className="mb-6 flex flex-col items-center gap-3">
          <BrandMark size={44} iconSize={26} />
          <div className="text-center">
            <h1 className="text-lg font-semibold text-slate-800 dark:text-zinc-100">{APP_NAME}</h1>
            <p className="mt-1 text-xs text-slate-500">请输入管理令牌登录控制台</p>
          </div>
        </div>

        <form
          onSubmit={(e) => void submit(e)}
          className="card space-y-4 p-5"
        >
          <div>
            <label htmlFor="aiwork-admin-token" className="label">
              <KeyRound size={13} className="mr-1 inline align-[-2px]" /> 管理令牌
            </label>
            <div className="relative">
              <input
                id="aiwork-admin-token"
                type={showToken ? 'text' : 'password'}
                value={token}
                onChange={(e) => setToken(e.target.value)}
                placeholder="Admin Token"
                autoFocus
                autoComplete="current-password"
                className="input pr-9 font-mono"
              />
              <button
                type="button"
                onClick={() => setShowToken((v) => !v)}
                className="absolute right-2 top-1/2 -translate-y-1/2 text-slate-400 transition hover:text-slate-600 dark:hover:text-zinc-300"
                aria-label={showToken ? '隐藏令牌' : '显示令牌'}
              >
                {showToken ? <EyeOff size={15} /> : <Eye size={15} />}
              </button>
            </div>
            <p className="mt-1.5 text-xs text-slate-400">
              令牌位于服务器 <code className="rounded bg-slate-100 px-1 py-0.5 font-mono text-[11px] dark:bg-zinc-800">conf/admin_token</code>
              ，或由 <code className="rounded bg-slate-100 px-1 py-0.5 font-mono text-[11px] dark:bg-zinc-800">AIWORK_ADMIN_TOKEN</code> 环境变量指定。
            </p>
          </div>

          {err && (
            <div className="rounded-lg border border-rose-200 bg-rose-50 px-3 py-2 text-xs text-rose-600 dark:border-rose-800 dark:bg-rose-900/20 dark:text-rose-300">
              {err}
            </div>
          )}

          <button type="submit" disabled={busy || !token.trim()} className="btn w-full justify-center bg-amber-500 text-white transition hover:bg-amber-600 disabled:cursor-not-allowed disabled:opacity-50">
            {busy ? <Loader2 size={15} className="animate-spin" /> : <LogIn size={15} />}
            {busy ? '登录中…' : '登录'}
          </button>
        </form>
      </div>
    </div>
  );
}
