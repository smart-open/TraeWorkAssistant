import { useState } from 'react';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { Minus, Square, X } from 'lucide-react';
import { useAppStore } from '../store';
import { Modal } from './ui';
import { APP_NAME } from '../lib/about';

const win = getCurrentWindow();

/** 品牌图标：渐变圆角方块内的机器人（AI）图形 */
export function BrandMark({ size = 24, iconSize = 14 }: { size?: number; iconSize?: number }) {
  return (
    <span
      className="flex shrink-0 items-center justify-center rounded-lg bg-gradient-to-br from-brand-500 to-brand-700 text-white shadow-sm dark:from-brand-400 dark:to-brand-600"
      style={{ width: size, height: size }}
    >
      <svg
        viewBox="0 0 24 24"
        style={{ width: iconSize, height: iconSize }}
        fill="none"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinecap="round"
        strokeLinejoin="round"
      >
        <path d="M12 8V4H8" />
        <rect width="16" height="12" x="4" y="8" rx="2" />
        <path d="M2 14h2" />
        <path d="M20 14h2" />
        <path d="M15 13v2" />
        <path d="M9 13v2" />
      </svg>
    </span>
  );
}

export default function TitleBar() {
  const [showCloseConfirm, setShowCloseConfirm] = useState(false);
  const proxyRunning = useAppStore((s) => s.proxy.running);

  return (
    <>
      <div
        className="flex h-9 shrink-0 items-center justify-between border-b border-slate-200 bg-slate-50 pl-3 pr-1 dark:border-zinc-800 dark:bg-zinc-950"
      >
        <div data-tauri-drag-region className="flex flex-1 items-center gap-2">
          <BrandMark />
          <span className="text-sm font-semibold text-slate-700 dark:text-zinc-200">
            {APP_NAME}
          </span>
        </div>
        <div className="flex items-center">
          {/* onMouseDown 阻止冒泡：否则事件冒泡到外层 data-tauri-drag-region，Tauri 会启动窗口拖拽而吞掉 click，导致最小/最大化/关闭无响应 */}
          <button
            onMouseDown={(e) => e.stopPropagation()}
            onClick={() => void win.hide()}
            className="flex h-8 w-10 items-center justify-center text-slate-500 transition hover:bg-slate-200/70 active:scale-90 dark:text-zinc-400 dark:hover:bg-zinc-800"
            aria-label="最小化"
            title="最小化到托盘"
          >
            <Minus size={15} />
          </button>
          <button
            onMouseDown={(e) => e.stopPropagation()}
            onClick={() => void win.toggleMaximize()}
            className="flex h-8 w-10 items-center justify-center text-slate-500 transition hover:bg-slate-200/70 active:scale-90 dark:text-zinc-400 dark:hover:bg-zinc-800"
            aria-label="最大化"
          >
            <Square size={13} />
          </button>
          <button
            onMouseDown={(e) => e.stopPropagation()}
            onClick={() => setShowCloseConfirm(true)}
            className="flex h-8 w-10 items-center justify-center text-slate-500 transition hover:bg-rose-500 hover:text-white active:scale-90"
            aria-label="关闭"
          >
            <X size={15} />
          </button>
        </div>
      </div>

      <Modal
        open={showCloseConfirm}
        onClose={() => setShowCloseConfirm(false)}
        title="确认退出应用"
        footer={
          <>
            <button className="btn-outline" onClick={() => setShowCloseConfirm(false)}>
              取消
            </button>
            <button className="btn-danger" onClick={() => void win.close()}>
              确认退出
            </button>
          </>
        }
      >
        {proxyRunning ? (
          <p>
            检测到代理正在运行，退出时将自动关闭代理并还原系统代理设置。
            <br />
            确认退出 {APP_NAME}？
          </p>
        ) : (
          <p>确认退出 {APP_NAME}？</p>
        )}
      </Modal>
    </>
  );
}
