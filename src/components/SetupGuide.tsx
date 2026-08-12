import { useState } from 'react';
import { CheckCircle2, Circle, ChevronRight } from 'lucide-react';
import { useAppStore } from '../store';
import { api } from '../lib/tauri';
import { Badge } from './ui';

interface Step {
  key: string;
  title: string;
  desc: string;
  done: boolean;
  actionLabel: string;
  run: () => Promise<void> | void;
}

export default function SetupGuide() {
  const env = useAppStore((s) => s.env);
  const certInstalled = useAppStore((s) => s.certInstalled);
  const proxy = useAppStore((s) => s.proxy);
  const accounts = useAppStore((s) => s.accounts);
  const setView = useAppStore((s) => s.setView);
  const startProxy = useAppStore((s) => s.startProxy);
  const refreshEnv = useAppStore((s) => s.refreshEnv);
  const refreshCert = useAppStore((s) => s.refreshCert);

  const [busy, setBusy] = useState<string | null>(null);

  // 每一步的「完成」判定均来自 store 实时状态，组件重渲染时自动刷新
  const steps: Step[] = [
    {
      key: 'install',
      title: '安装 Trae 客户端',
      desc: '签到目标客户端，需先安装并登录至少一个账号。',
      done: !!env?.installed,
      actionLabel: '前往下载',
      run: async () => {
        await api.env.openSite();
        await refreshEnv();
      },
    },
    {
      key: 'cert',
      title: '安装 CA 证书',
      desc: '代理需系统信任其证书，才能拦截并改写签到接口。',
      done: certInstalled,
      actionLabel: '安装证书',
      run: async () => {
        await api.cert.install();
        await refreshCert();
      },
    },
    {
      key: 'proxy',
      title: '启动代理',
      desc: '代理负责捕获登录态、转发并拦截签到请求。',
      done: proxy.running,
      actionLabel: '启动代理',
      run: async () => {
        await startProxy();
      },
    },
    {
      key: 'account',
      title: '添加账号',
      desc: '至少添加一个 Trae 账号，才能执行签到。',
      done: accounts.length > 0,
      actionLabel: '去添加',
      run: () => setView('accounts'),
    },
    {
      key: 'checkin',
      title: '完成首次签到',
      desc: '验证整条链路（代理 → 签到 → 浏览器）是否跑通。',
      done: accounts.some((a) => a.checked_today),
      actionLabel: '去签到',
      run: () => setView('checkin'),
    },
  ];

  const completed = steps.filter((s) => s.done).length;
  const allDone = completed === steps.length;

  // 未完成 + 当前非 busy 才允许执行；校验失败（done=true）时按钮不渲染，天然禁止重复处理
  const handleRun = async (step: Step) => {
    if (step.done || busy) return;
    setBusy(step.key);
    try {
      await step.run();
    } finally {
      setBusy(null);
    }
  };

  return (
    <div className="card overflow-hidden">
      <div className="flex items-center justify-between border-b border-slate-100 px-4 py-3 dark:border-slate-800">
        <div>
          <h3 className="font-medium">配置导航</h3>
          <p className="text-xs text-slate-500">
            按步骤完成初始化，已完成的步骤无需重复处理。
          </p>
        </div>
        <Badge tone={allDone ? 'green' : 'amber'}>
          {completed}/{steps.length} 已完成
        </Badge>
      </div>

      <ol className="divide-y divide-slate-100 dark:divide-slate-800">
        {steps.map((step, i) => (
          <li key={step.key} className="flex items-center gap-3 px-4 py-3">
            <div className={step.done ? 'text-emerald-500' : 'text-slate-300 dark:text-slate-600'}>
              {step.done ? <CheckCircle2 size={20} /> : <Circle size={20} />}
            </div>
            <div className="min-w-0 flex-1">
              <div className="text-sm font-medium text-slate-800 dark:text-slate-100">
                {i + 1}. {step.title}
              </div>
              <div className="text-xs text-slate-500">{step.desc}</div>
            </div>

            {step.done ? (
              <span className="shrink-0 rounded-full bg-emerald-50 px-2.5 py-1 text-xs font-medium text-emerald-600 dark:bg-emerald-500/15 dark:text-emerald-400">
                已完成
              </span>
            ) : (
              <button
                onClick={() => void handleRun(step)}
                disabled={busy === step.key}
                className="btn-primary shrink-0"
              >
                {busy === step.key ? '处理中…' : step.actionLabel}
                <ChevronRight size={14} />
              </button>
            )}
          </li>
        ))}
      </ol>

      {allDone && (
        <div className="border-t border-slate-100 bg-emerald-50/60 px-4 py-3 text-sm text-emerald-700 dark:border-slate-800 dark:bg-emerald-500/10 dark:text-emerald-300">
          🎉 全部配置已完成，去「一键签到」开始使用吧！
        </div>
      )}
    </div>
  );
}
