import {
  DatabaseBackup,
  Globe,
  History,
  KeyRound,
  LogIn,
  Save,
  ScanSearch,
  ShieldAlert,
  TerminalSquare,
} from 'lucide-react';
import { Modal } from '../../components/ui';

export function BuddyHelpModal({ open, onClose }: { open: boolean; onClose: () => void }) {
  return (
    <Modal open={open} onClose={onClose} title="Buddy 账号管理使用帮助" size="xl">
      <div className="space-y-4 text-sm">
        <section className="rounded-lg border border-amber-200 bg-amber-50 p-3 dark:border-amber-700 dark:bg-amber-900/20">
          <h3 className="mb-1.5 flex items-center gap-1.5 font-semibold text-amber-700 dark:text-amber-300">
            <Globe size={15} /> 添加账号流程（三步）
          </h3>
          <ol className="ml-4 list-decimal space-y-1 text-xs leading-relaxed text-amber-800 dark:text-amber-200">
            <li>
              <b>添加账号</b>：点右上角「OAuth 扫码」完成登录并自动入池；也可「导入本机账号」扫描本机已登录客户端。
            </li>
            <li>
              <b>登录客户端</b>：打开 WorkBuddy（或 CodeBuddy）客户端，登录刚添加的账号。
            </li>
            <li>
              <b>保存登录会话</b>：回到本页，点该账号行「保存当前登录态」图标并选择目标应用，备份当前登录会话——此后即可随时「切换」回来。
            </li>
          </ol>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <Globe size={15} className="text-amber-500" /> OAuth 扫码
          </h3>
          <p className="text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            点击「OAuth 扫码」，应用会自动打开浏览器完成登录，成功后自动解析凭证并入账号池（凭证仅写入本地 token
            store，全程掩码不上传）。浏览器未自动打开时，弹框内可「复制完整链接」手动打开；扫码超过 5
            分钟未收到结果会自动解除等待，最终结果以账号列表为准。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <ScanSearch size={15} className="text-amber-500" /> 导入本机账号
          </h3>
          <p className="text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            扫描本机 WorkBuddy 客户端已登录的账号，一键加入账号池。
          </p>
          <p className="mt-1 text-xs font-medium text-amber-600 dark:text-amber-400">
            ⚠ 客户端退出登录后登录凭证已被清空，将无法导入——需先重新登录客户端。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <Save size={15} className="text-amber-500" /> 保存当前登录态
          </h3>
          <p className="text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            点击账号行「保存」图标并选择目标应用（WorkBuddy / CodeBuddy）：系统自动关闭客户端 →
            备份该应用的登录态文件 → 重新启动。WorkBuddy 与 CodeBuddy
            双端快照相互独立（profiles_workbuddy / profiles_codebuddy），同一账号可分别为两个应用保存。
          </p>
          <p className="mt-1 text-xs font-medium text-amber-600 dark:text-amber-400">
            ⚠ 首次使用前，请先在对应客户端中登录目标账号，然后点「保存」图标创建快照。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <LogIn size={15} className="text-amber-500" /> 切换账号
          </h3>
          <p className="text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            点击账号行「切换」图标并选择目标应用：系统自动保存当前登录态到当前账号槽位 →
            恢复目标账号在该应用的登录态 → 重启客户端自动登录。
          </p>
          <p className="mt-1 text-xs font-medium text-amber-600 dark:text-amber-400">
            ⚠ 目标账号在所选应用下从未保存过登录态时，切换会被中止——请先用「保存」图标创建快照；带「需重新登录」标记的账号需先重新登录客户端并保存登录态。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <KeyRound size={15} className="text-amber-500" /> 凭证续期
          </h3>
          <p className="text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            点击账号行「续期」图标，用 refreshToken 换新 accessToken，恢复积分查询 / 签到 /
            API 池服务等凭证能力。带「需重新登录」红色标记的账号凭证已失效，续期无法恢复——需重新 OAuth 扫码或在客户端登录后保存登录态。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <TerminalSquare size={15} className="text-amber-500" /> CodeBuddy CLI 桥接
          </h3>
          <p className="text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            点击账号行「CLI」图标，将该账号设为 CodeBuddy CLI 的当前账号（写入
            <code className="mx-0.5 rounded bg-slate-100 px-1 dark:bg-zinc-800">~/.codebuddy/settings.json</code>
            ）。需账号已录入凭证副本（有 refreshToken）。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <DatabaseBackup size={15} className="text-amber-500" /> 会话备份 / 恢复 / 复制
          </h3>
          <p className="text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            页面顶部「WB 会话 / CB 会话」选择会话域，决定备份、恢复、复制作用于
            <code className="mx-0.5 rounded bg-slate-100 px-1 dark:bg-zinc-800">~/.workbuddy</code>还是
            <code className="mx-0.5 rounded bg-slate-100 px-1 dark:bg-zinc-800">~/.codebuddy</code>
            目录。「备份会话」打包 projects 与双 db 快照；之后可「恢复会话」回滚，或「复制会话」到其他账号（复制以全新会话 id
            进行并注册云端映射，执行前自动快照数据库）。执行时会自动关闭对应客户端。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <History size={15} className="text-amber-500" /> 登录态快照管理
          </h3>
          <p className="text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            点「快照管理」查看 WorkBuddy / CodeBuddy 双端快照槽位（按账号命名，含更新时间、大小、文件数），
            支持「恢复并启动」与删除，均需二次确认。
          </p>
        </section>

        <section className="rounded-lg border border-rose-200 bg-rose-50 p-3 dark:border-rose-700 dark:bg-rose-900/20">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold text-rose-700 dark:text-rose-300">
            <ShieldAlert size={15} /> 环境重置
          </h3>
          <p className="text-xs leading-relaxed text-rose-800 dark:text-rose-200">
            彻底清除本机 WorkBuddy 的全部认证残留（客户端回到未登录态），可同时注销 Keycloak SSO
            会话。账号池与已备份的登录态、会话数据不受影响。
          </p>
          <p className="mt-1 text-xs font-medium text-rose-600 dark:text-rose-400">
            ⚠ 所选残留将被永久清除，不可逆，请谨慎使用。
          </p>
        </section>
      </div>
    </Modal>
  );
}
