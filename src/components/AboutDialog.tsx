import { useEffect, useState } from 'react';
import { getVersion } from '@tauri-apps/api/app';
import { open as shellOpen } from '@tauri-apps/plugin-shell';
import {
  AlertCircle,
  CheckCircle2,
  Download,
  Github,
  Globe,
  Loader2,
  RefreshCw,
} from 'lucide-react';
import { Modal } from './ui';
import { api } from '../lib/tauri';
import { useAppStore } from '../store';
import type { UpdateCheckResult, UpdateDownloadProgress, UpdateDownloaded } from '../types';
import {
  APP_NAME,
  APP_TAGLINE,
  APP_OVERVIEW,
  APP_AUTHOR,
  APP_COPYRIGHT,
  APP_DISCLAIMER,
  LINK_GITHUB,
  LINK_BLOG,
  LINK_REPO,
  promoBanner,
} from '../lib/about';

type UpdateState =
  | { k: 'idle' }
  | { k: 'checking' }
  | { k: 'latest'; currentVersion: string }
  // 确认一：发现新版本，等待用户确认下载
  | { k: 'available'; info: UpdateCheckResult }
  | { k: 'downloading'; info: UpdateCheckResult; percent: number }
  // 确认二：下载完成，等待用户确认安装（/P /UPDATE /R，完成后自动重启）
  | { k: 'downloaded'; info: UpdateCheckResult; file: UpdateDownloaded }
  | { k: 'installing' }
  | { k: 'error'; msg: string };

function fmtSize(bytes: number): string {
  // GitHub API 异常时 size 可能为 0：显示「未知大小」而非误导性的 "0 B"
  if (bytes <= 0) return '未知大小';
  if (bytes >= 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(0)} KB`;
  return `${bytes} B`;
}

export default function AboutDialog({ open, onClose }: { open: boolean; onClose: () => void }) {
  // F-75 M3-3.3：更新安装引导按平台分派（win NSIS 自动重启 / mac dmg 人工拖拽 + Gatekeeper 放行）
  const platform = useAppStore((s) => s.platform);
  const isMac = platform === 'macos';
  const [upd, setUpd] = useState<UpdateState>({ k: 'idle' });
  // 版本号运行时读取（Tauri getVersion() ← Cargo.toml 单一来源），不再前端硬编码
  const [appVersion, setAppVersion] = useState('');

  useEffect(() => {
    getVersion()
      .then(setAppVersion)
      .catch(() => setAppVersion('dev'));
  }, []);

  // 重置状态 & 订阅下载进度（关闭对话框即取消 UI 订阅；下载在 Rust 侧继续不受影响）
  useEffect(() => {
    if (!open) {
      setUpd({ k: 'idle' });
      return;
    }
    let un1: (() => void) | undefined;
    let un2: (() => void) | undefined;
    let alive = true;
    void (async () => {
      const u1 = await api.updater.onDownloadProgress((p: UpdateDownloadProgress) => {
        setUpd((s) =>
          s.k === 'downloading' ? { ...s, percent: p.percent } : s,
        );
      });
      const u2 = await api.updater.onInstalling(() => {
        if (alive) setUpd({ k: 'installing' });
      });
      if (alive) {
        un1 = u1;
        un2 = u2;
      } else {
        u1();
        u2();
      }
    })();
    return () => {
      alive = false;
      un1?.();
      un2?.();
    };
  }, [open]);

  // 第一步：下载更新包（确认一后触发；下载在 Rust 侧进行，进度经事件回推，完成后回到确认二）
  const startDownload = (info: UpdateCheckResult) => {
    setUpd({ k: 'downloading', info, percent: 0 });
    void api.updater
      .download({
        downloadUrl: info.download_url,
        assetName: info.asset_name,
        expectedVersion: info.latest_version,
        expectedSha256: info.sha256 ?? null,
      })
      .then((file) => {
        setUpd((s) =>
          s.k === 'downloading' ? { k: 'downloaded', info: s.info, file } : s,
        );
      })
      .catch((e) => setUpd({ k: 'error', msg: String(e) }));
  };

  // 第二步：启动安装器（确认二后触发；win /P /UPDATE /R 自动重启 / mac open dmg 人工安装）
  const runInstaller = (file: UpdateDownloaded) => {
    setUpd({ k: 'installing' });
    void api.updater
      .runInstaller({ filePath: file.file_path, assetName: file.asset_name })
      .catch((e) => setUpd({ k: 'error', msg: String(e) }));
  };

  // mac 专用：人工拖拽安装完成后一键重启（LaunchServices 拉起 /Applications 新版）
  const restartAfterInstall = () => {
    setUpd({ k: 'installing' });
    void api.updater
      .restartApp()
      .catch((e) => setUpd({ k: 'error', msg: String(e) }));
  };

  const checkUpdate = async () => {
    if (upd.k === 'checking' || upd.k === 'downloading' || upd.k === 'installing') return;
    setUpd({ k: 'checking' });
    try {
      const r = await api.updater.check();
      if (r.has_update) {
        // 有新版本：停在「确认一」，由用户决定是否下载
        setUpd({ k: 'available', info: r });
      } else {
        setUpd({ k: 'latest', currentVersion: r.current_version });
      }
    } catch (e) {
      setUpd({ k: 'error', msg: String(e) });
    }
  };

  const openUrl = async (url: string) => {
    try {
      await shellOpen(url);
    } catch {
      window.open(url, '_blank');
    }
  };

  const busy = upd.k === 'checking' || upd.k === 'downloading' || upd.k === 'installing';

  return (
    <Modal open={open} onClose={onClose} title="关于">
      <div className="space-y-4 text-sm text-slate-600 dark:text-zinc-300">
        {/* 品牌头 */}
        <div className="flex items-center gap-3">
          <span className="flex h-11 w-11 items-center justify-center rounded-xl bg-gradient-to-br from-brand-500 to-brand-700 text-white shadow-md dark:from-brand-400 dark:to-brand-600">
            <svg viewBox="0 0 24 24" className="h-6 w-6" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
              <path d="M12 8V4H8" />
              <rect width="16" height="12" x="4" y="8" rx="2" />
              <path d="M2 14h2" />
              <path d="M20 14h2" />
              <path d="M15 13v2" />
              <path d="M9 13v2" />
            </svg>
          </span>
          <div>
            <div className="flex items-center gap-2 text-base font-bold text-slate-800 dark:text-zinc-100">
              {APP_NAME}
              <span className="rounded-md bg-slate-100 px-1.5 py-0.5 text-[11px] font-semibold text-slate-500 dark:bg-zinc-800 dark:text-zinc-400">
                v{appVersion}
              </span>
              {/* 检查更新：分析 GitHub Releases，发现新版本后两步确认（下载 → 安装） */}
              <button
                onClick={() => void checkUpdate()}
                disabled={busy}
                className="inline-flex items-center gap-1 rounded-md border border-sky-200 bg-sky-50 px-2 py-0.5 text-[11px] font-semibold text-sky-600 transition hover:bg-sky-100 disabled:cursor-not-allowed disabled:opacity-60 dark:border-sky-500/30 dark:bg-sky-500/10 dark:text-sky-400 dark:hover:bg-sky-500/20"
                title="检查更新（发现新版本后确认下载与安装）"
              >
                {upd.k === 'checking' || upd.k === 'downloading' || upd.k === 'installing' ? (
                  <Loader2 size={12} className="animate-spin" />
                ) : (
                  <RefreshCw size={12} />
                )}
                检查更新
              </button>
            </div>
            <div className="mt-0.5 text-xs text-slate-500 dark:text-zinc-400">{APP_TAGLINE}</div>
            {/* 更新状态行 */}
            {upd.k === 'latest' && (
              <div className="mt-1.5 flex items-center gap-1 text-xs font-medium text-emerald-600 dark:text-emerald-400">
                <CheckCircle2 size={13} /> 当前已是最新版本（v{upd.currentVersion}）
              </div>
            )}
            {/* 确认一：发现新版本，等用户确认下载 */}
            {upd.k === 'available' && (
              <div className="mt-1.5 flex max-w-sm flex-wrap items-center gap-1.5 text-xs font-medium text-amber-600 dark:text-amber-400">
                <Download size={13} />
                <span>
                  发现新版本 v{upd.info.latest_version}（{fmtSize(upd.info.size)}）
                </span>
                <button
                  onClick={() => startDownload(upd.info)}
                  className="rounded-md bg-sky-600 px-2 py-0.5 text-[11px] font-semibold text-white transition hover:bg-sky-700 dark:bg-sky-500 dark:hover:bg-sky-400"
                >
                  下载更新
                </button>
                <button
                  onClick={() => setUpd({ k: 'idle' })}
                  className="rounded-md border border-slate-200 px-2 py-0.5 text-[11px] font-medium text-slate-500 transition hover:bg-slate-100 dark:border-zinc-700 dark:text-zinc-400 dark:hover:bg-zinc-800"
                >
                  忽略
                </button>
              </div>
            )}
            {upd.k === 'downloading' && (
              <div className="mt-1.5 w-64">
                <div className="mb-1 flex items-center justify-between text-xs font-medium text-sky-600 dark:text-sky-400">
                  <span className="inline-flex items-center gap-1">
                    <Download size={13} /> 正在下载更新包 v{upd.info.latest_version}…
                  </span>
                  <span>{upd.percent}%</span>
                </div>
                <div className="h-1.5 w-full overflow-hidden rounded-full bg-slate-200 dark:bg-zinc-700">
                  <div
                    className="h-full rounded-full bg-sky-500 transition-all"
                    style={{ width: `${upd.percent}%` }}
                  />
                </div>
                <div className="mt-1 text-[11px] text-slate-400 dark:text-zinc-500">
                  {upd.info.asset_name} · {fmtSize(upd.info.size)} · 完成后将询问是否安装
                </div>
              </div>
            )}
            {/* 确认二：下载完成，等用户确认安装 */}
            {upd.k === 'downloaded' && (
              <div className="mt-1.5 flex max-w-sm flex-wrap items-center gap-1.5 text-xs font-medium text-emerald-600 dark:text-emerald-400">
                <CheckCircle2 size={13} />
                <span>
                  更新包下载完成（v{upd.info.latest_version} · {fmtSize(upd.file.size)}）
                </span>
                <button
                  onClick={() => runInstaller(upd.file)}
                  className="rounded-md bg-sky-600 px-2 py-0.5 text-[11px] font-semibold text-white transition hover:bg-sky-700 dark:bg-sky-500 dark:hover:bg-sky-400"
                >
                  {isMac ? '打开安装包' : '立即安装并重启'}
                </button>
                <button
                  onClick={() => setUpd({ k: 'idle' })}
                  className="rounded-md border border-slate-200 px-2 py-0.5 text-[11px] font-medium text-slate-500 transition hover:bg-slate-100 dark:border-zinc-700 dark:text-zinc-400 dark:hover:bg-zinc-800"
                >
                  稍后
                </button>
              </div>
            )}
            {upd.k === 'installing' && (isMac ? (
              // mac dmg 人工安装引导（F-75 M3-3.3/3.6）：拖拽安装 + Gatekeeper 放行 + 一键重启
              <div className="mt-1.5 max-w-sm space-y-1.5 rounded-lg border border-sky-200 bg-sky-50 p-2.5 text-[11px] leading-relaxed text-slate-600 dark:border-sky-500/30 dark:bg-sky-500/10 dark:text-zinc-300">
                <div className="font-semibold text-sky-600 dark:text-sky-400">安装包已打开，请完成以下步骤：</div>
                <div>① 将窗口中的「{APP_NAME}」拖入右侧的 Applications 文件夹，选择「替换」完成覆盖安装</div>
                <div>
                  ② 若 macOS 提示「无法验证开发者」：在「应用程序」文件夹中<b>右键点击</b>应用 → 「打开」→
                  再点「打开」（首次一次即可）；或在终端执行
                  <code className="mx-1 rounded bg-slate-100 px-1 py-0.5 font-mono text-[10px] dark:bg-zinc-800">
                    xattr -d com.apple.quarantine "/Applications/{APP_NAME}.app"
                  </code>
                </div>
                <div className="flex items-center gap-1.5 pt-0.5">
                  <button
                    onClick={restartAfterInstall}
                    className="rounded-md bg-sky-600 px-2 py-0.5 text-[11px] font-semibold text-white transition hover:bg-sky-700 dark:bg-sky-500 dark:hover:bg-sky-400"
                  >
                    安装完成，重启应用
                  </button>
                  <span className="text-slate-400 dark:text-zinc-500">拖拽完成后点击</span>
                </div>
              </div>
            ) : (
              <div className="mt-1.5 flex items-center gap-1 text-xs font-medium text-sky-600 dark:text-sky-400">
                <Loader2 size={13} className="animate-spin" /> 正在安装更新（进度条安装中），完成后应用将自动重启…
              </div>
            ))}
            {upd.k === 'error' && (
              <div className="mt-1.5 flex max-w-sm items-start gap-1 text-xs text-rose-600 dark:text-rose-400">
                <AlertCircle size={13} className="mt-0.5 shrink-0" />
                <span className="whitespace-pre-wrap leading-relaxed">{upd.msg}</span>
              </div>
            )}
          </div>
        </div>

        {/* 概述 */}
        <div>
          <div className="mb-1 text-xs font-bold uppercase tracking-wide text-slate-400 dark:text-zinc-500">概述</div>
          <p className="leading-relaxed">{APP_OVERVIEW}</p>
        </div>

        {/* 链接 */}
        <div className="flex flex-wrap gap-2">
          <button
            onClick={() => void openUrl(LINK_GITHUB)}
            className="inline-flex items-center gap-1.5 rounded-lg border border-slate-200 px-3 py-1.5 text-xs font-medium transition hover:bg-slate-100 dark:border-zinc-700 dark:hover:bg-zinc-800"
          >
            <Github size={14} /> GitHub
          </button>
          <button
            onClick={() => void openUrl(LINK_BLOG)}
            className="inline-flex items-center gap-1.5 rounded-lg border border-slate-200 px-3 py-1.5 text-xs font-medium transition hover:bg-slate-100 dark:border-zinc-700 dark:hover:bg-zinc-800"
          >
            <Globe size={14} /> 个人博客
          </button>
          <button
            onClick={() => void openUrl(LINK_REPO)}
            className="inline-flex items-center gap-1.5 rounded-lg border border-slate-200 px-3 py-1.5 text-xs font-medium transition hover:bg-slate-100 dark:border-zinc-700 dark:hover:bg-zinc-800"
          >
            <Github size={14} /> 项目仓库 / README
          </button>
        </div>

        {/* 软件宣传图 */}
        <div className="overflow-hidden rounded-xl border border-slate-200 dark:border-zinc-700">
          <img
            src={promoBanner}
            alt="AI Work 助手 — 多账号签到与管理一站式工作台"
            className="w-full"
          />
        </div>

        {/* 作者与版权 */}
        <div className="space-y-0.5 text-xs text-slate-500 dark:text-zinc-400">
          <div>作者：{APP_AUTHOR}</div>
          <div>{APP_COPYRIGHT}</div>
          <div className="pt-1 text-[11px] leading-relaxed text-slate-400 dark:text-zinc-500">{APP_DISCLAIMER}</div>
        </div>
      </div>
    </Modal>
  );
}
