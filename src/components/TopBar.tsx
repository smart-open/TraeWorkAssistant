import { useEffect, useState } from 'react';
import { ExternalLink, Power, PowerOff, ShieldCheck, ShieldAlert, MonitorCheck, MonitorX, Server, Wifi, KeyRound, FileCheck } from 'lucide-react';
import { open } from '@tauri-apps/plugin-shell';
import { useAppStore } from '../store';
import { Badge } from './ui';
import { api } from '../lib/tauri';
import type { AppLocate, WorkBuddyEnvCheck, WorkBuddyAccountView } from '../types';

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

/** 豆包专区顶栏：豆包安装状态 + 证书信任/代理状态（会员额度抓包用）+ 打开豆包/启动代理 */
function DoubaoTopBar() {
  const pushToast = useAppStore((s) => s.pushToast);
  const certInstalled = useAppStore((s) => s.certInstalled);
  const proxy = useAppStore((s) => s.proxy);
  const startProxy = useAppStore((s) => s.startProxy);
  const stopProxy = useAppStore((s) => s.stopProxy);
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
      // 代理运行中时注入 --proxy-server：客户端流量必走本地代理，凭证/额度抓取不依赖系统代理
      await api.doubao.launch(proxy.running ? proxy.port : undefined);
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
        {certInstalled ? (
          <Badge tone="green" title="系统已信任抓包代理 CA 证书（会员额度接口抓包需要）">
            <ShieldCheck size={13} /> 证书已信任
          </Badge>
        ) : (
          <Badge tone="amber" title="未安装/未信任抓包代理 CA 证书，会员额度接口抓包前需先安装">
            <ShieldAlert size={13} /> 证书未信任
          </Badge>
        )}
        <Badge tone={proxy.running ? 'blue' : 'slate'} title={proxy.running ? '抓包代理运行中' : '抓包代理未启动'}>
          <Wifi size={13} /> {proxy.running ? `代理运行中 :${proxy.port}` : '代理未启动'}
        </Badge>
      </div>
      <div className="flex items-center gap-2">
        <button onClick={() => void launch()} className="btn-primary" disabled={!locate?.exe}>
          <ExternalLink size={15} /> 打开豆包
        </button>
        {proxy.running ? (
          <button onClick={stopProxy} className="btn-outline">
            <PowerOff size={15} /> 停止代理
          </button>
        ) : (
          <button onClick={startProxy} className="btn-outline">
            <Power size={15} /> 启动代理
          </button>
        )}
      </div>
    </>
  );
}

/** Buddy 专区顶栏：客户端状态 + auth 登录态 + 账号池概览 + 打开客户端（Buddy 工作区与 Trae/豆包顶栏同构，上下文不串） */
function BuddyTopBar() {
  const pushToast = useAppStore((s) => s.pushToast);
  const [env, setEnv] = useState<WorkBuddyEnvCheck | null>(null);
  const [accountCount, setAccountCount] = useState<number | null>(null);

  useEffect(() => {
    let alive = true;
    api.workbuddy
      .envCheck()
      .then((e) => alive && setEnv(e))
      .catch(() => {});
    api.workbuddy
      .accountsList()
      .then((a) => alive && setAccountCount(a.length))
      .catch(() => alive && setAccountCount(0));
    return () => {
      alive = false;
    };
  }, []);

  const openClient = async () => {
    const exe = env?.exe;
    if (!exe) {
      pushToast('warn', '未检测到 WorkBuddy 客户端，请先安装');
      return;
    }
    try {
      if (exe) await open(`file:///${exe}`);
    } catch (e) {
      pushToast('error', `打开客户端失败：${String(e)}`);
    }
  };

  return (
    <>
      <div className="flex items-center gap-2">
        {env?.installed ? (
          <Badge tone="green" title={env.version ? `WorkBuddy 当前版本：v${env.version}` : 'WorkBuddy 客户端'}>
            <MonitorCheck size={13} /> 客户端已安装
          </Badge>
        ) : (
          <Badge tone="red">
            <MonitorX size={13} /> 客户端未检测到
          </Badge>
        )}
        <Badge tone={env?.running ? 'green' : 'slate'} title="WorkBuddy 桌面客户端运行状态">
          <Server size={13} /> {env?.running ? '运行中' : '未运行'}
        </Badge>
        <Badge tone={env?.auth_file_exists ? 'green' : 'amber'} title="登录态文件（workbuddy-desktop.info）">
          <FileCheck size={13} /> {env?.auth_file_exists ? '登录态正常' : '登录态缺失'}
        </Badge>
        <Badge tone={accountCount && accountCount > 0 ? 'blue' : 'slate'} title="账号池账号数量（账号管理页维护）">
          <KeyRound size={13} /> 账号池 {accountCount ?? '—'} 个
        </Badge>
      </div>
      <div className="flex items-center gap-2">
        <button onClick={() => void openClient()} className="btn-primary" disabled={!env?.exe}>
          <ExternalLink size={15} /> 打开客户端
        </button>
      </div>
    </>
  );
}

export default function TopBar() {
  const activeApp = useAppStore((s) => s.activeApp);
  return (
    <div className="flex h-12 shrink-0 items-center justify-between border-b border-slate-200 bg-white px-4 dark:border-zinc-800 dark:bg-zinc-950">
      {activeApp === 'doubao' ? <DoubaoTopBar /> : activeApp === 'buddy' ? <BuddyTopBar /> : <TraeTopBar />}
    </div>
  );
}
