import { useEffect, useRef, useState } from 'react';

/** 多选下拉（chips 触发 + 面板勾选）：看板/积分面板顶部分组与账号筛选控件。
 *  从 dashboard/Dashboard.tsx 抽出共享（豆包额度看板复用同一交互样式）。 */
export function MultiSelectDropdown({
  label,
  options,
  selected,
  onChange,
  onOpen,
  width = 'w-56',
}: {
  label: string;
  options: { id: string; name: string; color?: string }[];
  selected: string[];
  onChange: (ids: string[]) => void;
  /** 打开面板时回调（分组下拉重拉成员数据用） */
  onOpen?: () => void;
  width?: string;
}) {
  const [open, setOpen] = useState(false);
  const [search, setSearch] = useState('');
  const ref = useRef<HTMLDivElement | null>(null);
  const btnRef = useRef<HTMLButtonElement | null>(null);
  useEffect(() => {
    if (!open) return;
    const onDoc = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    // Esc 关闭面板并把焦点还给触发按钮（键盘可达性：不开新监听就不订阅，cleanup 同步移除）
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        setOpen(false);
        btnRef.current?.focus();
      }
    };
    document.addEventListener('mousedown', onDoc);
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('mousedown', onDoc);
      document.removeEventListener('keydown', onKey);
    };
  }, [open]);
  const toggle = (id: string) =>
    onChange(selected.includes(id) ? selected.filter((x) => x !== id) : [...selected, id]);
  // 全量搜索（文档需求②：账号多选可直接全量搜账号）；打开时重置
  const kw = search.trim().toLowerCase();
  const visible = kw ? options.filter((o) => o.name.toLowerCase().includes(kw) || o.id.toLowerCase().includes(kw)) : options;
  return (
    <div ref={ref} className="relative">
      <button
        ref={btnRef}
        type="button"
        className={`btn-outline !py-1.5 !text-xs ${selected.length ? '!border-sky-400 text-sky-600 dark:text-sky-400' : ''}`}
        onClick={() => {
          setOpen((v) => !v);
          setSearch('');
          if (!open) onOpen?.();
        }}
      >
        {label}
        {selected.length > 0 && (
          <span className="ml-1 rounded-full bg-sky-500 px-1.5 text-[10px] font-medium text-white">
            {selected.length}
          </span>
        )}
      </button>
      {open && (
        <div className={`absolute right-0 z-30 mt-1 max-h-72 ${width} overflow-auto rounded-lg border border-slate-200 bg-white p-2 shadow-lg dark:border-zinc-700 dark:bg-zinc-900`}>
          {options.length > 5 && (
            <input
              autoFocus
              value={search}
              onChange={(e) => setSearch(e.target.value)}
              placeholder={`搜索${label}…`}
              className="input mb-1 !py-1 !text-xs"
            />
          )}
          {visible.length === 0 && (
            <div className="px-2 py-1 text-xs text-slate-400">
              {options.length === 0 ? '暂无可选项' : '无匹配项'}
            </div>
          )}
          {visible.map((o) => (
            <label
              key={o.id}
              className="flex cursor-pointer items-center gap-2 rounded px-2 py-1 text-xs hover:bg-slate-50 dark:hover:bg-zinc-800"
            >
              <input
                type="checkbox"
                checked={selected.includes(o.id)}
                onChange={() => toggle(o.id)}
                className="h-3.5 w-3.5"
              />
              {o.color && (
                <span className="inline-block h-2.5 w-2.5 shrink-0 rounded-full" style={{ background: o.color }} />
              )}
              <span className="truncate">{o.name}</span>
            </label>
          ))}
          {selected.length > 0 && (
            <button
              type="button"
              onClick={() => onChange([])}
              className="mt-1 w-full rounded px-2 py-1 text-left text-xs text-slate-400 hover:bg-slate-50 dark:hover:bg-zinc-800"
            >
              清空筛选（全部）
            </button>
          )}
        </div>
      )}
    </div>
  );
}
