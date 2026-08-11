import { getCurrentWindow } from '@tauri-apps/api/window';
import { Minus, Square, X } from 'lucide-react';

const win = getCurrentWindow();

export default function TitleBar() {
  return (
    <div
      data-tauri-drag-region
      className="flex h-9 shrink-0 items-center justify-between border-b border-slate-200 bg-slate-50 pl-3 pr-1 dark:border-slate-800 dark:bg-slate-900"
    >
      <div data-tauri-drag-region className="flex items-center gap-2">
        <span className="flex h-5 w-5 items-center justify-center rounded-md bg-brand-600 text-[11px] font-bold text-white">
          TW
        </span>
        <span className="text-sm font-semibold text-slate-700 dark:text-slate-200">
          Trae Work 助手
        </span>
      </div>
      <div className="flex items-center">
        <button
          onClick={() => win.minimize()}
          className="flex h-8 w-10 items-center justify-center text-slate-500 hover:bg-slate-200 dark:hover:bg-slate-800"
          aria-label="最小化"
        >
          <Minus size={15} />
        </button>
        <button
          onClick={() => win.toggleMaximize()}
          className="flex h-8 w-10 items-center justify-center text-slate-500 hover:bg-slate-200 dark:hover:bg-slate-800"
          aria-label="最大化"
        >
          <Square size={13} />
        </button>
        <button
          onClick={() => win.close()}
          className="flex h-8 w-10 items-center justify-center text-slate-500 hover:bg-rose-500 hover:text-white"
          aria-label="关闭"
        >
          <X size={15} />
        </button>
      </div>
    </div>
  );
}
