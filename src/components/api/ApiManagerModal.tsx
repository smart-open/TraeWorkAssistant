/**
 * 全局 API 管理弹窗（unified-api-gateway-design §5.2，Phase 2）
 * Modal（max-w-[76.8rem]，较原 max-w-5xl 整体放大 1/5）+ Tab 分区【概览｜接口配置｜API Keys 管理｜用量统计｜自定义模型】，
 * 默认概览；头部常驻 GatewayHeader（网关常驻徽标 + 指标行），Tab 切换不消失；任意 activeApp 视图均可打开。
 * 概览 Tab 为资源总览 + 调度策略中心。
 * 子弹框（子 Key 配置/删除确认）沿用 Modal 组件叠加，z 序高于主弹窗。
 */
import { useEffect, useRef, useState } from 'react';
import { Modal } from '../ui';
import { useAppStore } from '../../store';
import { cn } from '../../lib/cn';
import GatewayHeader from './GatewayHeader';
import InterfaceConfig from './InterfaceConfig';
import ApiKeysManager from './ApiKeysManager';
import CustomModelsPanel from './CustomModelsPanel';
import ResourceSummary from './ResourceSummary';
import UsageStatsPanel from './UsageStatsPanel';

type TabKey = 'overview' | 'config' | 'keys' | 'usage' | 'custom';

const TABS: { key: TabKey; label: string }[] = [
  { key: 'overview', label: '概览' },
  { key: 'config', label: '接口配置' },
  { key: 'keys', label: 'API Keys 管理' },
  { key: 'usage', label: '用量统计' },
  { key: 'custom', label: '自定义模型' },
];

export default function ApiManagerModal() {
  const open = useAppStore((s) => s.showApiManager);
  const setOpen = useAppStore((s) => s.setShowApiManager);
  const [tab, setTab] = useState<TabKey>('overview');
  // 子弹框打开期间屏蔽主弹窗 ESC 关闭：Modal 的 keydown 监听按挂载序触发，
  // 主弹窗 onClose 先执行——此 ref 为 true 时不关主弹窗，仅由子弹框自身 onClose 关闭
  const subOpenRef = useRef(false);

  useEffect(() => {
    if (open) {
      setTab('overview');
      subOpenRef.current = false;
    }
  }, [open]);

  const requestClose = () => {
    if (!subOpenRef.current) setOpen(false);
  };

  return (
    <Modal open={open} onClose={requestClose} title="API 管理" widthClass="max-w-[76.8rem]">
      {/* 头部常驻：网关常驻徽标 + 指标行（Tab 切换不消失，§5.2） */}
      <GatewayHeader />

      {/* Tab 栏（突出显示：active 加底色 + 加重字重；按钮等高避免切换跳动） */}
      <div className="mt-4 flex items-center gap-1 border-b border-slate-200 dark:border-zinc-700">
        {TABS.map((t) => (
          <button
            key={t.key}
            onClick={() => setTab(t.key)}
            className={cn(
              '-mb-px rounded-t-md border-b-2 px-4 py-2 text-sm font-medium transition',
              tab === t.key
                ? 'border-brand-500 bg-brand-50/80 font-semibold text-brand-600 dark:bg-brand-500/10 dark:text-brand-400'
                : 'border-transparent text-slate-500 hover:bg-slate-50 hover:text-slate-700 dark:text-zinc-400 dark:hover:bg-zinc-800/60 dark:hover:text-zinc-200',
            )}
          >
            {t.label}
          </button>
        ))}
      </div>

      {/* Tab 内容（固定高度内部滚动，头部与 Tab 栏常驻；切换 Tab 高度不变，接口配置等长内容走内部滚动。
          高度推导：内容区占 100vh-560px（用户校准值，较初版收紧 200px），
          加上头部/Tab 栏/标题后整窗 ≈ 100vh-450px；矮屏由内部滚动兜底） */}
      <div className="mt-3 h-[calc(100vh_-_560px)] min-h-[320px] overflow-y-auto pr-0.5">
        {tab === 'overview' && <ResourceSummary />}
        {tab === 'config' && <InterfaceConfig />}
        {tab === 'keys' && (
          <ApiKeysManager onSubModalChange={(v) => { subOpenRef.current = v; }} />
        )}
        {tab === 'usage' && <UsageStatsPanel />}
        {tab === 'custom' && (
          <CustomModelsPanel onSubModalChange={(v) => { subOpenRef.current = v; }} />
        )}
      </div>
    </Modal>
  );
}
