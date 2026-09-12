import { Badge, Modal } from '../../components/ui';
import type { ImportPreview } from '../../types';

/** F-46 导入预览：勾选确认后按索引导入 */
export function ImportPreviewModal({
  preview,
  selected,
  importing,
  onClose,
  onToggle,
  onSelectOnly,
  onConfirm,
}: {
  preview: ImportPreview | null;
  selected: Set<number>;
  importing: boolean;
  onClose: () => void;
  onToggle: (index: number) => void;
  onSelectOnly: (indexes: Set<number>) => void;
  onConfirm: () => void;
}) {
  return (
    <Modal
      open={!!preview}
      onClose={onClose}
      title="导入预览"
      size="lg"
      footer={
        <>
          <button onClick={onClose} className="btn-ghost">
            取消
          </button>
          <button onClick={onConfirm} disabled={importing} className="btn-primary">
            {importing ? '导入中…' : `导入所选（${selected.size}）`}
          </button>
        </>
      }
    >
      {preview && (
        <div className="space-y-3">
          <div className="flex flex-wrap items-center gap-2 text-xs text-slate-500 dark:text-slate-400">
            <span>
              共 {preview.total} 个账号，
              已在账号池 {preview.accounts.filter((a) => a.exists).length} 个
            </span>
            {preview.new_groups.length > 0 && (
              <span className="text-violet-600 dark:text-violet-300">
                将新增分组：{preview.new_groups.map((g) => g.name).join('、')}
              </span>
            )}
            <button
              onClick={() =>
                onSelectOnly(
                  new Set(preview.accounts.filter((a) => !a.exists).map((a) => a.index)),
                )
              }
              className="ml-auto text-sky-600 hover:underline dark:text-sky-400"
            >
              全选未存在
            </button>
          </div>
          <div className="max-h-80 space-y-1.5 overflow-y-auto pr-1">
            {preview.accounts.map((a) => (
              <label
                key={a.index}
                className={`flex cursor-pointer items-center gap-2.5 rounded-lg border px-3 py-2 text-sm transition ${
                  selected.has(a.index)
                    ? 'border-sky-300 bg-sky-50 dark:border-sky-500/40 dark:bg-sky-500/10'
                    : 'border-slate-200 hover:bg-slate-50 dark:border-slate-700 dark:hover:bg-slate-800/60'
                }`}
              >
                <input
                  type="checkbox"
                  checked={selected.has(a.index)}
                  onChange={() => onToggle(a.index)}
                  className="h-4 w-4 accent-sky-500"
                />
                <span className="min-w-0 flex-1 truncate font-medium">{a.name}</span>
                {a.user_id && (
                  <span className="shrink-0 font-mono text-xs text-slate-400">
                    {a.user_id.length > 12 ? `…${a.user_id.slice(-8)}` : a.user_id}
                  </span>
                )}
                {a.group_id && <Badge tone="slate">分组</Badge>}
                {a.exists ? (
                  <Badge tone="amber">已存在</Badge>
                ) : a.has_jwt ? (
                  <Badge tone="green">可导入</Badge>
                ) : (
                  <Badge tone="slate">无 JWT</Badge>
                )}
              </label>
            ))}
          </div>
          <p className="text-xs text-slate-400">
            默认勾选未在账号池中的账号；已存在账号勾选导入也不会重复添加（按 uid+JWT 去重）。
          </p>
        </div>
      )}
    </Modal>
  );
}
