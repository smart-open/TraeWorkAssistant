import { useEffect, useState } from 'react';
import { ExternalLink, Loader2, Power, PowerOff, ShieldCheck, ShieldAlert, MonitorCheck, MonitorX, Server, Wifi, Play } from 'lucide-react';
import { useAppStore } from '../store';
import { Badge } from './ui';
import { api } from '../lib/tauri';
import type { AppLocate, WorkBuddyEnvCheck, WbCliStatus } from '../types';

/** API 网关启停（Trae/Buddy 顶栏共用）：直接调 api_server_start/stop 并同步 store */
function useApiGatewayToggle() {
  const pushToast = useAppStore((s) => s.pushToast);
  const apiStatus = useAppStore((s) => s.apiStatus);
  const [busy, setBusy] = useState(false);
  const running = apiStatus?.running ?? false;

  const toggle = async () => {
    if (busy) return;
    setBusy(true);
    try {
      if (running) {
        await api.apiServer.stop();
        useAppStore.setState({ apiStatus: null });
        pushToast('info', 'API 网关已停止');
      } else {
        const s = await api.apiServer.start();
        useAppStore.setState({ apiStatus: s });
        pushToast('success', `API 网关已启动（端口 ${s.port}）`);
      }
    } catch (e) {
      pushToast('error', `API 网关${running ? '停止' : '启动'}失败：${String(e)}`);
    } finally {
      setBusy(false);
    }
  };

  return { running, busy, toggle };
}

/** 网关启停按钮（与启停代理并排） */
function GatewayButton() {
  const { running, busy, toggle } = useApiGatewayToggle();
  return running ? (
    <button onClick={() => void toggle()} className="btn-outline" disabled={busy}>
      <PowerOff size={15} /> 停止网关
    </button>
  ) : (
    <button onClick={() => void toggle()} className="btn-outline" disabled={busy}>
      <Play size={15} /> 启动API网关
    </button>
  );
}

/** Trae 专区顶栏：双应用安装状态 + 证书/代理/API 网关 + 打开应用 */
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
          <Server size={13} /> {apiStatus?.running ? `API网关运行中 :${apiStatus.port}` : 'API网关未启动'}
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
          <button onClick={startProxy} className="btn-outline">
            <Power size={15} /> 启动代理
          </button>
        )}
        <GatewayButton />
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
        <button onClick={() => void launch()} className="btn-outline" disabled={!locate?.exe}>
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

/**
 * Buddy 专区顶栏：左列双客户端安装徽标（hover 显版本）+ 证书/代理/API 网关状态，
 * 右列 打开WorkBuddy / 打开CodeBuddy / 启动代理 / 启动API网关。
 * 与 Trae/豆包顶栏同构（上下文不串）；原登录态/账号池徽标已移除（账号信息看账号管理页）。
 */
function BuddyTopBar() {
  const pushToast = useAppStore((s) => s.pushToast);
  const certInstalled = useAppStore((s) => s.certInstalled);
  const proxy = useAppStore((s) => s.proxy);
  const apiStatus = useAppStore((s) => s.apiStatus);
  const startProxy = useAppStore((s) => s.startProxy);
  const stopProxy = useAppStore((s) => s.stopProxy);
  const [wbEnv, setWbEnv] = useState<WorkBuddyEnvCheck | null>(null);
  const [cbLocate, setCbLocate] = useState<AppLocate | null>(null);
  // CLI 桥状态：CodeBuddy CLI-only 用户（只装 CLI 无桌面版）用 settings_present 兜底判定
  const [cbCli, setCbCli] = useState<WbCliStatus | null>(null);
  // 打开客户端 pending（wb/cb 互斥防连点；执行期间两按钮均禁用）
  const [launching, setLaunching] = useState<'wb' | 'cb' | null>(null);

  useEffect(() => {
    let alive = true;
    api.workbuddy
      .envCheck()
      .then((e) => alive && setWbEnv(e))
      .catch(() => {});
    api.env
      .locate('codebuddy')
      .then((r) => alive && setCbLocate(r))
      .catch(() => {});
    api.workbuddy
      .cliStatus()
      .then((s) => alive && setCbCli(s))
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, []);

  // 打开客户端：走后端 spawn exe（Tauri v2 opener 对可执行文件静默失败，不能用 file:/// 打开）。
  // 按钮不再因未检测到 exe 而哑死禁用：后端 reject 文案含「未检测到」时降级为 warn 提示，其余为 error
  const launchApp = async (which: 'wb' | 'cb') => {
    if (launching) return;
    const label = which === 'wb' ? 'WorkBuddy' : 'CodeBuddy';
    setLaunching(which);
    try {
      if (which === 'wb') await api.env.openWorkbuddyApp();
      else await api.env.openCodebuddyApp();
      pushToast('success', `已启动 ${label}`);
    } catch (err) {
      const msg = String(err);
      if (msg.includes('未检测到')) pushToast('warn', msg);
      else pushToast('error', `打开 ${label} 失败：${msg}`);
    } finally {
      setLaunching(null);
    }
  };

  return (
    <>
      <div className="flex items-center gap-2">
        {wbEnv?.installed ? (
          <Badge tone="green" title={wbEnv.version ? `WorkBuddy 当前版本：v${wbEnv.version}` : 'WorkBuddy 客户端'}>
            <MonitorCheck size={13} /> WorkBuddy已安装
          </Badge>
        ) : (
          <Badge tone="red">
            <MonitorX size={13} /> WorkBuddy未检测到
          </Badge>
        )}
        {cbLocate?.exe ? (
          <Badge tone="green" title={cbLocate.version ? `CodeBuddy 当前版本：v${cbLocate.version}` : 'CodeBuddy 客户端'}>
            <MonitorCheck size={13} /> CodeBuddy已安装
          </Badge>
        ) : cbCli?.settings_present ? (
          <Badge tone="green" title="未检测到 CodeBuddy 桌面版；CLI 桥已就绪（~/.codebuddy/settings.json），切号/轮换可用">
            <MonitorCheck size={13} /> CodeBuddy CLI已就绪
          </Badge>
        ) : (
          <Badge tone="red">
            <MonitorX size={13} /> CodeBuddy未检测到
          </Badge>
        )}
        {certInstalled ? (
          <Badge tone="green">
            <ShieldCheck size={13} /> 证书已信任
          </Badge>
        ) : (
          <Badge tone="amber">
            <ShieldAlert size={13} /> 证书未信任
          </Badge>
        )}
        <Badge tone={proxy.running ? 'blue' : 'slate'}>
          <Wifi size={13} /> {proxy.running ? `代理运行中 :${proxy.port}` : '代理未启动'}
        </Badge>
        <Badge tone={apiStatus?.running ? 'green' : 'slate'}>
          <Server size={13} /> {apiStatus?.running ? `API网关运行中 :${apiStatus.port}` : 'API网关未启动'}
        </Badge>
      </div>
      <div className="flex items-center gap-2">
        <button onClick={() => void launchApp('wb')} className="btn-outline" disabled={launching != null}>
          {launching === 'wb' ? <Loader2 size={15} className="animate-spin" /> : <ExternalLink size={15} />} 打开WorkBuddy
        </button>
        <button onClick={() => void launchApp('cb')} className="btn-outline" disabled={launching != null}>
          {launching === 'cb' ? <Loader2 size={15} className="animate-spin" /> : <ExternalLink size={15} />} 打开CodeBuddy
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
        <GatewayButton />
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
