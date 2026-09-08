import { useEffect, useState } from 'react';
import { ExternalLink, Power, PowerOff, ShieldCheck, ShieldAlert, MonitorCheck, MonitorX, Server, Wifi } from 'lucide-react';
import { useAppStore } from '../store';
import { Badge } from './ui';
import { api } from '../lib/tauri';
import type { AppLocate } from '../types';

/** Trae 专区顶栏：双应用安装状态 + 证书/代理/API 服务 + 打开应用 */
function TraeTopBar() {
  const env = useAppStore((s) => s.env);
  const envCn = useAppStore((s) => s.envCn);
  const certInstalled = useAppStore((s) => s.certInstalled);
  const proxy = useAppStore((s) => s.proxy);
  const apiStatus = useAppStore((s) => s.apiStatus);
  const startProxy = useAppStore((s) => s.startProxy);
  const stopProxy = useAppStore((s) => s.stopProxy);

  const openTrae = async () => {
    await useAppStore.getState().openTraeWithProxy();
  };
  const openTraeCn = async () => {
    await useAppStore.getState().openTraeCn();
  };

  return (
    <>
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
    </>
  );
}

/** 豆包专区顶栏：仅豆包安装状态 + 打开豆包（证书/代理/API 服务与豆包无关，不展示） */
function DoubaoTopBar() {
  const pushToast = useAppStore((s) => s.pushToast);
  const [locate, setLocate] = useState<AppLocate | null>(null);

  useEffect(() => {
    let alive = true;
    api.env
      .locate('doubao')
      .then((r) => alive && setLocate(r))
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, []);

  const launch = async () => {
    try {
      await api.doubao.launch();
    } catch (err) {
      pushToast('error', `打开豆包失败：${String(err)}`);
    }
  };

  return (
    <>
      <div className="flex items-center gap-2">
        {locate?.exe ? (
          <Badge tone="green" title={locate.version ? `豆包当前版本：v${locate.version}` : '豆包桌面版'}>
            <MonitorCheck size={13} /> 豆包已安装
          </Badge>
        ) : (
          <Badge tone="red">
            <MonitorX size={13} /> 豆包未检测到
          </Badge>
        )}
      </div>
      <div className="flex items-center gap-2">
        <button onClick={() => void launch()} className="btn-primary" disabled={!locate?.exe}>
          <ExternalLink size={15} /> 打开豆包
        </button>
      </div>
    </>
  );
}

export default function TopBar() {
  const activeApp = useAppStore((s) => s.activeApp);
  return (
    <div className="flex h-12 shrink-0 items-center justify-between border-b border-slate-200 bg-white px-4 dark:border-zinc-800 dark:bg-zinc-950">
      {activeApp === 'doubao' ? <DoubaoTopBar /> : <TraeTopBar />}
    </div>
  );
}
