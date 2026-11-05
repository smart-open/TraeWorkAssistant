import { useEffect, useMemo, useState } from 'react';
import { Coins } from 'lucide-react';
import { Badge, EmptyState } from '../../components/ui';
import ExpiryCalendar, { type ExpiryItem } from '../../components/ExpiryCalendar';
import { PoolStatusBadge } from '../accounts/PoolStatusBadge';
import { usePoolStatus } from '../../hooks/usePoolStatus';
import { fmtCredits, dateStrToEndTs } from '../../lib/format';
import { api } from '../../lib/tauri';
import type { AccountView, CreditPackDetail, QoderCreditsResult, TraeExpiredEntsResult, WbCreditsResult } from '../../types';
import type { PlatformScope } from './KpiRow';

/**
 * 积分到期 Tab（credits-dashboard-plan.md §6）：
 * ① 账号积分明细：Trae（store.accounts）+ Buddy（workbuddy_credits_fetch.accounts[]）+ Qoder（qoder_credits_fetch.accounts[]）合并表；
 * ② 积分到期日历：Trae 按积分包明细逐包展示（creditDetail 实时拉取，对齐 Buddy/Qoder 包级口径；
 *    issue #82 T5.1 并入 expired_ents 实时逐包条目补齐已过期包）
 *    + Buddy packages[] + Qoder 积分包/Plan 订阅重置，按平台维度联动过滤。
 *    Trae 包明细拉取失败时回退账号级汇总展示（对齐上游「老缓存回退」语义）。
 */

/** 长期有效哨兵时间戳（2100-01-01，与后端 fetch_credit_detail 口径一致） */
const PERPETUAL_TS = 4102444800;

/** Trae 账号积分包明细状态：数组 = 已拉取；failed = 拉取失败（回退账号级口径） */
type TraePacks = Map<string, CreditPackDetail[] | 'failed'>;

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
  /** issue #178：运行期限流/冷却徽标联表键——各池取数响应里的账号标识 */
  uid?: string;
}

export default function ExpiryTab({
  scope,
  accounts,
  wbCredits,
  qoderCredits,
  expiredEnts,
}: {
  scope: PlatformScope;
  /** Trae 账号列表（store） */
  accounts: AccountView[];
  /** Buddy 积分查询结果 */
  wbCredits: WbCreditsResult | null;
  /** Qoder 积分查询结果（积分包 + Plan 订阅周期） */
  qoderCredits: QoderCreditsResult | null;
  /** Trae 逐包到期实时查询（issue #82 T5.1：expired_ents，含已过期包；credit_packs 只含未过期可用包） */
  expiredEnts: TraeExpiredEntsResult | null;
}) {
  // Trae 积分包明细：creditDetail 为实时上游请求（对齐账号页 CreditCell 悬停口径），
  // Tab 挂载时按账号列表并发拉取一次；失败回退账号级汇总。依赖用 user_id 串
  // （store.accounts 引用随任意刷新变化，不宜作依赖）
  const [traePacks, setTraePacks] = useState<TraePacks>(new Map());
  const traeUserIds = useMemo(
    () => (scope === 'trae' ? accounts.map((a) => a.user_id).join(',') : ''),
    [scope, accounts],
  );
  useEffect(() => {
    if (scope !== 'trae' || !traeUserIds) return;
    let cancelled = false;
    const ids = traeUserIds.split(',');
    setTraePacks(new Map());
    (async () => {
      // 4 并发小池逐账号实时上游请求（fetch_credit_detail）：全量并发在账号多时
      // 存在上游风控突发暴露；失败记 'failed' 回退账号级口径。
      // 注：React StrictMode dev 双跑会重复发首轮请求（仅 dev，生产单跑）——
      // cancelled 守卫丢弃过期回填，无状态污染
      const QUEUE = 4;
      const results: [string, CreditPackDetail[] | 'failed'][] = [];
      let cursor = 0;
      const worker = async () => {
        while (cursor < ids.length) {
          const id = ids[cursor++];
          try {
            results.push([id, (await api.accounts.creditDetail(id)).packs]);
          } catch {
            results.push([id, 'failed']);
          }
        }
      };
      await Promise.all(Array.from({ length: Math.min(QUEUE, ids.length) }, worker));
      if (!cancelled) setTraePacks(new Map<string, CreditPackDetail[] | 'failed'>(results));
    })();
    return () => {
      cancelled = true;
    };
  }, [scope, traeUserIds]);

  // 运行期限流/冷却徽标（issue #178）：三池内存池状态，3s 轮询联表注入
  const traePool = usePoolStatus('trae');
  const buddyPool = usePoolStatus('wb');
  const qoderPool = usePoolStatus('qoder');

  const rows = useMemo<AccountRow[]>(() => {
    const nowSec = Date.now() / 1000;
    const out: AccountRow[] = [];
    if (scope === 'trae') {
      for (const a of accounts) {
        // 已拉到包明细 → 包级口径：包数真实计数、最近到期取包级最早（排除长期有效
        // 哨兵 2100-01-01）；拉取中（Map 无键）→ 行状态提示「明细拉取中…」；
        // 拉取失败（'failed'）→ 回退账号级汇总口径
        const packs = traePacks.get(a.user_id);
        const hasPacks = Array.isArray(packs);
        const activePkgs = hasPacks ? packs.filter((p) => p.remaining > 0) : [];
        const pkgRealExpires = activePkgs
          .filter((p) => p.expire_time > 0 && p.expire_time < PERPETUAL_TS)
          .map((p) => p.expire_time);
        const expires = hasPacks
          ? [...pkgRealExpires, ...(a.membership_expire != null ? [a.membership_expire] : [])].filter((t) => t > 0)
          : [a.credits_expire_at, a.membership_expire].filter(
              (t): t is number => t != null && t > 0,
            );
        const cooling = a.cooldown_until != null && a.cooldown_until > nowSec;
        out.push({
          key: `trae-${a.user_id}`,
          platform: 'Trae',
          name: a.name,
          balance: a.remaining_credits,
          packages: hasPacks
            ? activePkgs.length + (a.membership_expire != null ? 1 : 0)
            : (a.credits_expire_at != null ? 1 : 0) + (a.membership_expire != null ? 1 : 0),
          nearestExpire: expires.length > 0 ? Math.min(...expires) : null,
          ok: !cooling,
          status:
            packs === undefined
              ? '明细拉取中…'
              : cooling
                ? '冷却中'
                : '正常',
          uid: a.user_id,
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
          // issue #179：ok=true 但包明细空时后端携带诚实标注 message（如 local_quota
          // 降级/上游形状变化），状态列不再恒显「正常」掩盖降级
          status: a.ok ? a.message || '正常' : a.message || '查询失败',
          source: a.source,
          uid: a.user_id,
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
          // 长期有效哨兵过滤（issue #141，port main aa4ec5a）：上游对长期有效包
          // 返回 9999-12-31 大哨兵，Math.min 会把「最近到期」显示成 9999/12/31——
          // 与到期日历 PERPETUAL_TS 同口径剔除；全部包为长期有效时
          // nearestExpire=null 显示「—」（日历仍以「长期有效」展示）
          if (endTs != null && endTs < PERPETUAL_TS) expires.push(endTs);
        }
        // 最近到期含 Plan 订阅周期（到期即重置；仅剩余额度 > 0 计入，与到期日历一致）
        const planEnd = dateStrToEndTs(a.plan_expires_at);
        // 包数与到期日历/KPI 口径对齐：明细缺 plan 包（逐包接口回退聚合口径）时，
        // 日历会补一条「Plan 订阅重置」条目 → 此处同步 +1，避免日历 2 条/明细行显示 1。
        // hasPlanPkg 仅计「计入 activePkgs 的 plan 包」——日历对 plan 包无 amount 过滤
        // （amount<=0 仍生成订阅重置条目），此处若按任意 plan 包判定会与日历差 1
        const hasPlanPkg = (a.packages ?? []).some(
          (p) => p.source === 'plan' && (p.amount == null || p.amount > 0),
        );
        const pkgCount =
          activePkgs.length + (!hasPlanPkg && planEnd != null && (a.plan_credits ?? 0) > 0 ? 1 : 0);
        if (planEnd != null && planEnd < PERPETUAL_TS && (a.plan_credits ?? 0) > 0) {
          expires.push(planEnd);
        }
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
          uid: a.user_id,
        });
      }
    }
    return out.sort((x, y) => (y.balance ?? -1) - (x.balance ?? -1));
  }, [scope, accounts, wbCredits, qoderCredits, traePacks]);

  // 到期日历 items（§6.2）：Trae 积分包明细逐包（对齐 Buddy/Qoder 包级口径）+ 会员；
  // Buddy remaining > 0 的积分包；Qoder 积分包 + Plan 订阅重置
  const expiryItems = useMemo<ExpiryItem[]>(() => {
    const items: ExpiryItem[] = [];
    if (scope === 'trae') {
      for (const a of accounts) {
        // 已拉到包明细 → 包级逐包条目（对齐上游 34e1757：label=平台·账号·包来源、
        // note=「kind · 剩余 X / 总 Y」；长期有效哨兵由 ExpiryCalendar 识别展示）；
        // 拉取中/失败 → 回退账号级汇总条目
        const packs = traePacks.get(a.user_id);
        if (Array.isArray(packs)) {
          packs.forEach((p, i) => {
            // 剩余积分为 0 的包（已用完）无到期提醒价值，过滤不展示（与 Buddy 同款防御）
            if (p.remaining <= 0) return;
            items.push({
              key: `trae-${a.user_id}-pack-${i}`,
              label: `Trae · ${a.name} · ${p.source}`,
              kind: '积分包',
              expire_ts: p.expire_time,
              note: `${p.kind} · 剩余 ${p.remaining.toFixed(2)}${p.total != null ? ` / 总 ${p.total.toFixed(2)}` : ''}`,
            });
          });
        } else if (a.credits_expire_at != null) {
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
        // expired_ents 逐包精确到期（issue #82 T5.1）：包明细只含「剩余>0 且
        // 未过期」的包，本接口补齐已过期包（未刷新过积分的账号也由此获得逐包条目）。
        // 与 creditDetail 已展示的包按「到期秒」去重——creditDetail.source 是归一化标签
        // 而 expired_ents.name 是上游原始包名，两接口名字口径不同，按名字匹配必然落空
        const packKeys = new Set(
          (Array.isArray(packs) ? packs : []).filter((p) => p.remaining > 0).map((p) => p.expire_time),
        );
        const expiredList =
          expiredEnts?.accounts.find((x) => x.user_id === a.user_id)?.expired ?? [];
        expiredList.forEach((e, i) => {
          const expireSec = Math.floor(e.expire_time_ms / 1000);
          if (expireSec <= 0 || packKeys.has(expireSec)) return;
          items.push({
            key: `trae-${a.user_id}-expired-${i}-${expireSec}`,
            label: `Trae · ${a.name} · ${e.name || '积分包'}`,
            kind: '积分包',
            expire_ts: expireSec,
            note: `额度 ${fmtCredits(e.credits_limit)}（官网逐包到期）`,
          });
        });
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
            note: `剩余 ${pkg.remaining.toFixed(2)} / 总 ${(pkg.total ?? 0).toFixed(2)}`,
          });
        }
      }
    }
    if (scope === 'qoder') {
      for (const acc of qoderCredits?.accounts ?? []) {
        for (const [i, p] of (acc.packages ?? []).entries()) {
          const isPlan = p.source === 'plan';
          const isAddon = p.source === 'addon';
          // dedicated 仅 sash 聚合路径产生（带 name）；bonus 等其余 source 均为
          // 个人资源包（此前 `!isPlan && !isAddon` 把所有 bonus 误标「专属资源包」）
          const isDedicated = p.source === 'dedicated';
          if (p.amount != null && p.amount <= 0 && !isPlan) continue;
          const isReset = isPlan || isAddon;
          const note =
            p.amount == null
              ? '剩余未知'
              : p.total == null
                ? `剩余 ${fmtCredits(p.amount)}`
                : `剩余 ${fmtCredits(p.amount)} / 总 ${fmtCredits(p.total)}`;
          items.push({
            // 索引键：同账号同 source 同到期日的无 name 包（bonus 常态）内容键会碰撞
            key: `qoder-${acc.user_id}-pack-${i}`,
            label: `Qoder · ${acc.name} · ${isPlan ? 'Plan 订阅配额' : isAddon ? 'Add-on 包' : isDedicated ? p.name || '专属资源包' : '个人资源包'}`,
            kind: isReset ? '订阅重置' : '积分包',
            expire_ts: dateStrToEndTs(p.expire_at),
            // Add-on 包随订阅周期重置（对齐上游），同 Plan 一样标注
            note: `${note}${isReset ? '（随订阅周期重置）' : ''}`,
          });
        }
        // 逐包明细缺 plan 包（回退聚合口径）但 plan 周期存在时，补一条 Plan 订阅重置条目
        const hasPlanPkg = (acc.packages ?? []).some((p) => p.source === 'plan');
        const planEnd = dateStrToEndTs(acc.plan_expires_at);
        if (!hasPlanPkg && planEnd != null && (acc.plan_credits ?? 0) > 0) {
          items.push({
            key: `qoder-${acc.user_id}-plan-reset`,
            label: `Qoder · ${acc.name} · Plan 订阅重置`,
            kind: '订阅重置',
            expire_ts: planEnd,
            note: `Plan 额度 ${fmtCredits(acc.plan_credits ?? 0)}（订阅周期到期重置）`,
          });
        }
      }
    }
    return items;
  }, [scope, accounts, wbCredits, qoderCredits, traePacks, expiredEnts]);

  // 到期明细查询失败/跳过的账号（issue #82 T5.1：单账号失败与无 JWT 跳过原先完全静默，
  // 审查 P5——在日历上方集中提示，不影响账号表状态列语义）
  const expiredEntsIssues = useMemo(() => {
    if (scope !== 'trae' || !expiredEnts) return [];
    return expiredEnts.accounts
      .filter((x) => !x.ok)
      .map((x) => ({ name: x.name || x.user_id, reason: x.error || '查询失败' }));
  }, [scope, expiredEnts]);

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
                    <Badge tone={r.platform === 'Trae' ? 'blue' : r.platform === 'Buddy' ? 'violet' : 'amber'}>{r.platform}</Badge>
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
                    <div className="flex flex-wrap items-center gap-1">
                      {r.ok ? (
                        <Badge tone="green">{r.status}</Badge>
                      ) : (
                        <Badge tone="red" title={r.status}>
                          异常
                        </Badge>
                      )}
                      {/* 运行期限流/冷却（issue #178）：按平台联表对应池 */}
                      {r.platform === 'Trae' && <PoolStatusBadge status={traePool.get(r.uid ?? '')} />}
                      {r.platform === 'Buddy' && <PoolStatusBadge status={buddyPool.get(r.uid ?? '')} />}
                      {r.platform === 'Qoder' && <PoolStatusBadge status={qoderPool.get(r.uid ?? '')} />}
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {/* 到期明细不完整提示（审查 P5）：单账号失败/无 JWT 跳过不再静默 */}
      {expiredEntsIssues.length > 0 && (
        <div className="card border-amber-300 bg-amber-50 px-4 py-2.5 text-xs text-amber-700 dark:border-amber-700/40 dark:bg-amber-900/20 dark:text-amber-300">
          到期明细未查询完整：{expiredEntsIssues.map((x) => `${x.name}（${x.reason}）`).join('；')}
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
