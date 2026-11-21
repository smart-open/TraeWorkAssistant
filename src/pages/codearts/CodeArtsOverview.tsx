import { useCallback, useEffect, useState } from 'react';
import { CalendarCheck, KeyRound, RefreshCw, Settings2, Users } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { StatCard } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import type { CodeartsAccountView, CodeartsCreditsResult } from '../../types';

/**
 * codearts-overview 概述：账号数 / 总积分两张统计卡 + 签到入口快捷链接。
 * 无 OAuth / 快照 / 环境重置 / 三通道——仅 AK/SK 手工录入。
 */
export default function CodeArtsOverview() {
  const pushToast = useAppStore((s) => s.pushToast);
  const setView = useAppStore((s) => s.setView);
  const [accounts, setAccounts] = useState<CodeartsAccountView[]>([]);
  const [credits, setCredits] = useState<CodeartsCreditsResult | null>(null);
  const [refreshing, setRefreshing] = useState(false);

  const refresh = useCallback(async () => {
    setRefreshing(true);
    try {
      const [accs, cr] = await Promise.all([
        api.codearts.accountsList().catch(() => [] as CodeartsAccountView[]),
        api.codearts.creditsFetch().then(
          (r) => ({ ok: true as const, r }),
          (err: unknown) => ({ ok: false as const, err }),
        ),
      ]);
      setAccounts(accs);
      if (cr.ok) setCredits(cr.r);
      else pushToast('error', `积分查询失败：${String(cr.err)}`);
    } catch (err) {
      pushToast('error', `CodeArts 概述刷新失败：${String(err)}`);
    } finally {
      setRefreshing(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    void refresh();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const total = accounts.length;
  const withCred = accounts.filter((a) => a.has_credential).length;
  const totalBalance =
    credits && credits.accounts.length > 0
      ? credits.accounts.reduce((s, a) => s + (a.total ?? 0), 0)
      : credits?.total_balance ?? null;

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="CodeArts · 概述"
        desc="华为云码道（国内站）运行总览 · 账号数 / 总积分 · 签到入口"
        actions={
          <button onClick={() => void refresh()} className="btn-outline" disabled={refreshing}>
            <RefreshCw size={15} className={refreshing ? 'animate-spin' : ''} /> 刷新
          </button>
        }
      />

      {/* 顶部统计卡（对齐 Qoder 概述：数值一眼掌握） */}
      <div className="grid grid-cols-2 gap-3 md:grid-cols-3">
        <StatCard label="账号总数" value={total} hint={`已录入凭证 ${withCred}`} tone="brand" />
        <StatCard
          label="总积分"
          value={totalBalance != null ? totalBalance.toFixed(2) : '—'}
          hint={credits ? '来自积分查询缓存' : '导入 AK/SK 后自动查询'}
          tone="amber"
        />
        <StatCard label="签到入口" value="每日签到" hint="支持应用内定时与计划任务" tone="violet" />
      </div>

      {/* 快捷入口 */}
      <div className="mt-5 card p-5">
        <h3 className="font-medium">快捷操作</h3>
        <p className="mt-1 text-xs text-slate-500">按步骤完成初始化后即可每日自动签到。</p>
        <div className="mt-4 grid gap-3 sm:grid-cols-3">
          <button onClick={() => setView('codearts-accounts')} className="btn-outline justify-start !py-3">
            <KeyRound size={16} /> 录入 AK/SK · 账号管理
          </button>
          <button onClick={() => setView('codearts-checkin')} className="btn-outline justify-start !py-3">
            <CalendarCheck size={16} /> 前往每日签到
          </button>
          <button onClick={() => setView('codearts-settings')} className="btn-outline justify-start !py-3">
            <Settings2 size={16} /> 签到时刻与调度
          </button>
        </div>
      </div>

      <div className="mt-5 card p-5">
        <div className="flex items-center gap-2">
          <Users size={16} className="text-slate-400" />
          <span className="text-sm font-medium">使用说明</span>
        </div>
        <p className="mt-2 text-xs leading-relaxed text-slate-400">
          CodeArts（华为云码道，国内站）通过 AK/SK 访问账号能力。请在账号管理页手工录入访问密钥，
          SK 仅在录入时提交一次、加密落库，前端不回显。签到收益为账号级积分，重复执行不会重复领取。
        </p>
      </div>
    </div>
  );
}
