import { Camera, Globe, KeyRound, LogIn, RotateCcw, Save, Zap } from 'lucide-react';
import { Modal } from '../../components/ui';

export function HelpModal({ open, onClose }: { open: boolean; onClose: () => void }) {
  return (
    <Modal open={open} onClose={onClose} title="账号管理使用帮助" size="xl">
      <div className="space-y-4 text-sm">
        <section className="rounded-lg border border-amber-200 bg-amber-50 p-3 dark:border-amber-700 dark:bg-amber-900/20">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold text-amber-700 dark:text-amber-300">
            <Globe size={15} /> OAuth 登录（自动保存账号）
          </h3>
          <p className="text-xs leading-relaxed text-amber-800 dark:text-amber-200">
            点击「OAuth 登录」按钮，在浏览器中完成 Trae 账号登录（Trae Work 与 Trae 同一账号体系，登录任一应用均可）。
            登录完成后将回调 URL 粘贴回应用，系统会自动解析 JWT 并保存账号信息，无需手动粘贴 token。
            适合首次添加账号或 JWT 过期后重新登录。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <Save size={15} className="text-amber-500" /> 保存当前登录态
          </h3>
          <p className="text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            在 Trae Work 或 Trae 中登录某个账号后，点击该账号行的「保存」图标并选择目标应用（Trae Work / Trae），
            系统会关闭所选应用 → 精准备份 9 类核心登录文件（storage.json、state.vscdb、machineid、aha、Network 等）
            → 重新启动所选应用。每个账号的登录态独立存储，互不干扰。
          </p>
          <p className="mt-1 text-xs font-medium text-amber-600 dark:text-amber-400">
            ⚠ 首次使用前，请先在对应应用中登录目标账号，然后点击「保存」图标并选择该应用创建快照。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <LogIn size={15} className="text-amber-500" /> 切换账号流程
          </h3>
          <ol className="ml-4 list-decimal space-y-1 text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            <li>点击目标账号行的「切换」图标，并选择目标应用（Trae Work 或 Trae）</li>
            <li>系统自动保存当前登录态到当前账号槽位（如果已知当前账号 ID）</li>
            <li>同时备份到 <code className="rounded bg-slate-100 px-1 dark:bg-zinc-800">last</code> 槽位作为安全回退</li>
            <li>恢复目标账号在该应用的登录态（含设备标识）</li>
            <li>重新启动所选应用，自动以目标账号登录</li>
          </ol>
          <p className="mt-1 text-xs leading-relaxed text-slate-500 dark:text-zinc-400">
            同一账号可分别为 Trae Work 与 Trae 创建快照，两应用快照相互独立、可分别切换。
          </p>
          <p className="mt-1 text-xs font-medium text-amber-600 dark:text-amber-400">
            ⚠ 如果目标账号在所选应用下从未保存过登录态，切换会被中止并提示「无快照」。请先用「保存」图标在该应用下创建快照。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <Camera size={15} className="text-amber-500" /> 快照管理
          </h3>
          <p className="text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            点击「快照管理」按钮可查看所有已保存的登录态快照。每个快照以账号 UserID 命名，
            显示文件数、大小和最后修改时间。支持手动备份、恢复和删除操作。
            <code className="rounded bg-slate-100 px-1 dark:bg-zinc-800">last</code> 槽位是切换时自动创建的安全备份。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <RotateCcw size={15} className="text-amber-500" /> 重置设备 ID
          </h3>
          <p className="text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            点击账号行的「重置」图标可重置该账号的设备 ID（用于解决设备绑定问题）。
            如需全局重置 6 层设备标识（machineid、storage.json、aha、注册表 MachineGuid 等），
            请到设置页面执行「6 层设备标识重置」。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <Zap size={15} className="text-amber-500" /> 代理自动抓取账号
          </h3>
          <p className="text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            启动代理服务后，在 Trae 中登录任何账号，代理会自动捕获 JWT token 并保存到账号列表中。
            无需手动粘贴 JWT，适合批量导入账号。捕获的账号会自动解析 UserID、过期时间等信息。
          </p>
        </section>

        <section className="rounded-lg border border-sky-200 bg-sky-50 p-3 dark:border-sky-700 dark:bg-sky-900/20">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold text-sky-700 dark:text-sky-300">
            <KeyRound size={15} /> JWT 续期与刷新
          </h3>
          <p className="text-xs leading-relaxed text-sky-800 dark:text-sky-200">
            JWT 默认 13 天过期。带有刷新令牌的账号（显示闪电图标）可点击「刷新」自动续期。
            不支持自动刷新的账号请点击「续期」图标，系统会启动代理并切换到该账号，通过代理捕获新 JWT。
          </p>
        </section>
      </div>
    </Modal>
  );
}
