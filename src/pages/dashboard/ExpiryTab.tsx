import { useMemo } from 'react';
import { Coins } from 'lucide-react';
import { Badge, EmptyState } from '../../components/ui';
import ExpiryCalendar, { type ExpiryItem } from '../../components/ExpiryCalendar';
import { fmtCredits, dateStrToEndTs } from '../../lib/format';
import type { AccountView, QoderCreditsResult, WbCreditsResult } from '../../types';
import type { PlatformScope } from './KpiRow';

/**
 * 积分到期 Tab（credits-dashboard-plan.md §6）：
 * ① 账号积分明细：Trae（store.accounts）+ Buddy（workbuddy_credits_fetch.accounts[]）+ Qoder（qoder_credits_fetch.accounts[]）合并表；
 * ② 积分到期日历：Trae 按积分包明细展示（credit_packs，对齐 Buddy packages[] 包级口径，
 *    老缓存无包明细时回退账号级汇总）+ Buddy packages[] + Qoder 积分包/Plan 订阅重置，按平台维度联动过滤。
 */

/** 长期有效哨兵时间戳（2100-01-01，与后端 pack_to_detail 口径一致） */
const PERPETUAL_TS = 4102444800;

interface AccountRow {
  key: string;
  platform: 'Trae' | 'Buddy' | 'Qoder';
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
  qoderCredits,
}: {
  scope: PlatformScope;
  /** Trae 账号列表（store） */
  accounts: AccountView[];
  /** Buddy 积分查询结果 */
  wbCredits: WbCreditsResult | null;
  /** Qoder 积分查询结果（积分包 + Plan 订阅周期） */
  qoderCredits: QoderCreditsResult | null;
}) {
  const rows = useMemo<AccountRow[]>(() => {
    const nowSec = Date.now() / 1000;
    const out: AccountRow[] = [];
    if (scope === 'trae') {
      for (const a of accounts) {
        // 有包明细（刷新过积分）→ 包级口径：包数真实计数、最近到期取包级最早（排除长期有效哨兵）；
        // 无明细（老缓存）→ 回退账号级汇总口径（原实现）
        const hasPacks = a.credit_packs != null;
        // 与 Buddy activePkgs 同口径：只计剩余 > 0 的包（后端已过滤，前端同款防御）
        const activePkgs = (a.credit_packs ?? []).filter((p) => p.remaining > 0);
        const pkgRealExpires = activePkgs
          .filter((p) => p.expire_time > 0 && p.expire_time < PERPETUAL_TS)
          .map((p) => p.expire_time);
        const expires: number[] = hasPacks
          ? [...pkgRealExpires, a.membership_expire ?? 0].filter((t) => t > 0)
          : [a.credits_expire_at, a.membership_expire].filter((t): t is number => t != null && t > 0);
        out.push({
          key: `trae-${a.user_id}`,
          platform: 'Trae',
          name: a.name,
          balance: a.remaining_credits,
          packages: hasPacks
            ? activePkgs.length + (a.membership_expire != null ? 1 : 0)
            : (a.credits_expire_at != null ? 1 : 0) + (a.membership_expire != null ? 1 : 0),
          nearestExpire: expires.length > 0 ? Math.min(...expires) : null,
          ok: !(a.cooldown_until != null && a.cooldown_until > nowSec),
          status:
            a.cooldown_until != null && a.cooldown_until > nowSec ? '冷却中' : '正常',
        });
      }
    }
    if (scope === 'buddy') {
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
    if (scope === 'qoder') {
      for (const a of qoderCredits?.accounts ?? []) {
        // 包计数与 KPI 口径对齐：剩余未知或 > 0 计入（已用完不计）
        const activePkgs = (a.packages ?? []).filter((p) => p.amount == null || p.amount > 0);
        const expires: number[] = [];
        for (const p of activePkgs) {
          const endTs = dateStrToEndTs(p.expire_at);
          if (endTs != null) expires.push(endTs);
        }
        // 最近到期含 Plan 订阅周期（到期即重置；仅剩余额度 > 0 计入，与到期日历一致）
        const planEnd = dateStrToEndTs(a.plan_expires_at);
        // 包数与到期日历/KPI 口径对齐：明细缺 plan 包（逐包接口回退聚合口径）时，
        // 日历会补一条「Plan 订阅重置」条目 → 此处同步 +1，避免日历 2 条/明细行显示 1
        const hasPlanPkg = (a.packages ?? []).some((p) => p.source === 'plan');
        const pkgCount =
          activePkgs.length + (!hasPlanPkg && planEnd != null && (a.plan_credits ?? 0) > 0 ? 1 : 0);
        if (planEnd != null && (a.plan_credits ?? 0) > 0) expires.push(planEnd);
        out.push({
          key: `qoder-${a.user_id}`,
          platform: 'Qoder',
          name: a.name,
          balance: a.total,
          packages: pkgCount,
          nearestExpire: expires.length > 0 ? Math.min(...expires) : null,
          ok: a.ok,
          status: a.ok ? '正常' : a.message || '查询失败',
          source: a.source,
        });
      }
    }
    return out.sort((x, y) => (y.balance ?? -1) - (x.balance ?? -1));
  }, [scope, accounts, wbCredits, qoderCredits]);

  // 到期日历 items（§6.2）：Trae 按积分包明细展示（对齐 Buddy 包级口径）+ 会员，
  // 老缓存无 credit_packs 时回退账号级汇总（积分包 1 条 + 会员 1 条）；
  // Buddy remaining > 0 的积分包；Qoder 积分包 + Plan 订阅重置
  const expiryItems = useMemo<ExpiryItem[]>(() => {
    const items: ExpiryItem[] = [];
    if (scope === 'trae') {
      for (const a of accounts) {
        if (a.credit_packs != null) {
          // 包级明细：每个可用包一条（对齐 Buddy 展示结构：label=平台·账号·包名、kind=积分包、
          // note=「剩余 X / 总 Y」两位小数；通用/Work 维度并入 note 前缀保留信息）
          (a.credit_packs ?? []).forEach((p, i) => {
            // 剩余积分为 0 的包（已用完）无到期提醒价值，过滤不展示（与 Buddy 同款防御）
            if (p.remaining <= 0) return;
            items.push({
              key: `trae-${a.user_id}-pack-${i}`,
              label: `Trae · ${a.name} · ${p.source}`,
              kind: '积分包',
              expire_ts: p.expire_time,
              note: `${p.kind} · 剩余 ${p.remaining.toFixed(2)} / ${(p.total ?? 0).toFixed(2)}`,
            });
          });
        } else if (a.credits_expire_at != null) {
          // 老缓存回退：账号级汇总展示
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
    if (scope === 'buddy') {
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
    if (scope === 'qoder') {
      for (const acc of qoderCredits?.accounts ?? []) {
        // 积分包明细（R-11 逐包口径，展示结构对齐 Buddy 包级样式）：
        // plan = 订阅配额（随订阅周期重置）、bonus = 个人资源包、
        // dedicated = 专属/组织资源包（sash usage 逐包，自有到期时间）、
        // addon = 旧聚合口径（addOnQuota 总额，随订阅周期展示）
        (acc.packages ?? []).forEach((p, i) => {
          if (p.amount != null && p.amount <= 0) return;
          const isPlan = p.source === 'plan';
          const isAddon = p.source === 'addon';
          const isDedicated = p.source === 'dedicated';
          const pkgName = isPlan
            ? 'Plan 订阅配额'
            : isAddon
              ? 'Add-on 包'
              : isDedicated
                ? p.name || '专属资源包'
                : '个人资源包';
          const resetNote = isPlan || isAddon ? '（随订阅周期重置）' : '';
          items.push({
            key: `qoder-${acc.user_id}-pack-${i}`,
            label: `Qoder · ${acc.name} · ${pkgName}`,
            kind: isPlan || isAddon ? '订阅重置' : '积分包',
            expire_ts: dateStrToEndTs(p.expire_at),
            note:
              p.amount == null
                ? '剩余未知'
                : p.total == null
                  ? `剩余 ${fmtCredits(p.amount)}${resetNote}`
                  : `剩余 ${fmtCredits(p.amount)} / 总 ${fmtCredits(p.total)}${resetNote}`,
          });
        });
        // Plan 订阅重置独立条目：包明细已含 plan 包（R-11 逐包口径）时不重复展示，
        // 仅老缓存/明细接口失败回退聚合口径时补
        const hasPlanPkg = (acc.packages ?? []).some((p) => p.source === 'plan');
        const planEnd = dateStrToEndTs(acc.plan_expires_at);
        if (!hasPlanPkg && planEnd != null && (acc.plan_credits ?? 0) > 0) {
          items.push({
            key: `qoder-${acc.user_id}-plan`,
            label: `Qoder · ${acc.name}`,
            kind: '订阅重置',
            expire_ts: planEnd,
            note: `Plan 额度 ${fmtCredits(acc.plan_credits ?? 0)}（订阅周期到期重置）`,
          });
        }
      }
    }
    return items;
  }, [scope, accounts, wbCredits, qoderCredits]);

  const fmtExpire = (ts: number | null) => {
    if (!ts) return '—';
    const d = new Date(ts * 1000);
    return `${d.getFullYear()}/${String(d.getMonth() + 1).padStart(2, '0')}/${String(d.getDate()).padStart(2, '0')}`;
  };

  return (
    <div className="space-y-5">
      {/* ① 账号积分明细（三平台合并，按可用积分降序） */}
      {rows.length === 0 ? (
        <EmptyState
          icon={<Coins size={22} />}
          title="暂无积分数据"
          hint="请先在「账号管理」导入账号（Trae 需 JWT / Buddy、Qoder 需已录入凭证）；查询失败时请检查登录态。"
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
                <th className="px-4 py-2 text-right" title="Trae = 积分包 + 会员包计数；Buddy/Qoder = 剩余 > 0 的包数（与 KPI 口径一致）">
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
                    <Badge tone={r.platform === 'Trae' ? 'blue' : r.platform === 'Buddy' ? 'violet' : 'amber'}>
                      {r.platform}
                    </Badge>
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

      {/* ② 积分到期日历（复用 ExpiryCalendar，三平台 items 拼装） */}
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
