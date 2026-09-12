import { useEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { Badge } from '../../components/ui';
import { api } from '../../lib/tauri';
import { fmtCredits } from '../../lib/format';
import type { AccountView, CreditDetail } from '../../types';

/** 可用积分单元格：鼠标悬停展示积分明细（仅剩余 > 0 且未过期的积分包，按到期时间升序） */
export function CreditCell({ account }: { account: AccountView }) {
  const [detail, setDetail] = useState<CreditDetail | null>(null);
  const [loading, setLoading] = useState(false);
  const [pos, setPos] = useState<{ top: number; left: number } | null>(null);
  const fetchedRef = useRef(false);
  // 请求序号守卫：过期响应（积分已刷新/组件已卸载）不回填，防止旧数据覆盖新状态
  const reqSeqRef = useRef(0);

  const loadDetail = () => {
    if (fetchedRef.current) return;
    fetchedRef.current = true;
    setLoading(true);
    const seq = ++reqSeqRef.current;
    api.accounts
      .creditDetail(account.user_id)
      .then((d) => {
        if (seq !== reqSeqRef.current) return;
        setDetail(d);
      })
      .catch(() => {
        if (seq !== reqSeqRef.current) return;
        setDetail(null);
        fetchedRef.current = false; // 失败后允许下次悬停重试
      })
      .finally(() => {
        if (seq !== reqSeqRef.current) return;
        setLoading(false);
      });
  };

  // 积分数据刷新后清除过期明细缓存，下次悬停重新拉取；同时作废在途响应
  useEffect(() => {
    reqSeqRef.current++;
    fetchedRef.current = false;
    setDetail(null);
    setLoading(false);
  }, [account.remaining_credits]);

  // 卸载后不再回填
  useEffect(() => () => {
    reqSeqRef.current++;
  }, []);

  const enter = (e: React.MouseEvent<HTMLElement>) => {
    if (account.remaining_credits == null) return;
    const r = e.currentTarget.getBoundingClientRect();
    // 卡片约 300x340，靠屏幕边缘时向内收
    const top = Math.min(r.bottom, window.innerHeight - 360);
    const left = Math.min(r.right - 300, window.innerWidth - 320);
    setPos({ top: Math.max(8, top), left: Math.max(8, left) });
    loadDetail();
  };

  const value = account.remaining_credits;
  return (
    <>
      <div
        className="cursor-help tabular-nums"
        onMouseEnter={enter}
        onMouseLeave={() => setPos(null)}
        title="悬停查看积分明细"
      >
        {value != null ? fmtCredits(value) : '-'}
      </div>
      {pos &&
        createPortal(
          <div
            className="fixed z-50 w-[300px] rounded-lg border border-slate-200 bg-white p-3 text-slate-700 shadow-xl dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
            style={{ top: pos.top, left: pos.left }}
            onMouseEnter={() => setPos(pos)}
            onMouseLeave={() => setPos(null)}
          >
            <div className="mb-2 flex items-center justify-between text-xs">
              <span className="font-semibold">{account.name} 积分明细</span>
              {loading && <span className="text-slate-400">加载中…</span>}
            </div>
            {!loading && detail && detail.packs.length === 0 && (
              <div className="py-2 text-xs text-slate-400">暂无可用积分包</div>
            )}
            {detail && (
              <>
                <div className="mb-2 grid grid-cols-2 gap-1.5 text-xs">
                  <div className="rounded bg-slate-50 px-2 py-1 dark:bg-zinc-900">
                    <span className="text-slate-400">通用积分</span>
                    <div className="font-semibold tabular-nums">{fmtCredits(detail.general)}</div>
                  </div>
                  <div className="rounded bg-slate-50 px-2 py-1 dark:bg-zinc-900">
                    <span className="text-slate-400">Work 积分</span>
                    <div className="font-semibold tabular-nums">{fmtCredits(detail.work)}</div>
                  </div>
                </div>
                {detail.packs.length > 0 && (
                  <div className="max-h-44 space-y-1 overflow-auto">
                    {detail.packs.map((p, i) => {
                      const noExpire = p.expire_time >= 4102444800; // 长期有效哨兵（2100-01-01）
                      const days = Math.max(0, Math.floor((p.expire_time - Date.now() / 1000) / 86400));
                      const hours = Math.max(0, Math.floor(((p.expire_time - Date.now() / 1000) % 86400) / 3600));
                      return (
                        <div key={i} className="flex items-center justify-between gap-2 text-xs">
                          <div className="flex min-w-0 items-center gap-1.5">
                            <Badge tone={p.kind === 'Work' ? 'violet' : 'blue'}>
                              {p.kind === 'Work' ? 'Work' : '通用'}
                            </Badge>
                            <span className="truncate text-slate-500">{p.source}</span>
                          </div>
                          <div className="shrink-0 text-right tabular-nums">
                            <span className="font-medium">{fmtCredits(p.remaining)}</span>
                            <span className="ml-1 text-slate-400">
                              {noExpire ? '长期有效' : days > 0 ? `${days}天后过期` : `${hours}小时后过期`}
                            </span>
                          </div>
                        </div>
                      );
                    })}
                  </div>
                )}
              </>
            )}
            {!loading && !detail && (
              <div className="py-2 text-xs text-slate-400">明细加载失败</div>
            )}
          </div>,
          document.body,
        )}
    </>
  );
}
