import { CalendarClock, Globe, KeyRound, LogIn, RefreshCw, Server, Upload } from 'lucide-react';
import { Modal } from '../../components/ui';

/**
 * Trae 账号管理使用帮助（Web 版）：
 * 按推荐使用流程组织——OAuth 登录添加账号 → 签到 → 定时任务 → API 服务配置；
 * 桌面版特有功能（保存登录态/切换账号/快照管理/重置设备 ID/代理抓取）已随桌面壳退役，不再赘述。
 */
export function HelpModal({ open, onClose }: { open: boolean; onClose: () => void }) {
  return (
    <Modal open={open} onClose={onClose} title="账号管理使用帮助" size="xl">
      <div className="space-y-4 text-sm">
        <section className="rounded-lg border border-amber-200 bg-amber-50 p-3 dark:border-amber-700 dark:bg-amber-900/20">
          <h3 className="mb-1.5 flex items-center gap-1.5 font-semibold text-amber-700 dark:text-amber-300">
            <Globe size={15} /> 推荐使用流程（三步）
          </h3>
          <ol className="ml-4 list-decimal space-y-1 text-xs leading-relaxed text-amber-800 dark:text-amber-200">
            <li>
              <b>添加账号</b>：点右上角「OAuth 登录」完成授权自动入池（推荐）；也可「添加账号」手动粘贴 JWT，或「导入账号」从 JSON 文件批量导入。
            </li>
            <li>
              <b>开启自动化</b>：到「Trae · 环境配置」开启定时任务——JWT 自动续期、每日自动签到、模型列表同步、积分快照自动采集，无需每天手动操作。
            </li>
            <li>
              <b>接入 API 服务（可选）</b>：到「API 服务」页勾选账号入池并保存，创建 API Key 后即可通过 OpenAI 兼容接口调用。
            </li>
          </ol>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <Globe size={15} className="text-amber-500" /> OAuth 登录（推荐，自动入池）
          </h3>
          <ol className="ml-4 list-decimal space-y-1 text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            <li>点「OAuth 登录」，系统生成登录链接并在浏览器打开 Trae 授权页</li>
            <li>在授权页完成登录，页面跳转到 <code className="rounded bg-slate-100 px-1 dark:bg-zinc-800">127.0.0.1:17388/authorize?…</code> 的「无法连接」页——这是正常现象</li>
            <li>复制该页地址栏的<b>完整 URL</b>，粘贴回弹窗提交</li>
            <li>系统自动解析凭证并完成登录，账号入池（无账号时自动生成设备身份，无需本机安装 Trae）</li>
          </ol>
          <p className="mt-1 text-xs leading-relaxed text-slate-500 dark:text-zinc-400">
            适合首次添加账号或 JWT 过期后重新登录；重复点击已进行中的登录流程会安全返回，不会产生冲突。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <Upload size={15} className="text-amber-500" /> 其他添加方式与账号维护
          </h3>
          <ul className="ml-4 list-disc space-y-1 text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            <li><b>添加账号</b>：手动粘贴 JWT（Token / Refresh Token），适合已有凭证的场景</li>
            <li><b>导入 / 导出账号</b>：JSON 文件批量迁移，兼容 Web 简版与桌面版导出格式；导出可用于备份</li>
            <li><b>分组管理</b>：新建 / 重命名 / 删除分组，签到、积分、API 服务页均可按组筛选与调度</li>
            <li><b>行内「刷新」</b>：用 Refresh Token 续期该账号 JWT（带闪电标记的账号支持）</li>
          </ul>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <CalendarClock size={15} className="text-amber-500" /> 签到与定时任务
          </h3>
          <ul className="ml-4 list-disc space-y-1 text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            <li><b>手动签到</b>：「一键签到」页批量签到（跳过已签 / 过期，失败自动重试两轮），支持按分组筛选</li>
            <li><b>自动签到</b>：「Trae · 环境配置 → 定时任务」默认每日 <b>09:00</b> 自动执行，无需人工干预</li>
            <li><b>定时任务一览</b>：JWT 自动续期 05:30 · 模型列表同步 05:40 · 每日签到 09:00 · 积分快照 23:40</li>
            <li>任务支持<b>开关</b>与<b>自定义执行时刻</b>（点时刻徽章修改，当天过点自动补跑）；「恢复推荐配置」一键还原默认</li>
            <li>任务失败自动冷却 30 分钟重试，并可通过「通知渠道」推送失败提醒</li>
          </ul>
        </section>

        <section className="rounded-lg border border-sky-200 bg-sky-50 p-3 dark:border-sky-700 dark:bg-sky-900/20">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold text-sky-700 dark:text-sky-300">
            <KeyRound size={15} /> JWT 续期说明
          </h3>
          <p className="text-xs leading-relaxed text-sky-800 dark:text-sky-200">
            JWT 默认 13 天过期。带刷新令牌的账号支持自动续期：调度任务每日 05:30 批量续期临期账号，
            请求时也会在临期 48 小时内自动懒刷新。刷新令牌失效（红色标记）的账号需重新 OAuth 登录恢复。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <Server size={15} className="text-amber-500" /> API 服务配置
          </h3>
          <ol className="ml-4 list-decimal space-y-1 text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            <li>「API 服务」页勾选要入池的账号并保存（或开启「自动纳入全部账号」）</li>
            <li>「API 管理 → API Keys 管理」创建子 Key（ck_ 前缀），可设每日配额</li>
            <li>客户端接口地址填 <code className="rounded bg-slate-100 px-1 dark:bg-zinc-800">http://&lt;服务器IP&gt;:8080/v1</code>（与管理面同端口，跟随实际访问地址）</li>
            <li>请求按模型 ID 匹配资源池（Trae / Buddy / 自定义模型），「接口配置」Tab 可设默认模型</li>
          </ol>
          <p className="mt-1 flex items-center gap-1 text-xs leading-relaxed text-slate-500 dark:text-zinc-400">
            <RefreshCw size={12} className="shrink-0" /> 令牌泄漏时可在 API Keys 页直接禁用或删除；用量与日志在「用量统计 / 日志」查看。
          </p>
        </section>
      </div>
    </Modal>
  );
}
