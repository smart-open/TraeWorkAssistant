import {
  ArrowUpDown,
  Fingerprint,
  Globe,
  History,
  KeyRound,
  LogIn,
  ScanSearch,
  ShieldAlert,
  TerminalSquare,
} from 'lucide-react';
import { Modal } from '../../components/ui';

export function QoderHelpModal({ open, onClose }: { open: boolean; onClose: () => void }) {
  return (
    <Modal open={open} onClose={onClose} title="Qoder 账号管理使用帮助" size="xl">
      <div className="space-y-4 text-sm">
        <section className="rounded-lg border border-amber-200 bg-amber-50 p-3 dark:border-amber-700 dark:bg-amber-900/20">
          <h3 className="mb-1.5 flex items-center gap-1.5 font-semibold text-amber-700 dark:text-amber-300">
            <Globe size={15} /> 账号入池流程（三通道）
          </h3>
          <ol className="ml-4 list-decimal space-y-1 text-xs leading-relaxed text-amber-800 dark:text-amber-200">
            <li>
              <b>导入 PAT</b>：在 qoder.com.cn → Integrations 创建 PAT（pt- 前缀），点右上角「导入 PAT」粘贴入池。
            </li>
            <li>
              <b>OAuth 设备流登录</b>：点右上角「OAuth登录」，应用自动发起设备流授权，成功后凭证自动入池。
            </li>
            <li>
              <b>扫描本地账号</b>：本机 Qoder CN IDE 已登录时，点「扫描本地账号」一键解密导入。
            </li>
          </ol>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <KeyRound size={15} className="text-amber-500" /> 导入 PAT
          </h3>
          <p className="text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            在 Qoder 官网 Integrations 页面创建 Personal Access Token 后粘贴导入。PAT
            为官方认可凭证（pt- 前缀），可用于积分查询 / 签到 / API 服务。同一账号的 PAT
            与客户端凭证按 token 派生 id，分属两条池记录，互不覆盖。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <Globe size={15} className="text-amber-500" /> OAuth 设备流登录
          </h3>
          <p className="text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            点击「OAuth登录」，应用自动打开浏览器授权页，在浏览器中完成 Qoder 账号授权（登录并确认）后凭证自动回填入池，无需输入设备码。成功后凭证（dt-
            前缀）自动入池，约 30 天自动续期，无需手动维护。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <ScanSearch size={15} className="text-amber-500" /> 扫描本地账号
          </h3>
          <p className="text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            扫描本机 Qoder CN IDE 的登录态存储并解密导入账号池，适合已在 IDE 中登录的用户一键入池。
          </p>
          <p className="mt-1 text-xs font-medium text-amber-600 dark:text-amber-400">
            ⚠ IDE 退出登录后本地凭证已被清空，将无法导入——需先重新登录 IDE。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <History size={15} className="text-amber-500" /> 登录态快照管理
          </h3>
          <p className="text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            点「快照管理」查看按账号槽位存放的登录态快照（
            <code className="mx-0.5 rounded bg-slate-100 px-1 dark:bg-zinc-800">data/profiles_qoder/&lt;账号id&gt;/</code>
            ），支持「恢复」到 Qoder IDE 与删除，均需二次确认。恢复快照时会自动注入该账号绑定的设备指纹，支持多账号并存。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <Fingerprint size={15} className="text-amber-500" /> 设备指纹
          </h3>
          <p className="text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            每个账号槽位绑定独立的设备指纹（§5.10），切换账号时随快照一并注入，避免 Qoder
            风控因机器标识重复判定多开。点击账号行指纹图标可查看当前绑定详情。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <LogIn size={15} className="text-amber-500" /> 切换账号
          </h3>
          <p className="text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            点击账号行「切换」图标：系统自动备份当前 IDE 登录态到当前账号槽位 →
            恢复目标账号快照并注入其绑定设备指纹 → 重启 IDE 自动登录。
          </p>
          <p className="mt-1 text-xs font-medium text-amber-600 dark:text-amber-400">
            ⚠ 目标账号从未备份过快照时，切换会被中止——请先在目标账号登录后备份快照。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <TerminalSquare size={15} className="text-amber-500" /> Qoder CLI 状态桥
          </h3>
          <p className="text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            页面底部展示 Qoder CLI（
            <code className="mx-0.5 rounded bg-slate-100 px-1 dark:bg-zinc-800">~/.qoder-cn</code>
            ）的登录状态 / 版本 / 写入方，为白名单只读透传（R-3），无独立凭证通道，本工具绝不写回。凭证由调度器每 6
            小时兜底刷新（M4）。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <ArrowUpDown size={15} className="text-amber-500" /> 导出 / 导入账号池
          </h3>
          <p className="text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            「导出账号」将账号池打包为 JSON 文件；「导入账号」读取后按 uid 幂等合并——已存在的账号原位更新：仅补全空缺字段，绝不覆盖已有数据。适合跨设备迁移或多机同步。
          </p>
          <p className="mt-1 text-xs font-medium text-amber-600 dark:text-amber-400">
            ⚠ 勾选「附带凭证副本」的导出文件中凭证以 AES-256-GCM 加密写入（需设置导出密码），导入时须提供同一密码——仍等同密码，请妥善保管，切勿通过不可信渠道传输。
          </p>
        </section>

        <section className="rounded-lg border border-rose-200 bg-rose-50 p-3 dark:border-rose-700 dark:bg-rose-900/20">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold text-rose-700 dark:text-rose-300">
            <ShieldAlert size={15} /> 环境重置
          </h3>
          <p className="text-xs leading-relaxed text-rose-800 dark:text-rose-200">
            清除本机 Qoder CN 的认证残留（8 项：IDE 登录态 / 机器标识等，默认勾选已检测到的项），执行时会自动关闭
            Qoder CN，不影响应用本体安装。账号池与已备份的快照不受影响。Qoder 无 SSO
            注销对应物，仅清理本地残留。
          </p>
          <p className="mt-1 text-xs font-medium text-rose-600 dark:text-rose-400">
            ⚠ 所选残留将被永久清除，不可逆，清除后需重新登录才能继续使用。
          </p>
        </section>
      </div>
    </Modal>
  );
}
