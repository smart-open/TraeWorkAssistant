/**
 * 全局 API 管理 · 网关头部（unified-api-gateway-design §5.2/§5.4）
 * Web 版网关常驻运行：无启停按钮，仅展示「网关运行中 :端口」徽标 + 指标行（API Key 数量）。
 * 数据源：gateway_settings_get（端口）、api_keys_list。
 */
import { useEffect, useState } from 'react';
import { Badge } from '../ui';
import { api } from '../../lib/tauri';
import { gatewayBaseUrl } from '../../lib/gateway';
import { useAppStore } from '../../store';
import GatewayHelpModal, { GatewayHelpButton } from './GatewayHelpModal';

/** 紧凑指标单元（弹窗头部不做大号 StatCard） */
function Metric({ label, value, hint }: { label: string; value: string | number; hint?: string }) {
  return (
    <div className="rounded-lg border border-slate-200 px-3 py-2 dark:border-zinc-700" title={hint}>
      <div className="text-[11px] text-slate-400 dark:text-zinc-500">{label}</div>
      <div className="mt-0.5 text-lg font-semibold tabular-nums text-slate-800 dark:text-zinc-100">
        {value}
      </div>
    </div>
  );
}

export default function GatewayHeader() {
  const toast = useAppStore((s) => s.pushToast);
  const [port, setPort] = useState(7864);
  const [keyCount, setKeyCount] = useState(0);
  const [helpOpen, setHelpOpen] = useState(false);

  useEffect(() => {
    try {
      void api.apiServer.gatewaySettingsGet().then((s) => setPort(s.port));
    } catch {
      /* 保留默认端口展示 */
    }
    api.apiServer
      .keysList()
      .then((view) => setKeyCount(view.keys.length))
      .catch((err) => toast('error', `读取 API Key 列表失败：${String(err)}`));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return (
    <div className="card p-4">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="min-w-0">
          <p className="text-sm font-medium text-slate-800 dark:text-zinc-100">
            OpenAI / Anthropic 兼容接口，通过 Trae / Buddy 资源池智能调度实现多账号负载均衡
          </p>
          <p className="mt-0.5 text-xs text-slate-400 dark:text-zinc-500">
            统一网关 {gatewayBaseUrl(port).replace(/^https?:\/\//, '')} · 请求按模型 ID 匹配资源池
          </p>
        </div>
        <div className="flex shrink-0 items-center gap-1">
          <Badge tone="green">网关运行中 :{port}</Badge>
          <GatewayHelpButton onClick={() => setHelpOpen(true)} />
        </div>
      </div>

      <div className="mt-3 grid grid-cols-1 gap-2 sm:grid-cols-2">
        <Metric label="API Key 数量" value={keyCount} hint="data/api_keys.json 全部条目" />
        <Metric
          label="运行状态"
          value="常驻运行"
          hint="Web 版网关随服务端常驻运行，无需手动启停"
        />
      </div>

      <GatewayHelpModal open={helpOpen} onClose={() => setHelpOpen(false)} />
    </div>
  );
}
