import { useMemo } from 'react';
import { Coins } from 'lucide-react';
import { Badge, EmptyState } from '../../components/ui';
import ExpiryCalendar, { type ExpiryItem } from '../../components/ExpiryCalendar';
import { fmtCredits } from '../../lib/format';
import type { AccountView, WbCreditsResult } from '../../types';
import type { PlatformScope } from './KpiRow';

/**
 * 积分到期 Tab（credits-dashboard-plan.md §6）：
 * ① 账号积分明细：Trae（store.accounts）+ Buddy（workbuddy_credits_fetch.accounts[]）合并表；
 * ② 积分到期日历：Trae expiryItems（积分包 + 会员）+ Buddy packages[]，按平台维度联动过滤。
 */

interface AccountRow {
  key: string;
  platform: 'Trae' | 'Buddy';
  name: string;
  balance: number | null;
  packages: number;
  nearestExpire: number | null;
  ok: boolean;
  status: string;
  /** Buddy 取数来源标记（cloud/legacy/local_quota/none/fetch_failed；Trae 行无此字段） */
  source?: string;
}

export default function ExpiryTab({
  scope,
  accounts,
  wbCredits,
}: {
  scope: PlatformScope;
  /** Trae 账号列表（store） */
  accounts: AccountView[];
  /** Buddy 积分查询结果 */
  wbCredits: WbCreditsResult | null;
}) {
  const rows = useMemo<AccountRow[]>(() => {
    const nowSec = Date.now() / 1000;
    const out: AccountRow[] = [];
    if (scope !== 'buddy') {
      for (const a of accounts) {
        const expires = [a.credits_expire_at, a.membership_expire].filter(
          (t): t is number => t != null && t > 0,
        );
        out.push({
          key: `trae-${a.user_id}`,
          platform: 'Trae',
          name: a.name,
          balance: a.remaining_credits,
          packages:
            (a.credits_expire_at != null ? 1 : 0) + (a.membership_expire != null ? 1 : 0),
          nearestExpire: expires.length > 0 ? Math.min(...expires) : null,
          ok: !(a.cooldown_until != null && a.cooldown_until > nowSec),
          status:
            a.cooldown_until != null && a.cooldown_until > nowSec ? '冷却中' : '正常',
        });
      }
    }
    if (scope !== 'trae') {
      for (const a of wbCredits?.accounts ?? []) {
        // 积分包数与 KPI「积分包总数」口径对齐：只计剩余 > 0 的包（已用完不计，审查遗留修复）
        const activePkgs = (a.packages ?? []).filter((p) => p.remaining > 0);
        const expires = activePkgs
          .filter((p) => p.expire_ts != null && p.expire_ts > 0)
          .map((p) => p.expire_ts as number);
        out.push({
          key: `buddy-${a.user_id}`,
          platform: 'Buddy',
          name: a.name,
          balance: a.balance,
          packages: activePkgs.length,
          nearestExpire: expires.length > 0 ? Math.min(...expires) : null,
          ok: a.ok,
          status: a.ok ? '正常' : a.message || '查询失败',
          source: a.source,
        });
      }
    }
    return out.sort((x, y) => (y.balance ?? -1) - (x.balance ?? -1));
  }, [scope, accounts, wbCredits]);

  // 到期日历 items（§6.2）：Trae 积分包 + 会员；Buddy remaining > 0 的积分包
  const expiryItems = useMemo<ExpiryItem[]>(() => {
    const items: ExpiryItem[] = [];
    if (scope !== 'buddy') {
      for (const a of accounts) {
        if (a.credits_expire_at != null) {
          items.push({
            key: `trae-${a.user_id}-credits`,
            label: `Trae · ${a.name}`,
            kind: '积分包',
            expire_ts: a.credits_expire_at,
            note: `剩余 ${fmtCredits(a.remaining_credits ?? 0)}${a.total_credits != null ? ` / 总 ${fmtCredits(a.total_credits)}` : ''} 积分`,
          });
        }
        if (a.membership_expire != null) {
          items.push({
            key: `trae-${a.user_id}-membership`,
            label: `Trae · ${a.name}`,
            kind: '会员',
            expire_ts: a.membership_expire,
            note: a.pay_identity ? `套餐 ${a.pay_identity}` : null,
          });
        }
      }
    }
    if (scope !== 'trae') {
      for (const acc of wbCredits?.accounts ?? []) {
        for (const pkg of acc.packages ?? []) {
          // 剩余积分为 0 的包（已用完）无到期提醒价值，过滤不展示
          if (pkg.remaining <= 0) continue;
          items.push({
            key: `buddy-${acc.user_id}-${pkg.name}-${pkg.expire_ts ?? 0}`,
            label: `Buddy · ${acc.name} · ${pkg.name}`,
            kind: '积分包',
            expire_ts: pkg.expire_ts,
            note: `剩余 ${pkg.remaining.toFixed(2)} / ${(pkg.total ?? 0).toFixed(2)}`,
          });
        }
      }
    }
    return items;
  }, [scope, accounts, wbCredits]);

  const fmtExpire = (ts: number | null) => {
    if (!ts) return '—';
    const d = new Date(ts * 1000);
    return `${d.getFullYear()}/${String(d.getMonth() + 1).padStart(2, '0')}/${String(d.getDate()).padStart(2, '0')}`;
  };

  return (
    <div className="space-y-5">
      {/* ① 账号积分明细（双平台合并，按可用积分降序） */}
      {rows.length === 0 ? (
        <EmptyState
          icon={<Coins size={22} />}
          title="暂无积分数据"
          hint="请先在「账号管理」导入账号（Trae 需 JWT / Buddy 需已录入凭证）；查询失败时请检查登录态。"
        />
      ) : (
        <div className="card overflow-hidden">
          <div className="flex items-center justify-between border-b border-slate-100 px-5 py-3 dark:border-zinc-800">
            <h3 className="font-medium">账号积分明细</h3>
            <span className="text-xs text-slate-400">按可用积分降序 · {rows.length} 个账号</span>
          </div>
          <table className="w-full text-sm">
            <thead className="bg-slate-50 text-xs uppercase text-slate-500 dark:bg-zinc-900">
              <tr>
                <th className="px-4 py-2 text-left">平台</th>
                <th className="px-4 py-2 text-left">账号</th>
                <th className="px-4 py-2 text-right">可用积分</th>
                <th className="px-4 py-2 text-right" title="Trae = 积分包 + 会员包计数；Buddy = 剩余积分 > 0 的包数（与 KPI 口径一致）">
                  积分包
                </th>
                <th className="px-4 py-2 text-right">最近到期</th>
                <th className="px-4 py-2 text-left">状态</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((r) => (
                <tr key={r.key} className="row-hover border-t border-slate-200 dark:border-zinc-800">
                  <td className="px-4 py-2">
                    <Badge tone={r.platform === 'Trae' ? 'blue' : 'violet'}>{r.platform}</Badge>
                  </td>
                  <td className="px-4 py-2">
                    <div className="flex items-center gap-1.5">
                      <span className="font-medium">{r.name}</span>
                      {r.source === 'legacy' && (
                        <Badge
                          tone="slate"
                          title="新版计费三接口（summary/paid/free）本次未返回数据，已自动改用旧版聚合接口（v2 get-user-resource）兜底取数：余额 = 积分包剩余求和，数据可信。偶发多为网络抖动或接口短暂异常；若该账号持续出现，建议在日志页核查计费接口返回码。"
                        >
                          旧接口回退
                        </Badge>
                      )}
                      {r.source === 'local_quota' && (
                        <Badge
                          tone="slate"
                          title="云端计费接口全部失败，已探测本机桌面服务端口兜底取得余额（无积分包明细）。"
                        >
                          本地兜底
                        </Badge>
                      )}
                    </div>
                  </td>
                  <td className="px-4 py-2 text-right tabular-nums">
                    {r.balance != null ? fmtCredits(r.balance) : '—'}
                  </td>
                  <td className="px-4 py-2 text-right text-xs text-slate-400">{r.packages} 个</td>
                  <td className="px-4 py-2 text-right text-xs tabular-nums text-slate-500">
                    {fmtExpire(r.nearestExpire)}
                  </td>
                  <td className="px-4 py-2">
                    {r.ok ? (
                      <Badge tone="green">{r.status}</Badge>
                    ) : (
                      <Badge tone="red" title={r.status}>
                        异常
                      </Badge>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {/* ② 积分到期日历（复用 ExpiryCalendar，双平台 items 拼装） */}
      <div className="card p-4">
        <h3 className="mb-3 font-medium">积分到期日历</h3>
        <ExpiryCalendar
          items={expiryItems}
          emptyHint="暂无到期项：待账号完成签到/积分查询后展示积分包与会员到期时间。"
        />
      </div>
    </div>
  );
}
