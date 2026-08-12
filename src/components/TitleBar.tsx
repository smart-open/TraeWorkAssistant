import { getCurrentWindow } from '@tauri-apps/api/window';
import { Minus, Square, X } from 'lucide-react';
import { useAppStore } from '../store';

const win = getCurrentWindow();

export default function TitleBar() {
  // 托盘启用时，最小化应隐藏窗口到托盘（而非最小化到任务栏），否则无入口可恢复
  const trayEnabled = useAppStore((s) => s.settings?.tray ?? false);
  return (
    <div
      className="flex h-9 shrink-0 items-center justify-between border-b border-slate-200 bg-slate-50 pl-3 pr-1 dark:border-slate-800 dark:bg-slate-900"
    >
      <div data-tauri-drag-region className="flex flex-1 items-center gap-2">
        <span className="flex h-5 w-5 items-center justify-center rounded-md bg-gradient-to-br from-brand-500 to-brand-700 text-[11px] font-bold text-white shadow-sm">
          TW
        </span>
        <span className="text-sm font-semibold text-slate-700 dark:text-slate-200">
          Trae Work 助手
        </span>
      </div>
      <div className="flex items-center">
        {/* onMouseDown 阻止冒泡：否则事件冒泡到外层 data-tauri-drag-region，Tauri 会启动窗口拖拽而吞掉 click，导致最小/最大化/关闭无响应 */}
        <button
          onMouseDown={(e) => e.stopPropagation()}
          onClick={() =>
            trayEnabled ? void win.hide() : void win.minimize()
          }
          className="flex h-8 w-10 items-center justify-center text-slate-500 transition hover:bg-slate-200/70 active:scale-90 dark:hover:bg-slate-800"
          aria-label="最小化"
        >
          <Minus size={15} />
        </button>
        <button
          onMouseDown={(e) => e.stopPropagation()}
          onClick={() => void win.toggleMaximize()}
          className="flex h-8 w-10 items-center justify-center text-slate-500 transition hover:bg-slate-200/70 active:scale-90 dark:hover:bg-slate-800"
          aria-label="最大化"
        >
          <Square size={13} />
        </button>
        <button
          onMouseDown={(e) => e.stopPropagation()}
          onClick={() => void win.close()}
          className="flex h-8 w-10 items-center justify-center text-slate-500 transition hover:bg-rose-500 hover:text-white active:scale-90"
          aria-label="关闭"
        >
          <X size={15} />
        </button>
      </div>
    </div>
  );
}
