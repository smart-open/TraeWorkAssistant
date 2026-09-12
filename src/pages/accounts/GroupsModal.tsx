import { useEffect, useState } from 'react';
import { Plus, Trash2 } from 'lucide-react';
import { Modal } from '../../components/ui';
import type { GroupView } from '../../types';

const PRESET_COLORS = [
  '#6366f1', '#22c55e', '#f59e0b', '#ef4444', '#0ea5e9', '#a855f7', '#14b8a6',
];

export function GroupsModal({
  open,
  onClose,
  groups,
  onCreate,
  onRename,
  onRecolor,
  onDelete,
}: {
  open: boolean;
  onClose: () => void;
  groups: GroupView[];
  onCreate: (name: string, color: string) => Promise<void>;
  onRename: (id: string, name: string) => Promise<void>;
  onRecolor: (id: string, color: string) => Promise<void>;
  onDelete: (id: string) => Promise<void>;
}) {
  const [name, setName] = useState('');
  const [color, setColor] = useState(PRESET_COLORS[0]);
  const [editingNames, setEditingNames] = useState<Record<string, string>>({});
  // 删除分组确认弹框（禁 window.confirm，红线）
  const [deleteGroupTarget, setDeleteGroupTarget] = useState<GroupView | null>(null);

  useEffect(() => {
    setEditingNames((prev) => {
      const next = { ...prev };
      groups.forEach((g) => {
        if (!(g.id in next)) next[g.id] = g.name;
      });
      return next;
    });
  }, [groups]);

  return (
    <>
      <Modal
      open={open}
      onClose={onClose}
      title="分组管理"
      footer={
        <>
          <button onClick={onClose} className="btn-ghost">关闭</button>
          <button
            onClick={async () => {
              if (!name.trim()) return;
              await onCreate(name.trim(), color);
              setName('');
            }}
            className="btn-primary"
          >
            <Plus size={14} /> 新建
          </button>
        </>
      }
    >
      <div className="mb-4 space-y-3">
        <div>
          <label className="label">新分组名称</label>
          <input value={name} onChange={(e) => setName(e.target.value)} className="input" placeholder="例如：工作 / 私人" />
        </div>
        <div>
          <label className="label">颜色</label>
          <div className="flex flex-wrap gap-2">
            {PRESET_COLORS.map((c) => (
              <button
                key={c}
                type="button"
                onClick={() => setColor(c)}
                className={`h-6 w-6 rounded-full border-2 ${color === c ? 'border-slate-900 dark:border-white' : 'border-transparent'}`}
                style={{ background: c }}
                aria-label={c}
              />
            ))}
          </div>
        </div>
      </div>
      <div className="max-h-64 space-y-2 overflow-auto">
        {groups.length === 0 && <div className="text-xs text-slate-400">暂无分组</div>}
        {groups.map((g) => (
          <div key={g.id} className="flex items-center gap-2 rounded-lg border border-slate-200 p-2 dark:border-zinc-700">
            <span className="inline-block h-4 w-4 rounded-full" style={{ background: g.color }} />
            <input
              value={editingNames[g.id] ?? g.name}
              onChange={(e) =>
                setEditingNames((prev) => ({ ...prev, [g.id]: e.target.value }))
              }
              onBlur={(e) => {
                const val = e.target.value.trim();
                if (val && val !== g.name) {
                  void onRename(g.id, val).then(() => {
                    setEditingNames((prev) => {
                      const next = { ...prev };
                      delete next[g.id];
                      return next;
                    });
                  });
                }
              }}
              className="input !py-1 flex-1 !text-xs"
            />
            <select
              value={g.color}
              onChange={(e) => void onRecolor(g.id, e.target.value)}
              className="input !py-1 !text-xs w-24"
            >
              {PRESET_COLORS.map((c) => (
                <option key={c} value={c}>{c}</option>
              ))}
            </select>
            <span className="text-xs text-slate-400">{g.count}</span>
            <button
              onClick={() => setDeleteGroupTarget(g)}
              className="btn-ghost !p-2 text-rose-500"
              aria-label="删除"
            >
              <Trash2 size={14} />
            </button>
          </div>
        ))}
      </div>
    </Modal>
      {/* 删除分组确认弹框（禁 window.confirm，红线） */}
      <Modal
        open={deleteGroupTarget != null}
        onClose={() => setDeleteGroupTarget(null)}
        title="删除分组"
        footer={
          <>
            <button className="btn-outline" onClick={() => setDeleteGroupTarget(null)}>取消</button>
            <button
              className="btn-primary !bg-rose-600 hover:!bg-rose-500"
              onClick={async () => {
                const target = deleteGroupTarget;
                setDeleteGroupTarget(null);
                if (target) await onDelete(target.id);
              }}
            >
              确认删除
            </button>
          </>
        }
      >
        <div className="text-sm">
          删除分组「{deleteGroupTarget?.name}」？该分组下账号会回落为「未分组」。
        </div>
      </Modal>
    </>
  );
}
