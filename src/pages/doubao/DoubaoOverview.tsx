import { useEffect, useState } from 'react';
import { RefreshCw, ShieldCheck, Users, FolderOpen } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { StatCard, Badge } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import type { AppLocate, DoubaoAccountView } from '../../types';

/** 豆包接入路线（doubao-trae-switch-plan.md §4 实施计划；已落地：P0 环境识别 + P2 快照切换/账号池） */
const ROADMAP: { phase: string; title: string; desc: string; done?: boolean }[] = [
  { phase: 'P0', title: '安装位置自动识别', desc: 'app_locate 四级探测已支持豆包档案（注册表 → 默认路径 → 进程反查）', done: true },
  { phase: 'P2', title: '目录级快照切换', desc: 'User Data 白名单快照（Cookies / Local State / leveldb…）+ doubao_accounts.json 账号池', done: true },
  { phase: 'P3', title: '会话续期定时任务', desc: 'sid_guard 30 天滑动续期，每日轻量接口保活 + 到期桌面通知' },
  { phase: 'P4', title: '会员额度展示', desc: 'MITM 抓包固化订阅额度接口，额度条展示（专业版/生图/视频）' },
  { phase: 'P5', title: 'Cookie 级热切换', desc: 'v10/DPAPI 解密 sessionid 池化，进程内重写 Cookies 表免重启切换' },
];

export default function DoubaoOverview() {
  const pushToast = useAppStore((s) => s.pushToast);
  const setView = useAppStore((s) => s.setView);
  const [locate, setLocate] = useState<AppLocate | null>(null);
  const [accounts, setAccounts] = useState<DoubaoAccountView[]>([]);
  const [refreshing, setRefreshing] = useState(false);

  const refresh = async () => {
    setRefreshing(true);
    try {
      // 环境检测与账号池并行；账号池失败不阻断概述
      const [loc, accs] = await Promise.all([
        api.env.locate('doubao'),
        api.doubao.accountsList().catch(() => [] as DoubaoAccountView[]),
      ]);
      setLocate(loc);
      setAccounts(accs);
    } catch (err) {
      pushToast('error', `豆包环境检测失败：${String(err)}`);
    } finally {
      setRefreshing(false);
    }
  };

  useEffect(() => {
    void refresh();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const installed = !!locate?.exe;
  const sourceText =
    locate?.source === 'settings'
      ? '手动指定'
      : locate?.source === 'registry'
        ? '注册表'
        : locate?.source === 'default'
          ? '默认路径'
          : locate?.source === 'process'
            ? '进程反查'
            : '未检测到';
  const snapshotCount = accounts.filter((a) => a.has_snapshot).length;
  const currentAccount = accounts.find((a) => a.is_current);

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="豆包 · 概述"
        desc="豆包桌面版账号管理与自动化 · 接入开发中（方案见 doubao-trae-switch-plan.md）"
        actions={
          <button onClick={() => void refresh()} className="btn-outline" disabled={refreshing}>
            <RefreshCw size={15} className={refreshing ? 'animate-spin' : ''} /> 刷新
          </button>
        }
      />

      <div className="mt-5 grid grid-cols-2 gap-4 xl:grid-cols-4">
        <button className="text-left" onClick={() => setView('doubao-accounts')}>
          <StatCard
            label="账号总数"
            value={String(accounts.length)}
            hint={
              currentAccount
                ? `当前：${currentAccount.name}`
                : accounts.length > 0
                  ? '尚无切换记录'
                  : '在账号管理页保存登录态后计入'
            }
            tone="brand"
          />
        </button>
        <StatCard
          label="安装情况"
          value={installed ? '已安装' : '未检测到'}
          hint={locate?.version ? `${sourceText} · ${locate.version}` : sourceText}
          tone={installed ? 'green' : 'amber'}
        />
        <button className="text-left" onClick={() => setView('doubao-accounts')}>
          <StatCard
            label="登录态快照"
            value={String(snapshotCount)}
            hint={snapshotCount > 0 ? '可随时切换/恢复' : '随首次保存登录态生成'}
            tone="violet"
          />
        </button>
        <StatCard
          label="数据目录"
          value={locate?.user_data_dir ? '已定位' : '—'}
          hint={locate?.user_data_dir}
          tone="amber"
        />
      </div>

      <div className="mt-5 card p-4">
        <div className="mb-3 flex items-center gap-2">
          <ShieldCheck size={16} className="text-emerald-500" />
          <span className="text-sm font-medium">接入路线（doubao-trae-switch-plan.md §4）</span>
        </div>
        <div className="space-y-2">
          {ROADMAP.map((r) => (
            <div key={r.phase} className="flex items-start gap-3 rounded-lg border border-slate-100 p-3 dark:border-zinc-800">
              <Badge tone={r.done ? 'green' : 'slate'}>{r.phase}</Badge>
              <div className="min-w-0 flex-1">
                <div className="text-sm font-medium">
                  {r.title}
                  {r.done && <span className="ml-2 text-xs font-normal text-emerald-600 dark:text-emerald-400">已完成</span>}
                </div>
                <div className="text-xs text-slate-500">{r.desc}</div>
              </div>
            </div>
          ))}
        </div>
      </div>

      <div className="mt-4 flex items-start gap-2 rounded-lg border border-slate-100 p-3 text-xs text-slate-500 dark:border-zinc-800">
        <Users size={14} className="mt-0.5 shrink-0" />
        <span>
          合规说明：豆包 / Trae 均为第三方账号体系，本工具仅管理本人合法持有的账号，不破解、不绕过付费；会员额度接口仅做展示。sessionid 等凭证等同密码，入库文件本地存储并全程掩码展示。
        </span>
      </div>
    </div>
  );
}
