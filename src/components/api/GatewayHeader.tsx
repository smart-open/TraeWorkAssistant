/**
 * 全局 API 管理 · 网关头部（unified-api-gateway-design §5.2/§5.4）
 * 启停按钮 + 常驻指标行（运行状态 / 总请求数 / 当前并发数 / API Key 数量）。
 * 数据源：api_server_status（含 inflight）、api_keys_list；3s 轮询刷新。
 */
import { useCallback, useEffect, useState } from 'react';
import { Play, Square } from 'lucide-react';
import { Badge } from '../ui';
import { api } from '../../lib/tauri';
import { withMinDelay } from '../../lib/delay';
import { useAppStore } from '../../store';
import GatewayHelpModal, { GatewayHelpButton } from './GatewayHelpModal';
import type { ApiServiceStatus } from '../../types';

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
  const [status, setStatus] = useState<ApiServiceStatus | null>(null);
  const [keyCount, setKeyCount] = useState(0);
  const [starting, setStarting] = useState(false);
  const [stopping, setStopping] = useState(false);
  const [helpOpen, setHelpOpen] = useState(false);

  const refresh = useCallback(async () => {
    try {
      setStatus(await api.apiServer.status());
    } catch {
      /* 保留上次状态 */
    }
    try {
      const view = await api.apiServer.keysList();
      setKeyCount(view.keys.length);
    } catch {
      /* 保留上次数量 */
    }
  }, []);

  useEffect(() => {
    void refresh();
    const id = setInterval(() => void refresh(), 3000);
    return () => clearInterval(id);
  }, [refresh]);

  const start = async () => {
    setStarting(true);
    try {
      const s = await withMinDelay(api.apiServer.start());
      setStatus(s);
      useAppStore.setState({ apiStatus: s });
      toast('success', `API 服务已启动（端口 ${s.port}）`);
    } catch (err) {
      toast('error', `启动失败：${String(err)}`);
    } finally {
      setStarting(false);
    }
  };

  const stop = async () => {
    setStopping(true);
    try {
      await withMinDelay(api.apiServer.stop());
      setStatus(null);
      useAppStore.setState({ apiStatus: null });
      toast('info', 'API 服务已停止');
    } catch (err) {
      toast('error', `停止失败：${String(err)}`);
    } finally {
      setStopping(false);
    }
  };

  const running = status?.running ?? false;

  return (
    <div className="card p-4">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="min-w-0">
          <p className="text-sm font-medium text-slate-800 dark:text-zinc-100">
            OpenAI / Anthropic 兼容接口，通过 Trae / Buddy 资源池智能调度实现多账号负载均衡
          </p>
          <p className="mt-0.5 text-xs text-slate-400 dark:text-zinc-500">
            统一网关 127.0.0.1:{status?.port ?? 7864} · 请求按模型 ID 匹配资源池（未运行时端口为配置值）
          </p>
        </div>
        <div className="flex shrink-0 items-center gap-1">
          {running ? (
            <button
              className="btn-danger flex items-center gap-1.5 !py-1.5 text-xs"
              onClick={() => void stop()}
              disabled={stopping}
            >
              <Square size={14} />
              {stopping ? '停止中…' : '停止网关'}
            </button>
          ) : (
            <button
              className="btn-outline flex items-center gap-1.5 !py-1.5 text-xs"
              onClick={() => void start()}
              disabled={starting}
            >
              <Play size={14} />
              {starting ? '启动中…' : '启动网关'}
            </button>
          )}
          <GatewayHelpButton onClick={() => setHelpOpen(true)} />
        </div>
      </div>

      <div className="mt-3 grid grid-cols-2 gap-2 sm:grid-cols-4">
        <div className="rounded-lg border border-slate-200 px-3 py-2 dark:border-zinc-700">
          <div className="text-[11px] text-slate-400 dark:text-zinc-500">运行状态</div>
          <div className="mt-1">
            <Badge tone={running ? 'green' : 'slate'}>
              {running ? `运行中 :${status?.port ?? 0}` : '已停止'}
            </Badge>
          </div>
        </div>
        <Metric label="总请求数" value={status?.total_requests ?? 0} hint="累计处理的 API 调用" />
        <Metric
          label="当前并发数"
          value={running ? (status?.inflight ?? 0) : '—'}
          hint="正在处理中的请求数（inflight，§4.5）"
        />
        <Metric label="API Key 数量" value={keyCount} hint="data/api_keys.json 全部条目" />
      </div>

      {status?.last_error && (
        <p className="mt-2 break-all text-xs text-rose-600 dark:text-rose-400">
          最近错误：{status.last_error}
        </p>
      )}

      <GatewayHelpModal open={helpOpen} onClose={() => setHelpOpen(false)} />
    </div>
  );
}
