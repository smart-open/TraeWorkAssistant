import { CheckCircle2, Loader2, Plus, RefreshCw } from 'lucide-react';
import { Badge, Modal } from '../../components/ui';
import type { DiscoveredAccount } from '../../types';

/** F-08 自动发现弹框：展示本机 Trae Work / Trae 登录账号，一键加入账号池 */
export function DiscoverModal({
  open,
  scanning,
  discovered,
  addingUid,
  onClose,
  onAdd,
  onRescan,
}: {
  open: boolean;
  scanning: boolean;
  discovered: DiscoveredAccount[] | null;
  addingUid: string | null;
  onClose: () => void;
  onAdd: (d: DiscoveredAccount) => void;
  onRescan: () => void;
}) {
  const apps = ['TraeWork', 'Trae'] as const;
  const notInPool = discovered?.filter((d) => !d.in_pool).length ?? 0;
  return (
    <Modal open={open} onClose={onClose} title="扫描本机登录账号" size="lg">
      <div className="space-y-3 text-sm">
        <p className="text-xs leading-relaxed text-slate-500 dark:text-zinc-400">
          扫描本机 Trae Work（TRAE SOLO CN）与 Trae 的 <code className="rounded bg-slate-100 px-1 dark:bg-zinc-800">storage.json</code> 与
          {' '}<code className="rounded bg-slate-100 px-1 dark:bg-zinc-800">state.vscdb</code>，
          识别当前已登录账号（Cloud-IDE uid，与账号池同体系）。未入池的账号可一键加入
          （先以占位形式入库，之后启动代理打开对应应用时，JWT 会被自动捕获回填）。
        </p>

        {scanning && (
          <div className="flex items-center gap-2 py-4 text-sm text-slate-500">
            <Loader2 size={15} className="animate-spin" /> 正在扫描本机应用…
          </div>
        )}

        {!scanning && discovered && discovered.length === 0 && (
          <div className="py-4 text-center text-sm text-slate-400">
            未发现已登录账号（两个应用均未登录或未安装）
          </div>
        )}

        {!scanning && discovered && discovered.length > 0 && (
          <>
            {apps.map((app) => {
              const list = discovered.filter((d) => d.app === app);
              if (list.length === 0) return null;
              return (
                <div key={app} className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
                  <div className="mb-2 flex items-center justify-between">
                    <span className="font-semibold">{list[0].app_label}</span>
                    <span className="text-xs text-slate-400">{list.length} 个登录账号</span>
                  </div>
                  <div className="space-y-1.5">
                    {list.map((d) => (
                      <div
                        key={`${d.app}-${d.user_id}`}
                        className="flex items-center justify-between gap-2 rounded bg-slate-50 px-2 py-1.5 dark:bg-zinc-900"
                      >
                        <div className="min-w-0">
                          <div className="truncate font-mono text-xs">{d.user_id}</div>
                          {d.uid_confident ? (
                            d.dc_uid ? (
                              <div className="truncate text-[10px] text-slate-400 dark:text-zinc-500">
                                账户中心 uid：{d.dc_uid}（与账号池 id 体系不同，仅作参考）
                              </div>
                            ) : null
                          ) : (
                            <div className="text-[10px] text-amber-600 dark:text-amber-400">
                              无法确认账号池 uid（仅识别到账户中心 id），暂不能入池
                            </div>
                          )}
                        </div>
                        {d.in_pool ? (
                          <Badge tone="green">
                            <CheckCircle2 size={12} /> 已入池
                          </Badge>
                        ) : d.uid_confident ? (
                          <button
                            onClick={() => onAdd(d)}
                            disabled={addingUid === d.user_id}
                            className="btn-outline !px-2 !py-1 text-xs disabled:cursor-not-allowed disabled:opacity-60"
                          >
                            {addingUid === d.user_id ? (
                              <>
                                <Loader2 size={12} className="animate-spin" /> 加入中
                              </>
                            ) : (
                              <>
                                <Plus size={12} /> 加入
                              </>
                            )}
                          </button>
                        ) : (
                          <span className="text-[10px] text-slate-400">不可入池</span>
                        )}
                      </div>
                    ))}
                  </div>
                </div>
              );
            })}
            <div className="flex items-center justify-between pt-1">
              <span className="text-xs text-slate-400">
                {notInPool > 0 ? `${notInPool} 个账号未入池` : '所有登录账号均已入池'}
              </span>
              <button onClick={onRescan} className="btn-outline text-xs">
                <RefreshCw size={12} /> 重新扫描
              </button>
            </div>
          </>
        )}
      </div>
    </Modal>
  );
}
