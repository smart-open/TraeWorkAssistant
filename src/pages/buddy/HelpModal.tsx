import { CalendarClock, Globe, KeyRound, RefreshCw, ScanSearch, Server, Upload } from 'lucide-react';
import { Modal } from '../../components/ui';

/**
 * Buddy 账号管理使用帮助（Web 版）：
 * 按推荐使用流程组织——OAuth 登录添加账号 → 签到与成长 → 定时任务 → API 服务配置；
 * 桌面版特有功能（保存登录态/切换账号/快照管理/会话备份/CLI 桥接/环境重置）已随桌面壳退役，不再赘述。
 */
export function BuddyHelpModal({ open, onClose }: { open: boolean; onClose: () => void }) {
  return (
    <Modal open={open} onClose={onClose} title="Buddy 账号管理使用帮助" size="xl">
      <div className="space-y-4 text-sm">
        <section className="rounded-lg border border-amber-200 bg-amber-50 p-3 dark:border-amber-700 dark:bg-amber-900/20">
          <h3 className="mb-1.5 flex items-center gap-1.5 font-semibold text-amber-700 dark:text-amber-300">
            <Globe size={15} /> 推荐使用流程（三步）
          </h3>
          <ol className="ml-4 list-decimal space-y-1 text-xs leading-relaxed text-amber-800 dark:text-amber-200">
            <li>
              <b>添加账号</b>：点「OAuth 登录」完成授权自动入池（推荐）；也可「扫描本机账号」导入本机客户端已登录账号，或「导入账号」从 JSON 文件批量导入。
            </li>
            <li>
              <b>开启自动化</b>：到「Buddy · 环境配置」开启定时任务——签到和成长、token 兜底续期、积分快照自动采集，无需每天手动操作。
            </li>
            <li>
              <b>接入 API 服务（可选）</b>：到「API 服务」页启用 WB 上游池并勾选账号，创建 API Key 后即可通过 OpenAI 兼容接口调用。
            </li>
          </ol>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <Globe size={15} className="text-amber-500" /> OAuth 登录（推荐，自动入池）
          </h3>
          <ol className="ml-4 list-decimal space-y-1 text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            <li>点「OAuth 登录」，弹窗显示登录链接并自动打开新标签页（浏览器拦截弹窗时点击链接或复制到新窗口）</li>
            <li>在腾讯 Copilot 授权页完成登录，系统后台自动轮询登录结果</li>
            <li>成功后凭证自动解析入池（仅存本地，全程掩码展示），弹窗提示完成</li>
          </ol>
          <p className="mt-1 text-xs leading-relaxed text-slate-500 dark:text-zinc-400">
            流程超过 5 分钟未收到结果会自动解除等待，最终以账号列表为准；重复点击进行中的登录会安全返回，不会产生冲突。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <ScanSearch size={15} className="text-amber-500" /> 扫描本机账号
          </h3>
          <p className="text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            扫描<b>服务器本机</b> WorkBuddy / CodeBuddy 客户端已登录的账号，一键加入账号池。
          </p>
          <p className="mt-1 text-xs font-medium text-amber-600 dark:text-amber-400">
            ⚠ 仅当服务端与客户端装在同一台机器时可用（Docker 容器部署无客户端，请使用 OAuth 登录）；
            客户端退出登录后凭证已被清空，将无法导入，需先重新登录客户端。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <Upload size={15} className="text-amber-500" /> 导入导出与分组
          </h3>
          <ul className="ml-4 list-disc space-y-1 text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            <li><b>导出账号</b>：JSON 文件备份（默认含凭证），可用于迁移或导入其他实例</li>
            <li><b>导入账号</b>：兼容 Web 简版与桌面版导出格式，导入前预览确认、自动去重</li>
            <li><b>分组管理</b>：新建 / 重命名 / 换色 / 删除分组，签到、积分、API 服务页均可按组筛选</li>
            <li><b>行内「刷新」</b>：用 Refresh Token 续期该账号 accessToken，恢复积分查询 / 签到 / API 池能力</li>
          </ul>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <CalendarClock size={15} className="text-amber-500" /> 签到与定时任务
          </h3>
          <ul className="ml-4 list-disc space-y-1 text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            <li><b>手动签到</b>：「一键签到」页批量签到 + 成长中心自动化（旅行 / 盲盒 / 任务领奖），支持按分组筛选</li>
            <li><b>自动签到</b>：「Buddy · 环境配置」开启自动签到后，每日 <b>09:10</b> 定时执行签到和成长，服务启动时还会自动补签当天漏签账号</li>
            <li><b>定时任务一览</b>：签到和成长 09:10 · token 兜底续期 10:30 · 积分快照 23:30</li>
            <li>任务支持<b>开关</b>与<b>自定义执行时刻</b>（点时刻徽章修改，当天过点自动补跑）；「恢复推荐配置」一键还原默认</li>
            <li>任务失败自动冷却重试，可通过「通知渠道」（系统设置，Trae / Buddy 共用）推送失败提醒</li>
          </ul>
        </section>

        <section className="rounded-lg border border-sky-200 bg-sky-50 p-3 dark:border-sky-700 dark:bg-sky-900/20">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold text-sky-700 dark:text-sky-300">
            <KeyRound size={15} /> 凭证续期说明
          </h3>
          <p className="text-xs leading-relaxed text-sky-800 dark:text-sky-200">
            调度任务每日 10:30 对池内账号做 token 兜底续期。带「需重新登录」红色标记的账号凭证已失效，
            行内续期无法恢复——需重新 OAuth 登录。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <Server size={15} className="text-amber-500" /> API 服务配置
          </h3>
          <ol className="ml-4 list-decimal space-y-1 text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            <li>「API 服务」页开启 WB 上游池，勾选账号加入白名单并保存</li>
            <li>「API 管理 → API Keys 管理」创建子 Key（ck_ 前缀），可设每日配额</li>
            <li>客户端接口地址填 <code className="rounded bg-slate-100 px-1 dark:bg-zinc-800">http://&lt;服务器IP&gt;:8080/v1</code>（与管理面同端口，跟随实际访问地址）</li>
            <li>Buddy 池模型以统一模型目录为准（Buddy 专属模型如 GLM-5.1、Kimi-K2.6 等仅 Buddy 池提供）</li>
          </ol>
          <p className="mt-1 flex items-center gap-1 text-xs leading-relaxed text-slate-500 dark:text-zinc-400">
            <RefreshCw size={12} className="shrink-0" /> 调度策略（smart / 优先级）与指纹清洗开关在「Buddy · 环境配置」调整。
          </p>
        </section>
      </div>
    </Modal>
  );
}
