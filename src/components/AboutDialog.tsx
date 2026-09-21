import { Github, Globe } from 'lucide-react';
import { Modal } from './ui';
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
  APP_VERSION,
  promoBanner,
} from '../lib/about';

export default function AboutDialog({ open, onClose }: { open: boolean; onClose: () => void }) {
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
                v{APP_VERSION}
              </span>
            </div>
            <div className="mt-0.5 text-xs text-slate-500 dark:text-zinc-400">{APP_TAGLINE}</div>
          </div>
        </div>

        {/* 概述 */}
        <div>
          <div className="mb-1 text-xs font-bold uppercase tracking-wide text-slate-400 dark:text-zinc-500">概述</div>
          <p className="leading-relaxed">{APP_OVERVIEW}</p>
        </div>

        {/* 链接（Web 版：新窗口打开） */}
        <div className="flex flex-wrap gap-2">
          <a
            href={LINK_GITHUB}
            target="_blank"
            rel="noreferrer"
            className="inline-flex items-center gap-1.5 rounded-lg border border-slate-200 px-3 py-1.5 text-xs font-medium transition hover:bg-slate-100 dark:border-zinc-700 dark:hover:bg-zinc-800"
          >
            <Github size={14} /> GitHub
          </a>
          <a
            href={LINK_BLOG}
            target="_blank"
            rel="noreferrer"
            className="inline-flex items-center gap-1.5 rounded-lg border border-slate-200 px-3 py-1.5 text-xs font-medium transition hover:bg-slate-100 dark:border-zinc-700 dark:hover:bg-zinc-800"
          >
            <Globe size={14} /> 个人博客
          </a>
          <a
            href={LINK_REPO}
            target="_blank"
            rel="noreferrer"
            className="inline-flex items-center gap-1.5 rounded-lg border border-slate-200 px-3 py-1.5 text-xs font-medium transition hover:bg-slate-100 dark:border-zinc-700 dark:hover:bg-zinc-800"
          >
            <Github size={14} /> 项目仓库 / README
          </a>
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
