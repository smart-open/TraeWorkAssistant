import {
  ExternalLink,
  Power,
  PowerOff,
  ShieldCheck,
  ShieldAlert,
  MonitorCheck,
  MonitorX,
  Server,
  Wifi,
} from 'lucide-react';
import { useAppStore } from '../store';
import { Badge } from './ui';

export default function TopBar() {
  const env = useAppStore((s) => s.env);
  const envCn = useAppStore((s) => s.envCn);
  const certInstalled = useAppStore((s) => s.certInstalled);
  const proxy = useAppStore((s) => s.proxy);
  const apiStatus = useAppStore((s) => s.apiStatus);
  const startProxy = useAppStore((s) => s.startProxy);
  const stopProxy = useAppStore((s) => s.stopProxy);
  const pushToast = useAppStore((s) => s.pushToast);

  const openTrae = async () => {
    await useAppStore.getState().openTraeWithProxy();
  };
  const openTraeCn = async () => {
    await useAppStore.getState().openTraeCn();
  };

  return (
    <div className="flex h-12 shrink-0 items-center justify-between border-b border-slate-200 bg-white px-4 dark:border-zinc-800 dark:bg-zinc-950">
      <div className="flex items-center gap-2">
        {envCn?.installed ? (
          <Badge tone="green" title={envCn.version ? `Trae 当前版本：v${envCn.version}` : '未检测到 Trae 版本号'}>
            <MonitorCheck size={13} /> Trae 已安装
          </Badge>
        ) : (
          <Badge tone="red">
            <MonitorX size={13} /> Trae 未安装
          </Badge>
        )}
        {env?.installed ? (
          <Badge tone="green" title={env.version ? `Trae Work 当前版本：v${env.version}` : '未检测到 Trae Work 版本号'}>
            <MonitorCheck size={13} /> Trae Work 已安装
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
          <Wifi size={13} /> {proxy.running ? `代理运行中 :${proxy.port}` : '代理未启动'}
        </Badge>
        <Badge tone={apiStatus?.running ? 'green' : 'slate'}>
          <Server size={13} /> {apiStatus?.running ? `API 服务 :${apiStatus.port}` : 'API 服务未启动'}
        </Badge>
      </div>
      <div className="flex items-center gap-2">
        <button onClick={openTraeCn} className="btn-outline" title="打开 Trae CN IDE">
          <ExternalLink size={15} /> 打开 Trae
        </button>
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
