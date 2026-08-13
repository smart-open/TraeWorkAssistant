import {
  ExternalLink,
  Power,
  PowerOff,
  ShieldCheck,
  ShieldAlert,
  MonitorCheck,
  MonitorX,
} from 'lucide-react';
import { useAppStore } from '../store';
import { Badge } from './ui';

export default function TopBar() {
  const env = useAppStore((s) => s.env);
  const certInstalled = useAppStore((s) => s.certInstalled);
  const proxy = useAppStore((s) => s.proxy);
  const startProxy = useAppStore((s) => s.startProxy);
  const stopProxy = useAppStore((s) => s.stopProxy);
  const pushToast = useAppStore((s) => s.pushToast);

  const openTrae = async () => {
    await useAppStore.getState().openTraeWithProxy();
  };

  return (
    <div className="flex h-12 shrink-0 items-center justify-between border-b border-slate-200 bg-white px-4 dark:border-slate-800 dark:bg-slate-900">
      <div className="flex items-center gap-2">
        {env?.installed ? (
          <Badge tone="green">
            <MonitorCheck size={13} /> Trae Work 已安装
            {env.version ? ` v${env.version}` : ''}
          </Badge>
        ) : (
          <Badge tone="red">
            <MonitorX size={13} /> Trae Work 未安装
          </Badge>
        )}
        {certInstalled ? (
          <Badge tone="green">
            <ShieldCheck size={13} /> 证书已信任
          </Badge>
        ) : (
          <Badge tone="amber">
            <ShieldAlert size={13} /> 证书未安装
          </Badge>
        )}
        <Badge tone={proxy.running ? 'blue' : 'slate'}>
          {proxy.running ? `代理运行中 :${proxy.port}` : '代理未启动'}
        </Badge>
      </div>
      <div className="flex items-center gap-2">
        <button onClick={openTrae} className="btn-outline">
          <ExternalLink size={15} /> {env?.installed ? '打开 Trae Work' : '下载 Trae Work'}
        </button>
        {proxy.running ? (
          <button onClick={stopProxy} className="btn-outline">
            <PowerOff size={15} /> 停止代理
          </button>
        ) : (
          <button onClick={startProxy} className="btn-primary">
            <Power size={15} /> 启动代理
          </button>
        )}
      </div>
    </div>
  );
}
