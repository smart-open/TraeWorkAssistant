import { useEffect, useState } from 'react';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { invoke } from '@tauri-apps/api/core';
import { Copy, Minus, Square, X } from 'lucide-react';
import { useAppStore } from '../store';
import { Modal } from './ui';
import { APP_NAME } from '../lib/about';

const win = getCurrentWindow();

/** macOS 红绿灯按钮：红=关闭 黄=最小化到托盘 绿=最大化/还原，hover 时浮现符号 */
function TrafficLights(props: {
  maximized: boolean;
  onClose: () => void;
  onMinimize: () => void;
  onToggleMaximize: () => void;
}) {
  // onMouseDown 阻止冒泡：Windows 侧三按钮嵌在 data-tauri-drag-region 容器内
  //（红绿灯虽为容器兄弟节点，仍统一 stopPropagation 防拖拽吞 click 并保持两套按钮同约定）
  // tooltip 用 left-0 而非居中：红灯贴近窗口左缘，居中会被裁掉
  const base =
    'group relative flex h-3 w-3 items-center justify-center rounded-full border transition active:brightness-90 [&>svg]:opacity-0 hover:[&>svg]:opacity-100';
  const tip =
    'pointer-events-none absolute left-0 top-full z-50 mt-1.5 whitespace-nowrap rounded-md bg-zinc-900 px-2 py-1 text-xs leading-4 text-zinc-100 opacity-0 shadow-md transition-opacity delay-300 duration-150 group-hover:opacity-100 dark:bg-zinc-800';
  return (
    <div className="flex items-center gap-2">
      <button
        onMouseDown={(e) => e.stopPropagation()}
        onClick={props.onClose}
        className={`${base} border-[#d9483f] bg-[#ff5f57]`}
        aria-label="关闭"
      >
        <svg
          viewBox="0 0 12 12"
          className="h-2 w-2 text-black/60"
          fill="none"
          stroke="currentColor"
          strokeWidth="1.6"
          strokeLinecap="round"
        >
          <path d="M3.8 3.8l4.4 4.4M8.2 3.8l-4.4 4.4" />
        </svg>
        <span className={tip}>关闭</span>
      </button>
      <button
        onMouseDown={(e) => e.stopPropagation()}
        onClick={props.onMinimize}
        className={`${base} border-[#d9a021] bg-[#febc2e]`}
        aria-label="最小化"
      >
        <svg
          viewBox="0 0 12 12"
          className="h-2 w-2 text-black/60"
          fill="none"
          stroke="currentColor"
          strokeWidth="1.6"
          strokeLinecap="round"
        >
          <path d="M3.2 6h5.6" />
        </svg>
        <span className={tip}>最小化到托盘</span>
      </button>
      <button
        onMouseDown={(e) => e.stopPropagation()}
        onClick={props.onToggleMaximize}
        className={`${base} border-[#1ba32f] bg-[#28c840]`}
        aria-label={props.maximized ? '还原' : '最大化'}
      >
        <svg viewBox="0 0 12 12" className="h-2 w-2 text-black/60" fill="currentColor">
          <path d="M3 3h3.6L3 6.6zM9 9H5.4L9 5.4z" />
        </svg>
        <span className={tip}>{props.maximized ? '向下还原' : '最大化'}</span>
      </button>
    </div>
  );
}

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
  const [maximized, setMaximized] = useState(false);
  const proxyRunning = useAppStore((s) => s.proxy.running);
  const isMac = useAppStore((s) => s.platform) === 'macos';

  // issue #46：跟踪窗口真实最大化状态——toggleMaximize 在无边框窗口 hide/show 后
  // 可能与内部状态失同步（视觉已还原但 isMaximized() 仍为 true，点最大化无反应），
  // 改为显式判断 + 图标随状态切换
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    void (async () => {
      setMaximized(await win.isMaximized());
      unlisten = await win.onResized(async () => {
        setMaximized(await win.isMaximized());
      });
    })();
    return () => unlisten?.();
  }, []);

  const toggleMaximize = async () => {
    if (await win.isMaximized()) {
      await win.unmaximize();
    } else {
      await win.maximize();
    }
  };

  return (
    <>
      <div
        className="flex h-9 shrink-0 items-center justify-between border-b border-slate-200 bg-slate-50 pl-3 pr-1 dark:border-zinc-800 dark:bg-zinc-950"
      >
        {isMac && (
          <TrafficLights
            maximized={maximized}
            onClose={() => setShowCloseConfirm(true)}
            onMinimize={() => void invoke('minimize_to_tray')}
            onToggleMaximize={() => void toggleMaximize()}
          />
        )}
        <div
          data-tauri-drag-region
          className={`flex flex-1 items-center gap-2 ${isMac ? 'pl-3' : ''}`}
        >
          <BrandMark />
          <span className="text-sm font-semibold text-slate-700 dark:text-zinc-200">
            {APP_NAME}
          </span>
        </div>
        {!isMac && (
          <div className="flex items-center">
          {/* onMouseDown 阻止冒泡：否则事件冒泡到外层 data-tauri-drag-region，Tauri 会启动窗口拖拽而吞掉 click，导致最小/最大化/关闭无响应 */}
          <button
            onMouseDown={(e) => e.stopPropagation()}
            onClick={() => void invoke('minimize_to_tray')}
            className="flex h-8 w-10 items-center justify-center text-slate-500 transition hover:bg-slate-200/70 active:scale-90 dark:text-zinc-400 dark:hover:bg-zinc-800"
            aria-label="最小化"
            title="最小化到托盘"
          >
            <Minus size={15} />
          </button>
          <button
            onMouseDown={(e) => e.stopPropagation()}
            onClick={() => void toggleMaximize()}
            className="flex h-8 w-10 items-center justify-center text-slate-500 transition hover:bg-slate-200/70 active:scale-90 dark:text-zinc-400 dark:hover:bg-zinc-800"
            aria-label={maximized ? '还原' : '最大化'}
            title={maximized ? '向下还原' : '最大化'}
          >
            {maximized ? <Copy size={13} /> : <Square size={13} />}
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
        )}
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
