import { Globe, KeyRound, LogIn, CalendarCheck2, Coins, FolderCog, Tags, Server, ShieldAlert, RefreshCw } from 'lucide-react';
import { Modal } from '../../components/ui';

/**
 * BuddyAI 使用帮助弹框（对齐国内 BuddyHelpModal 形态；内容为国际版口径——
 * 打卡为每日活跃打卡 ACP 链、无切换/快照/CLI 桥体系）。
 */
export function BuddyAiHelpModal({ open, onClose }: { open: boolean; onClose: () => void }) {
  return (
    <Modal
      open={open}
      onClose={onClose}
      title="BuddyAI 账号管理使用帮助"
      size="xl"
      bodyClass="max-h-[66.7vh] overflow-y-auto pr-1"
    >
      <div className="space-y-4 text-sm">
        <section className="rounded-lg border border-amber-200 bg-amber-50 p-3 dark:border-amber-700 dark:bg-amber-900/20">
          <h3 className="mb-1.5 flex items-center gap-1.5 font-semibold text-amber-700 dark:text-amber-300">
            <Globe size={15} /> 添加账号流程（三步）
          </h3>
          <ol className="ml-4 list-decimal space-y-1 text-xs leading-relaxed text-amber-800 dark:text-amber-200">
            <li>
              {/* 审查修复（M9 同款）：原「扫码登录/导入凭证」均非本页按钮实名，且「手工粘贴 token」
                  机制不存在（导入仅接受账号池 JSON 文件），照帮助操作会找不到入口；
                  审查修复：docker 版已裁剪「扫描本机账号」入口，此处同步移除该表述 */}
              <b>添加账号</b>：点右上角「OAuth登录」走 workbuddy.ai 授权自动入池；或「导入账号」导入账号池
              JSON（凭证须为 .workbuddy.ai 域，国内账号会被区域校验拒绝）。
            </li>
            <li>
              <b>同步积分</b>：到「积分看板」点「刷新数据」拉取各账号余额与积分包明细。
            </li>
            <li>
              <b>开启调度</b>：到「资源调度」打开 buddy-ai 国际档，独立池参与统一网关路由（与国内档互不影响）。
            </li>
          </ol>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            {/* 审查修复（M9 同款）：章节标题与正文引用按钮实名「OAuth登录」（原「扫码登录」非实名） */}
            <LogIn size={15} className="text-amber-500" /> OAuth登录（扫码授权）
          </h3>
          <p className="text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            点击「OAuth登录」，应用自动打开浏览器完成 workbuddy.ai
            授权，成功后自动解析凭证并入池（凭证仅写入本地 token store，全程掩码不上传）。浏览器未自动打开时，弹框内可「复制完整链接」手动打开；超过
            5 分钟未收到结果会自动解除等待，最终结果以账号列表为准。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            {/* 审查修复（M9 同款）：章节标题与正文对齐按钮实名「导入账号」，并纠正机制描述
                （导入仅接受账号池 JSON 文件，非手工粘贴 token） */}
            <KeyRound size={15} className="text-amber-500" /> 导入账号 / 凭证续期
          </h3>
          <p className="text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            「导入账号」接受「导出账号」生成的账号池 JSON（access_token 必填、refresh_token
            选填——缺失则不可自动续期）。行内「续期」用 refreshToken 换新 accessToken；网关 401 / token
            临期时也会自动走 workbuddy.ai 刷新端点轮换（每账号刷新锁互斥，rt 轮换防覆盖）。带「需重新登录」标记的账号凭证已失效，续期无法恢复——需重新 OAuth 登录或导入新账号 JSON。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <CalendarCheck2 size={15} className="text-amber-500" /> 每日活跃打卡
          </h3>
          <p className="text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            国际版为「每日活跃打卡」：网页会话 + 沙箱跑完一轮指定模型的 agent
            对话，服务端在会话完成后自动入账 30/50 积分（按套餐定档，客户端不选档）。支持每日定时与手动触发，成功才落幂等戳——失败不占位，下轮巡检自动重试。
          </p>
          <p className="mt-1 text-xs font-medium text-amber-600 dark:text-amber-400">
            ⚠ 免费模型钉 deepseek-v4.1-flash 且打卡链路必发 set_model——请勿改动，改用计费模型会扣积分。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <Coins size={15} className="text-amber-500" /> 积分看板
          </h3>
          <p className="text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            「刷新数据」经 workbuddy.ai get-user-resource 实时取数：余额 / 积分包明细（分包剩余与到期）/ 90 天趋势 /
            Token 统计（网关记账 buddy-ai 档）/ 请求明细（官网请求级流水）。账号管理页积分单元格可点开查看该账号积分包明细。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <FolderCog size={15} className="text-amber-500" /> 分组与标签
          </h3>
          <p className="text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            「分组管理」可新建 / 重命名 / 换色 / 删除分组（删除后组内账号回落「未分组」）；「标签管理」为三池共享标签体系，行内
            chips 点击即可编辑账号标签。分组同时供「资源调度」页与 API Key 绑定使用。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <RefreshCw size={15} className="text-amber-500" /> 启停与批量操作
          </h3>
          <p className="text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            账号是否参与打卡花名册与网关取号由「资源调度」页账号池勾选控制（停用不影响凭证存储）；勾选多行可「批量移除」（逐项容错，失败项可重试）；「导出账号」默认脱敏，附带凭证副本需强确认——导出文件等同密码。
          </p>
        </section>

        <section className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold">
            <Server size={15} className="text-amber-500" /> 资源调度（国际档）
          </h3>
          <p className="text-xs leading-relaxed text-slate-600 dark:text-zinc-300">
            buddy-ai 为独立池：账号 / 模型冷却 / 用量记账 / 清洗模板全部独立，与国内 Buddy 档零数据共享。国际档默认关闭、显式开启制；模型目录在
            API 服务启动时直连官网 /v3/config 动态刷新，失败回落静态兜底。
          </p>
        </section>

        <section className="rounded-lg border border-rose-200 bg-rose-50 p-3 dark:border-rose-700 dark:bg-rose-900/20">
          <h3 className="mb-1 flex items-center gap-1.5 font-semibold text-rose-700 dark:text-rose-300">
            <ShieldAlert size={15} /> 安全红线
          </h3>
          <p className="text-xs leading-relaxed text-rose-800 dark:text-rose-200">
            凭证不明文出后端（导出清单默认脱敏）；登录链接等同登录态，请勿泄露；删除类操作均有二次确认。
          </p>
        </section>
      </div>
    </Modal>
  );
}
