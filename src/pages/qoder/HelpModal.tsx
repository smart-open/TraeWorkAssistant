import {
  ArrowUpDown,
  Fingerprint,
  Globe,
  KeyRound,
} from 'lucide-react';
import { Modal } from '../../components/ui';

export function QoderHelpModal({ open, onClose }: { open: boolean; onClose: () => void }) {
  return (
    <Modal open={open} onClose={onClose} title="Qoder 账号管理使用帮助" size="xl">
      <div className="space-y-4 text-sm">
        <section className="rounded-lg border border-amber-200 bg-amber-50 p-3 dark:border-amber-700 dark:bg-amber-900/20">
          <h3 className="mb-1.5 flex items-center gap-1.5 font-semibold text-amber-700 dark:text-amber-300">
            <Globe size={15} /> 账号入池流程（两通道）
          </h3>
          <ol className="ml-4 list-decimal space-y-1 text-xs leading-relaxed text-amber-800 dark:text-amber-200">
            <li>
              <b>导入 PAT</b>：在 qoder.com.cn → Integrations 创建 PAT（pt- 前缀），点右上角「导入 PAT」粘贴入池。
            </li>
            <li>
              <b>OAuth 设备流登录</b>：点右上角「OAuth登录」，应用自动发起设备流授权，成功后凭证自动入池。
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
            <Fingerprint size={15} className="text-amber-500" /> 设备指纹
          </h3>
          <p className="text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            每个账号槽位绑定独立的设备指纹（§5.10），切换账号时随快照一并注入，避免 Qoder
            风控因机器标识重复判定多开。点击账号行指纹图标可查看当前绑定详情。
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
      </div>
    </Modal>
  );
}
