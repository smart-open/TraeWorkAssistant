import { create } from 'zustand';
import { APP_NAME } from './lib/about';
import { sendNotification } from '@tauri-apps/plugin-notification';
import { api, setupListeners, type CheckinProgressEvent, type ProfileDoneEvent, type SaveLoginDoneEvent } from './lib/tauri';
import { withMinDelay } from './lib/delay';
import type {
  AccountView,
  ApiServiceStatus,
  CheckinAccountResult,
  CheckinDone,
  CreditRecord,
  CreditsDailySnapshot,
  EnvStatus,
  GroupView,
  AppEntitlement,
  TraeWorkMigrationReport,
  LogLine,
  ProfileInfo,
  ProxyStatus,
  QoderCheckinDone,
  QoderCheckinLine,
  Settings,
  ViewKey,
  AppKey,
} from './types';
import { APP_HOME_VIEW } from './types';

export type ToastKind = 'info' | 'success' | 'error' | 'warn';
export interface Toast {
  id: number;
  kind: ToastKind;
  msg: string;
}

export interface CheckinRetryInfo {
  /** 第几轮重试（1 起） */
  round: number;
  /** 本轮重试的失败账号数 */
  total: number;
  /** 本轮开始时刻（Date.now() 毫秒），用于倒计时展示 */
  until: number;
}

export interface CheckinState {
  active: boolean;
  total: number;
  index: number;
  results: CheckinAccountResult[];
  done: CheckinDone | null;
  /** 失败自动重试状态（非 null 时展示重试横幅/倒计时，T5） */
  retry: CheckinRetryInfo | null;
}

/** Qoder 签到进行态（疑点⑦ store 化：事件监听在 store 层注册，不随页面卸载——
 * 签到进行中切走页面再切回，running/进度/汇总完好；调度器后台触发亦不丢事件） */
export interface QoderCheckinState {
  running: boolean;
  lines: QoderCheckinLine[];
  done: QoderCheckinDone | null;
  /** done 事件递增计数：页面据此联动刷新账号列表/签到记录 */
  doneRev: number;
}

export interface LogQuery {
  logType?: string;
  date?: string;
  keyword?: string;
  limit?: number;
}

interface AppState {
  ready: boolean;
  /** 运行平台（F-75 M0-0.5）：init 时经 platform_info 拉取，控制 Windows 专属入口显隐与文案 */
  platform: 'windows' | 'macos';
  view: ViewKey;
  /** 侧边栏应用切换（trae = 现有菜单；buddy = 后期扩展置灰；doubao = 豆包页） */
  activeApp: AppKey;
  env: EnvStatus | null;
  certInstalled: boolean;
  proxy: ProxyStatus;
  apiStatus: ApiServiceStatus | null;
  accounts: AccountView[];
  groups: GroupView[];
  settings: Settings | null;
  /** F3：settings_get 是否成功加载过。false 时 store.settings 只是默认值兜底，
   *  saveSettings 拒绝写入，防止用默认值整表覆盖服务端真实配置 */
  settingsLoaded: boolean;
  logs: LogLine[];
  creditsHistory: CreditRecord[];
  creditsDaily: CreditsDailySnapshot[];
  proxyLog: string[];
  switchProgress: string[];
  switchingTo: string | null;
  /** issue #126 续期在途标记：记录发起续期的 uid 与切换前 jwt_exp_hours，
      switch-done 后对比有效期得出「捕获成功/空手」结论（null=非续期切换） */
  renewPending: { uid: string; expBefore: number | null } | null;
  saveLoginProgress: string[];
  savingLogin: string | null;
  deviceResetProgress: string[];
  deviceResetActive: boolean;
  checkin: CheckinState;
  /** Qoder 签到进行态（疑点⑦：store 单例持有，页面切走不丢事件） */
  qoderCheckin: QoderCheckinState;
  toasts: Toast[];
  profiles: ProfileInfo[];
  profileProgress: string[];
  profileActive: boolean;
  /** 本机 Trae 客户端当前登录账号的套餐信息（storage.json 明文，零 API） */
  localEntitlement: AppEntitlement | null;
  /** 全局 API 管理弹窗（unified-api-gateway-design §5.2；任意 activeApp 视图均可打开） */
  showApiManager: boolean;
  /** Trae 双应用合并升级引导弹窗（首启有迁移报告时弹出，关闭落标记后不再展示） */
  traeworkGuideOpen: boolean;
  traeworkGuide: TraeWorkMigrationReport | null;

  init: () => Promise<void>;
  setView: (v: ViewKey) => void;
  /** 切换侧边栏应用 Tab，并跳到该应用默认首页 */
  setActiveApp: (app: AppKey) => void;
  applyCheckinEvent: (e: CheckinProgressEvent) => void;
  /** Qoder 签到 NDJSON 进度归约（start/done/逐账号行/exit；skipped_busy 与
   * empty 计数口径对齐 Rust 侧） */
  applyQoderCheckinLine: (line: string) => void;
  /** 发起 Qoder 签到（连点自守；失败置回 running 并 toast） */
  startQoderCheckin: () => Promise<void>;

  refreshEnv: () => Promise<void>;
  refreshCert: () => Promise<void>;
  refreshProxy: () => Promise<void>;
  refreshApiStatus: () => Promise<void>;
  refreshAccounts: () => Promise<void>;
  refreshGroups: () => Promise<void>;
  refreshSettings: () => Promise<void>;
  refreshLogs: (q?: LogQuery, manual?: boolean) => Promise<void>;
  refreshCreditsHistory: () => Promise<void>;
  refreshCreditsDaily: () => Promise<void>;
  refreshProfiles: () => Promise<void>;
  refreshLocalEntitlement: () => Promise<void>;
  setShowApiManager: (v: boolean) => void;
  /** 关闭升级引导：立即收起弹窗并落「已读」标记；标记写失败以 toast 明示（下次启动会再弹） */
  dismissTraeworkGuide: () => Promise<void>;

  startProxy: () => Promise<void>;
  stopProxy: () => Promise<void>;
  openTrae: () => Promise<void>;
  addAccount: (name: string, jwt: string, groupId?: string) => Promise<void>;
  deleteAccount: (userId: string, deleteProfile: boolean) => Promise<void>;
  updateAccount: (userId: string, name?: string, jwt?: string) => Promise<void>;
  createGroup: (name: string, color: string) => Promise<void>;
  updateGroup: (
    id: string,
    patch: { name?: string; color?: string; order?: number },
  ) => Promise<void>;
  removeGroup: (id: string) => Promise<void>;
  moveAccount: (userId: string, groupId: string | null) => Promise<void>;
  resetDevice: (userId: string) => Promise<void>;
  switchTo: (userId: string, targetApp?: 'Trae' | 'Doubao' | 'WorkBuddy' | 'CodeBuddy' | 'Qoder' | 'QoderWork') => Promise<void>;
  /** issue #44：看门狗超时后清空切换/保存进行中状态——解除 spinner/「切换中…」永挂；
      迟到的 done 事件仍会正常提示结果（onSwitchDone 对 null 幂等） */
  clearSwitchLocks: () => void;
  /** C1：一键以账号 X 打开豆包（恢复快照后拉起客户端；代理运行中时注入代理） */
  openDoubaoAs: (userId: string, proxyPort?: number) => Promise<void>;
  saveCurrentLogin: (userId: string, targetApp?: 'Trae' | 'Doubao' | 'WorkBuddy' | 'CodeBuddy' | 'Qoder' | 'QoderWork') => Promise<void>;
  renewJwt: (userId: string) => Promise<void>;
  resetDeviceIds: (targetApp?: 'Trae') => Promise<void>;
  startCheckin: (opts: {
    scope: string;
    user_ids?: string[];
    skip_checked_in: boolean;
    skip_expired: boolean;
  }) => Promise<void>;
  refreshRemainingCredits: () => Promise<void>;
  cooldownClear: (userId: string) => Promise<void>;
  refreshJwt: (userId: string, force?: boolean) => Promise<void>;
  saveSettings: (patch: Partial<Settings>) => Promise<void>;
  profileBackup: (userId: string) => Promise<void>;
  profileRestore: (userId: string) => Promise<void>;
  profileDelete: (userId: string) => Promise<void>;
  oauthLogin: (
    callbackUrl: string,
    accountName?: string,
    groupId?: string,
    expectUid?: string,
  ) => Promise<void>;

  pushToast: (kind: ToastKind, msg: string) => void;
  dismissToast: (id: number) => void;
}

let toastSeq = 0;
// 已注册的事件监听取消函数；StrictMode 下 init 会执行两次，靠它先注销旧监听避免重复注册
let unsubs: Array<() => void> = [];
/** init 幂等锁：StrictMode 双跑（dev）时第二次调用直接返回，防并发双注册监听 */
let initStarted = false;

function defaultSettings(): Settings {
  return {
    proxy_port: 8899,
    theme: 'system',
    launch_minimized: false,
    silent_checkin: false,
    /** issue #82 T6.1：会话明细概述显示开关（默认开） */
    sessions_preview_enabled: true,
    auto_start_proxy: true,
    auto_start_api: false,
    tray: true,
    language: 'zh-CN',
    checkin_skip_checked: true,
    checkin_skip_expired: true,
    retry: 1,
    notify: 'toast',
    trae_cn_path: null,
    doubao_path: null,
    doubao_renew_url: 'https://www.doubao.com/info/v2/',
    doubao_quota_url: 'https://www.doubao.com/alice/commerce/sale/subscription/quota/summary/',
    doubao_snapshot_include_idb: false,
    // issue #147：豆包应用内调度开关默认开
    doubao_keepalive_enabled: true,
    doubao_quota_enabled: true,
    workbuddy_path: null,
    codebuddy_path: null,
    wb_auth_file_path: null,
    data_dir: null,
    log_retention_days: 30,
    proxy_domains: 'trae.cn,trae.com.cn,mchost.guru,zijieapi.com,bytedance.com,volcengine.com,volces.com,treecode.com,doubao.com,qoder.com.cn',
    proxy_log_path: null,
    api_port: 7864,
    api_default_model: 'deepseek-v4-flash',
    // F-74：切换时自动迁移会话——默认关（旧行为保持"只切登录态，不写会话"）
    buddy_switch_migrate_chats: false,
    // Trae 每日签到调度时刻（环境配置页可改，默认 09:00；schtasks 注册时间复用该值）
    trae_checkin_hhmm: '09:00',
    // 签到多账号间隔秒（默认 3s，防上游频控；0=关闭；Trae/Buddy/Qoder 各自环境配置页可改，Buddy 签到与成长共用）
    trae_checkin_gap_secs: 3,
    wb_checkin_gap_secs: 3,
    qoder_checkin_gap_secs: 3,
    // Trae JWT 定时续期（issue #27）：默认开，每日 09:00 惰性续期临期账号
    jwt_renew_enabled: true,
    jwt_renew_hhmm: '09:00',
    // WorkBuddy 每日成长（任务配置页）：默认开，每日 09:00 执行成长轮
    wb_growth_enabled: true,
    wb_growth_hhmm: '09:00',
    // WorkBuddy 每日签到调度时刻（任务配置页可改，默认 09:10）
    wb_checkin_hhmm: '09:10',
    // 积分/Token 同步：Buddy 与 Trae 默认每 2 小时（issue #82 会话明细定时落盘），
    // 间隔可在环境配置页手工设置；daily 模式时刻 23:30 / 23:40；模型同步默认开（05:45 / 05:40）
    wb_credits_sync_mode: 'every_hours',
    wb_credits_sync_hhmm: '23:30',
    wb_credits_sync_hours: '2',
    trae_credits_sync_mode: 'every_hours',
    trae_credits_sync_hhmm: '23:40',
    trae_credits_sync_hours: '2',
    wb_catalog_sync_enabled: true,
    wb_catalog_sync_hhmm: '05:45',
    qoder_catalog_sync_enabled: true,
    qoder_catalog_sync_hhmm: '05:50',
    trae_models_sync_enabled: true,
    trae_models_sync_hhmm: '05:40',
    // 通知渠道（F-19，Trae/Buddy 全平台共用）：总开关与事件通知默认开，渠道留空 = 关闭
    notify_enabled: true,
    notify_on_checkin: true,
    notify_on_task_fail: true,
    notify_bark_url: null,
    notify_webhook_url: null,
    notify_serverchan_sendkey: null,
    notify_pushplus_token: null,
    notify_pushplus_template: null,
    notify_pushplus_topic: null,
    // 侧边栏应用显示：默认固定 Trae，其余应用全部显示；
    // BuddyAI（buddy-ai 国际版）默认不展示（独立 opt-in 开关，见应用显示面板）
    pinned_app: 'trae',
    hidden_apps: [],
    app_icons: {},
    // 侧边栏应用显示顺序：空 = 内置默认顺序（APP_TABS 声明序），拖拽排序后写入
    app_order: [],
    buddy_ai_visible: false,
    // buddy-ai 每日活跃打卡（阶段 3b）：调度开关默认关（显式开启制）、时刻 09:20、间隔 3s
    buddy_ai_checkin_enabled: false,
    buddy_ai_checkin_hhmm: '09:20',
    buddy_ai_checkin_gap_secs: 3,
    // buddy-ai 定时同步/续期（2026-10-16 六页对齐批 3）：默认开，时刻错峰
    buddy_ai_renew_enabled: true,
    buddy_ai_renew_hhmm: '10:35',
    buddy_ai_credits_sync_enabled: true,
    buddy_ai_credits_sync_hhmm: '23:45',
    buddy_ai_credits_sync_mode: 'daily',
    buddy_ai_credits_sync_hours: '2',
    buddy_ai_credits_sync_days: '1',
    buddy_ai_catalog_sync_enabled: true,
    buddy_ai_catalog_sync_hhmm: '05:55',
    // BuddyAI 国际客户端手动路径（环境配置页；null = 自动检测）
    workbuddy_ai_path: null,
    codebuddy_ai_path: null,
    // Qoder（F-80）：客户端路径与调度时刻/开关（默认 10:15 签到）；
    // 积分数据同步与 Buddy/Trae 同款四模式（默认 every_hours 每2小时，daily 时刻 23:40）
    qoder_ide_path: null,
    qoderwork_path: null,
    qoder_checkin_hhmm: '10:15',
    qoder_credits_sync_hhmm: '23:40',
    qoder_credits_sync_enabled: true,
    qoder_credits_sync_mode: 'every_hours',
    qoder_credits_sync_hours: '2',
    qoder_token_renew_enabled: true,
    qoder_token_renew_interval_hours: 6,
    qoder_sessions_compact_enabled: true,
    qoder_sessions_compact_days: '3',
    qoder_sessions_compact_hhmm: '04:30',
  };
}

// 日志轮询去重：上一轮未返回时跳过本轮（防 2s 轮询堆积与旧响应乱序覆盖）
let logsPollInflight = false;
// 日志查询并发序号（最新请求胜出）：手动刷新与轮询并发时，旧响应直接丢弃
let logsReqSeq = 0;
// 账号列表并发序号（最新请求胜出，审查修复 M2）：refreshAccounts 被捕获事件/
// 签到完成/页面挂载等多源高频并发触发，先发后至的旧列表不得覆盖新数据
let accountsReqSeq = 0;
// 同因 toast 限频：读取持续失败期间每 60s 最多弹一次（防 toast 风暴）；手动调用直通
let lastLogsErrToastAt = 0;
// F5：签到看门狗定时器（模块级，store 单例持有）
let checkinWatchdog: ReturnType<typeof setTimeout> | null = null;
/** F5：签到看门狗时长——覆盖多账号 + 2 轮失败重试的最坏耗时 */
const CHECKIN_WATCHDOG_MS = 15 * 60 * 1000;

function disarmCheckinWatchdog() {
  if (checkinWatchdog) {
    clearTimeout(checkinWatchdog);
    checkinWatchdog = null;
  }
}

/** F5：武装/续期签到看门狗。事件通道丢帧时 done 事件可能永不到达，
 *  checkin.active 只能由 done/exit 复位 → 「签到进行中」永久卡死、开始按钮永久禁用。
 *  任一签到事件到达即续期；超时自动解除 active 并提示（迟到的事件仍会正常归约展示）。 */
function armCheckinWatchdog() {
  disarmCheckinWatchdog();
  checkinWatchdog = setTimeout(() => {
    checkinWatchdog = null;
    const s = useAppStore.getState();
    if (!s.checkin.active) return;
    s.pushToast(
      'warn',
      '签到超过 15 分钟未收到任何进度事件（事件通道可能异常），已自动解除「签到进行中」状态；结果请以账号列表与签到记录为准',
    );
    useAppStore.setState((st) => ({ checkin: { ...st.checkin, active: false, retry: null } }));
  }, CHECKIN_WATCHDOG_MS);
}

/** BuddyAI Tab 可见性「用户已显式决定」标记（审查修复 #10，localStorage）：
 *  store init 的一次性自动放开与设置面板手动开关共用——标记落盘后自动逻辑永久退避 */
export const BUDDY_AI_VIS_DECIDED_KEY = 'buddy_ai_vis_decided';

export const useAppStore = create<AppState>((set, get) => ({
  ready: false,
  // 初始兜底用 UA 猜测（mac = "Macintosh; Intel Mac OS X"），platform_info 成功后覆盖
  platform: /Mac/i.test(navigator.userAgent) ? 'macos' : 'windows',
  view: 'dashboard',
  activeApp: 'trae',
  env: null,
  certInstalled: false,
  proxy: { running: false, port: 0, captured: 0, started_at: null },
  apiStatus: null,
  accounts: [],
  groups: [],
  settings: null,
  settingsLoaded: false,
  logs: [],
  creditsHistory: [],
  creditsDaily: [],
  proxyLog: [],
  switchProgress: [],
  switchingTo: null,
  renewPending: null,
  saveLoginProgress: [],
  savingLogin: null,
  deviceResetProgress: [],
  deviceResetActive: false,
  checkin: { active: false, total: 0, index: 0, results: [], done: null, retry: null },
  qoderCheckin: { running: false, lines: [], done: null, doneRev: 0 },
  toasts: [],
  profiles: [],
  profileProgress: [],
  profileActive: false,
  localEntitlement: null,
  showApiManager: false,
  traeworkGuideOpen: false,
  traeworkGuide: null,

  init: async () => {
    // StrictMode 下 effect 会执行两次（dev）：两次 init 同步并发启动，旧的「先注销旧监听」
    // 防护在 setupListeners resolve 前执行时注销不到任何东西，两组监听都会注册成功，
    // 且后者覆盖 unsubs → 第一组永久泄漏、事件双发（proxy-log 重复、captured +2）。
    // 幂等锁：仅首次执行注册，后续调用直接复用（审查修复 P1-16）
    if (initStarted) return;
    initStarted = true;
    // F-75 M0-0.5：平台标志先行拉取（失败保持默认 windows，不阻塞启动）
    try {
      const p = await api.platform.info();
      set({ platform: p.os });
    } catch {
      /* ignore：保持默认 */
    }
    unsubs = await setupListeners({
      onProxyLog: (line) =>
        set((s) => ({ proxyLog: [line, ...s.proxyLog.slice(0, 199)] })),
      onAccountCaptured: (uid) => {
        // 事件驱动累加捕获数（后端 Arc<AtomicI64> 的实时镜像，避免轮询）
        set((s) => ({ proxy: { ...s.proxy, captured: s.proxy.captured + 1 } }));
        get().pushToast('success', `已捕获账号 ${uid}`);
        void get().refreshAccounts();
      },
      onCheckinProgress: (e) => get().applyCheckinEvent(e),
      onQoderCheckinProgress: (line) => get().applyQoderCheckinLine(line),
      onSwitchProgress: (line) =>
        set((s) => ({ switchProgress: [...s.switchProgress.slice(-49), line] })),
      // D2：订阅后端 switch-done，给用户明确的切换完成/失败信号
      onSwitchDone: (e) => {
        // 提取脚本 [fatal] 行的原文（如「目标账号 xxx 无快照，请先登录该账号并点击保存当前登录态」）
        // raw 兜底空串：后端偶发缺 payload 时避免 TypeError 把 switchingTo 永久锁死
        const raw = e.raw ?? '';
        const reason = e.success ? null : (raw.match(/\[fatal\]\s*(.+)$/)?.[1]?.trim() ?? null);
        // verify 超时（authfile 布局）：切换动作完成但登录身份未确认 → warn 而非 success，
        // 避免「切换成功」toast 掩盖客户端未登录的事实（switcher.log 实测 4/4 超时）
        const warnUnconfirmed = e.success && /未确认登录身份/.test(raw);
        const doneLine = warnUnconfirmed
          ? '[警告] 已切换，但 30 秒内未确认登录身份，请打开客户端核实'
          : e.success
            ? '[完成] 登录态切换成功'
            : `[失败] ${reason ?? '登录态切换未完成，请查看日志'}`;
        // issue #126：续期流程（renewPending 命中本次切换）以 JWT 有效期对比结论
        // 替换通用成功 toast——捕获空手时诚实说明原因与出路，不再静默「没效果」；
        // 失败/身份未确认仍走通用提示
        const doneUid = get().switchingTo;
        const renewing = get().renewPending;
        const isRenewal = !!renewing && renewing.uid === doneUid;
        const renewalConcluded = isRenewal && e.success && !warnUnconfirmed;
        if (isRenewal) set({ renewPending: null });
        set((s) => ({
          switchingTo: null,
          switchProgress: [...s.switchProgress.slice(-49), doneLine],
        }));
        if (renewalConcluded) {
          void (async () => {
            await get().refreshAccounts();
            const after = get().accounts.find((a) => a.user_id === doneUid);
            const expAfter = after?.jwt_exp_hours ?? null;
            const before = renewing.expBefore;
            // 新 JWT 寿命约 13 天（~312h）：剩余有效期较切换前显著增长才算捕获成功
            const renewed =
              expAfter != null && (before == null ? expAfter > 24 : expAfter > before + 24);
            if (renewed) {
              const line = `[完成] 续期成功：新 JWT 剩余 ${expAfter.toFixed(0)} 小时`;
              set((s) => ({ switchProgress: [...s.switchProgress.slice(-49), line] }));
              get().pushToast('success', `续期成功：新 JWT 剩余 ${expAfter.toFixed(0)} 小时`);
            } else {
              const guide = after?.has_refresh_token
                ? '本账号有 refresh_token：请使用「刷新 JWT」按钮直接换发新凭证'
                : '本账号无 refresh_token：请保持代理开启与 TRAE 运行，JWT 过期后 TRAE 会自动重新登录并自动捕获；或重新 OAuth 登录';
              const line = `[警告] 未捕获到新 JWT（有效期未变化）。${guide}`;
              set((s) => ({ switchProgress: [...s.switchProgress.slice(-49), line] }));
              get().pushToast(
                'info',
                `切换完成，但未捕获到新 JWT——TRAE 凭证仍有效时不会重新鉴权。${guide}`,
              );
            }
          })();
        } else {
          get().pushToast(
            warnUnconfirmed ? 'warn' : e.success ? 'success' : 'error',
            warnUnconfirmed
              ? '已执行切换，但 30 秒内未确认登录身份——请打开客户端核实；若未登录，请重新登录后「保存当前登录态」'
              : e.success
                ? '登录态切换完成'
                : `切换失败：${reason ?? '请查看系统日志'}`,
          );
          void get().refreshAccounts();
        }
        void get().refreshProxy();
        // 登录中徽标（localEntitlement）刷新：切换完成后本机登录 uid 已变，
        // 立即刷新一次；客户端启动写入本机使用证据需数秒，延迟再刷一次兜底
        void get().refreshLocalEntitlement();
        setTimeout(() => void get().refreshLocalEntitlement(), 8000);
      },
      onSaveLoginProgress: (line) =>
        set((s) => ({ saveLoginProgress: [...s.saveLoginProgress.slice(-49), line] })),
      onSaveLoginDone: (e: SaveLoginDoneEvent) => {
        // 同上：raw 缺失时兜底空串，避免 savingLogin 被锁死
        const reason = e.success ? null : ((e.raw ?? '').match(/\[fatal\]\s*(.+)$/)?.[1]?.trim() ?? null);
        set((s) => ({
          savingLogin: null,
          saveLoginProgress: [
            ...s.saveLoginProgress.slice(-49),
            e.success ? '[完成] 登录态保存成功' : `[失败] ${reason ?? '登录态保存失败，请查看日志'}`,
          ],
        }));
        get().pushToast(
          e.success ? 'success' : 'error',
          e.success ? '登录态已保存，可随时切换回此账号' : `保存失败：${reason ?? '请查看系统日志'}`,
        );
        void get().refreshProfiles();
      },
      onDeviceResetProgress: (line) =>
        set((s) => ({ deviceResetProgress: [...s.deviceResetProgress.slice(-99), line] })),
      onDeviceResetDone: (e) => {
        // 层级数按平台分派（mac 5 层 / Windows 6 层，与 Settings.tsx 文案同口径）
        const layers = get().platform === 'macos' ? '5 层' : '6 层';
        set((s) => ({
          deviceResetActive: false,
          deviceResetProgress: [
            ...s.deviceResetProgress.slice(-99),
            e.success ? `[完成] ${layers}设备标识重置成功` : '[失败] 设备标识重置未完成，请查看日志',
          ],
        }));
        get().pushToast(
          e.success ? 'success' : 'error',
          e.success ? `${layers}设备标识重置完成` : '设备标识重置失败，请查看日志',
        );
      },
      onProfileProgress: (line) =>
        set((s) => ({ profileProgress: [...s.profileProgress.slice(-49), line] })),
      onProfileDone: (e: ProfileDoneEvent) => {
        set((s) => ({
          profileActive: false,
          profileProgress: [
            ...s.profileProgress.slice(-49),
            e.success ? `[完成] ${e.action === 'backup' ? '备份' : '恢复'}成功` : `[失败] ${e.action === 'backup' ? '备份' : '恢复'}失败`,
          ],
        }));
        get().pushToast(
          e.success ? 'success' : 'error',
          e.success
            ? `登录态${e.action === 'backup' ? '备份' : '恢复'}完成`
            : `登录态${e.action === 'backup' ? '备份' : '恢复'}失败`,
        );
        void get().refreshProfiles();
      },
    }).catch((e) => {
      // 审查修复 M3：监听注册失败时复位幂等锁，允许重试——否则 initStarted
      // 恒为 true，任何重试调用直接 return，ready 永远 false，应用卡死在未就绪态
      initStarted = false;
      throw e;
    });
    await Promise.all([
      get().refreshEnv(),
      get().refreshCert(),
      get().refreshProxy(),
      get().refreshApiStatus(),
      get().refreshAccounts(),
      get().refreshGroups(),
      get().refreshSettings(),
      get().refreshCreditsHistory(),
      get().refreshCreditsDaily(),
      get().refreshProfiles(),
      get().refreshLocalEntitlement(),
    ]);
    set({ ready: true });

    // 启动时根据设置自动开启代理
    const s = get();
    if (!s.proxy.running && s.settings?.auto_start_proxy) {
      void s.startProxy();
    }
    // 启动时根据设置自动开启 API 网关（复刻 auto_start_proxy 模式）：
    // Rust 侧 do_start 对已运行实例返回 Err，此处判重 + 静默兜底，避免与托盘启停竞态时误报
    if (!s.apiStatus?.running && s.settings?.auto_start_api) {
      try {
        const status = await api.apiServer.start();
        set({ apiStatus: status });
      } catch {
        /* 静默失败：状态栏保持未启动，用户可手动启动 */
      }
    }
    // 待取启动通知（一次性）：后端启动期消息（如分组统一迁移结果）toast 告知用户，
    // 拉取即清不会重复弹出；失败静默（通知不阻塞启动）
    try {
      const notice = await api.misc.startupNoticeTake();
      if (notice) get().pushToast('info', notice);
    } catch {
      /* 静默失败 */
    }
    // Trae 双应用合并升级引导：有迁移报告且未关闭过 → 弹窗展示迁移流程与结果；
    // 关闭时后端落「已读」标记，之后启动不再展示。失败静默（引导不阻塞启动）
    try {
      const report = await api.misc.traeworkMigrationGuide();
      if (report) set({ traeworkGuide: report, traeworkGuideOpen: true });
    } catch {
      /* 静默失败 */
    }
    // BuddyAI Tab 一次性自动放开（审查修复 #10）：buddy_ai_visible 此前仅能手动勾选，
    // 装了国际客户端的用户默认看不到入口。首次使用（localStorage 无 decided 标记，
    // 视为用户从未显式决定过）时经现有 env locate 探测国际客户端（workbuddy_ai /
    // codebuddy_ai，与 BuddyAiTopBar 同款调用），任一安装即自动放开显示；未安装仅落
    // 标记（本机只探测一次，避免每次启动重复探测开销）。用户在设置面板显式切换过开关
    // 同样落标记（onChange 写入），自动逻辑此后永久退避、不与用户显式选择打架。
    // 失败静默，不阻塞启动
    void (async () => {
      try {
        if (localStorage.getItem(BUDDY_AI_VIS_DECIDED_KEY)) return;
        if (!get().settingsLoaded) return;
        const [wb, cb] = await Promise.all([
          api.env.locate('workbuddy_ai').catch(() => null),
          api.env.locate('codebuddy_ai').catch(() => null),
        ]);
        // 探测期间用户可能在设置面板显式开启（onChange 已落标记）：以其选择为准不覆盖
        if (get().settings?.buddy_ai_visible === true) {
          localStorage.setItem(BUDDY_AI_VIS_DECIDED_KEY, '1');
          return;
        }
        // 探测完成即落标记：无论是否安装，本机只自动探测一次
        localStorage.setItem(BUDDY_AI_VIS_DECIDED_KEY, '1');
        if (wb?.exe || cb?.exe) {
          await get().saveSettings({ buddy_ai_visible: true }).catch(() => {});
        }
      } catch {
        /* 静默失败：下次启动满足条件时重试 */
      }
    })();
  },

  setView: (v) => set({ view: v }),
  setActiveApp: (app) => set({ activeApp: app, view: APP_HOME_VIEW[app] }),
  setShowApiManager: (v) => set({ showApiManager: v }),
  dismissTraeworkGuide: async () => {
    // 先收起弹窗再落标记：标记写失败（下次启动会再弹）不静默，toast 明示
    set({ traeworkGuideOpen: false });
    try {
      await api.misc.traeworkMigrationGuideDismiss();
    } catch (e) {
      get().pushToast('error', `保存升级引导关闭标记失败，下次启动可能再次弹出: ${String(e)}`);
    }
  },

  applyCheckinEvent: (e) => {
    // F5：done 解除看门狗，其余事件（start/retry/account）到达即续期
    if (e.type === 'done') disarmCheckinWatchdog();
    else armCheckinWatchdog();
    set((s) => {
      if (e.type === 'start') {
        // Rust 侧 start 带 scope 内全集清单（候选 pending / 跳过带原因 / 重试轮沿用上轮状态），
        // 重建列表使被跳过的账号也可见；Python 转发的 start（无 accounts）仅同步候选总数
        if (e.accounts) {
          return {
            checkin: {
              active: true,
              total: e.total,
              index: 0,
              results: e.accounts.map((a, i) => ({
                index: i + 1,
                user_id: a.user_id,
                name: a.name,
                status: a.status,
                skip_reason: a.skip_reason ?? null,
              })),
              done: null,
              retry: s.checkin.retry,
            },
          };
        }
        // Python 转发的 start（无 accounts）同样重置进度与旧结果，避免上一轮残留干扰本轮展示
        return { checkin: { ...s.checkin, active: true, total: e.total, index: 0, results: [], done: null } };
      }
      if (e.type === 'retry') {
        return {
          checkin: {
            ...s.checkin,
            active: true,
            retry: { round: e.round, total: e.total, until: Date.now() + e.delay * 1000 },
          },
        };
      }
      if (e.type === 'account') {
        const results = s.checkin.results.slice();
        // 按 user_id 匹配行：事件 index 是本轮候选内的序号，与全集列表位置无关
        const i = results.findIndex((r) => r.user_id === e.user_id);
        const row = {
          index: i >= 0 ? results[i].index : results.length + 1,
          user_id: e.user_id,
          name: e.name,
          status: e.status,
          skip_reason: null,
          credits: e.credits,
          delta: e.delta,
          elapsed: e.elapsed,
          code: e.code,
          message: e.message,
          error_type: e.error_type,
          cooldown_until: e.cooldown_until,
        };
        if (i >= 0) results[i] = row;
        else results.push(row);
        // index 累计本轮已处理候选数，驱动进度条（total 口径=本轮候选数）
        return { checkin: { ...s.checkin, index: s.checkin.index + 1, results } };
      }
      return {
        checkin: {
          ...s.checkin,
          active: false,
          retry: null,
          done: { ok: e.ok, already: e.already, failed: e.failed, total: e.total },
        },
      };
    });
    if (e.type === 'done') {
      void get().refreshAccounts();
      // JWT 吊销类失败的精确提示（issue #9）：401=服务端已吊销 JWT，重新登录+保存即可恢复，
      // 不再让用户面对笼统的「失败 N」自己摸索原因
      const deadCount = get().checkin.results.filter(
        (r) => r?.status === 'fail' && r.error_type === 'SessionDead',
      ).length;
      // 服务端拒绝认证类失败的精确提示（issue #128）：凭证级认证拒绝与版本升级无关，
      // 自动重试无法恢复，给出临时方案引导
      const authRejectedCount = get().checkin.results.filter(
        (r) => r?.status === 'fail' && r.error_type === 'AuthRejected',
      ).length;
      // 签到完成后静默刷新剩余积分（内部会再次 refreshAccounts）
      void api.accounts.refreshRemainingCredits().then(() => {
        get().refreshAccounts();
        get().refreshCreditsDaily();
      }).catch(() => {});
      get().pushToast(
        e.failed > 0 ? 'warn' : 'success',
        // 空轮次：过滤后无候选（全部已签/过期/冷却中），给用户明确文案而非「成功 0 已签 0 失败 0」
        e.empty
          ? '没有需要签到的账号（全部已签/过期/冷却中）'
          : `签到完成：成功 ${e.ok}，已签 ${e.already}，失败 ${e.failed}`,
      );
      if (deadCount > 0) {
        get().pushToast(
          'error',
          `${deadCount} 个账号 JWT 已被服务端吊销（该账号在别处重新登录/IDE 内退出过登录）：请在 TRAE 中重新登录该账号并「保存当前登录态」，再点「续期 JWT」重新捕获`,
        );
      }
      if (authRejectedCount > 0) {
        get().pushToast(
          'error',
          `${authRejectedCount} 个账号凭证被 Trae 服务端拒绝认证（服务端认证失败，与版本升级无关，自动重试无法恢复）：请在账号管理对该账号重新执行一次 OAuth 登录即可恢复签到`,
        );
      }
    }
  },

  applyQoderCheckinLine: (line) => {
    let parsed: unknown;
    try {
      parsed = JSON.parse(line);
    } catch {
      return;
    }
    if (!parsed || typeof parsed !== 'object') return;
    const ev = parsed as Record<string, unknown>;
    if (ev.type === 'start') {
      // 调度器后台触发时页面不在场也能正确进入 running 态（store 层归约的收益）
      set((s) => ({ qoderCheckin: { ...s.qoderCheckin, running: true, lines: [], done: null } }));
      return;
    }
    if (ev.type === 'done') {
      // 跨进程锁被占用（schtasks/CLI 与应用内调度同刻触发）：整轮幂等跳过，
      // 不误报「成功完成」、不落全 0 的 done 徽标（调度器侧同款不记当日已跑）
      if (ev.skipped_busy === true) {
        set((s) => ({ qoderCheckin: { ...s.qoderCheckin, running: false, done: null } }));
        get().pushToast('warn', '另一进程正在执行 Qoder 签到，本轮已跳过（调度稍后会自动重试）');
        return;
      }
      const num = (v: unknown) => (typeof v === 'number' && isFinite(v) ? v : 0);
      // failed_empty_campaigns（活动未开始/不可用）为非用户可操作失败：与真实
      // 失败分开计数，避免用户对无解失败反复重试（口径对齐 Rust 侧补签通知）
      const empty = Math.max(0, num(ev.failed_empty_campaigns));
      const failed = num(ev.failed);
      const done: QoderCheckinDone = {
        ok: num(ev.ok),
        already: num(ev.already),
        failed,
        empty,
      };
      set((s) => ({
        qoderCheckin: { ...s.qoderCheckin, running: false, done, doneRev: s.qoderCheckin.doneRev + 1 },
      }));
      // 审查 Low：消费后端 failed_permanent（凭证终态失败，重试注定无效）——
      // 可重试数 = failed - empty - permanent（口径对齐调度器 retryable 判定），
      // 文案明示其中哪些需重新登录/更新 PAT，防用户对终态失败反复手点重试；
      // 级别：含终态失败同样 warn（需要用户行动，不降级 success）
      const permanent = Math.max(0, Math.min(num(ev.failed_permanent), failed - empty));
      const retryable = failed - empty - permanent;
      get().pushToast(
        retryable > 0 || permanent > 0 ? 'warn' : 'success',
        `Qoder 签到完成：成功 ${done.ok}，已签 ${done.already}，失败 ${done.failed}` +
          (empty > 0 ? `（其中 ${empty} 项为活动未开放）` : '') +
          (permanent > 0 ? `（其中 ${permanent} 项需重新登录/更新 PAT 后重试）` : ''),
      );
      return;
    }
    if (ev.type === 'exit') {
      set((s) => ({ qoderCheckin: { ...s.qoderCheckin, running: false } }));
      return;
    }
    if (typeof ev.index === 'number' && ev.index > 0) {
      const i = ev.index;
      // reward 数值归一（后端偶发字符串形态；NaN/缺失归 undefined）
      const raw = ev.reward;
      const reward =
        typeof raw === 'number' && isFinite(raw)
          ? raw
          : typeof raw === 'string' && raw.trim() !== '' && !isNaN(Number(raw))
            ? Number(raw)
            : undefined;
      set((s) => {
        const lines = s.qoderCheckin.lines.slice();
        lines[i - 1] = {
          index: i,
          user_id: String(ev.user_id ?? ''),
          name: String(ev.name ?? ''),
          status: (ev.status as QoderCheckinLine['status']) ?? 'fail',
          message: ev.message != null ? String(ev.message) : undefined,
          reward,
        };
        return { qoderCheckin: { ...s.qoderCheckin, lines } };
      });
    }
  },

  startQoderCheckin: async () => {
    if (get().qoderCheckin.running) return; // 连点自守（后端轮次锁仍兜底）
    set((s) => ({ qoderCheckin: { ...s.qoderCheckin, running: true, lines: [], done: null } }));
    try {
      await api.qoder.checkinStart({ skip_checked_in: true });
    } catch (err) {
      set((s) => ({ qoderCheckin: { ...s.qoderCheckin, running: false } }));
      get().pushToast('error', `发起签到失败：${String(err)}`);
    }
  },

  refreshEnv: async () => {
    try {
      const env = await api.env.check();
      set({ env });
    } catch (err) {
      get().pushToast('error', `环境检测失败：${String(err)}`);
    }
  },
  refreshCert: async () => {
    try {
      const r = await api.cert.status();
      set({ certInstalled: r.installed });
    } catch {
      /* ignore */
    }
  },
  refreshProxy: async () => {
    try {
      const proxy = await api.proxy.status();
      set({ proxy });
    } catch {
      /* ignore */
    }
  },
  refreshApiStatus: async () => {
    try {
      const s = await api.apiServer.status();
      set({ apiStatus: s });
    } catch {
      set({ apiStatus: null });
    }
  },
  refreshAccounts: async () => {
    // 并发序号（审查修复 M2，同 logsReqSeq 最新胜出模式）：旧响应直接丢弃，
    // 防「刚捕获的账号闪没」
    const seq = ++accountsReqSeq;
    try {
      const accounts = await api.accounts.list();
      if (seq === accountsReqSeq) set({ accounts });
    } catch (err) {
      if (seq === accountsReqSeq) {
        get().pushToast('error', `读取账号失败：${String(err)}`);
      }
    }
  },
  refreshGroups: async () => {
    try {
      const groups = await api.groups.list();
      set({ groups });
    } catch {
      /* ignore */
    }
  },
  refreshSettings: async () => {
    try {
      const settings = await api.misc.settingsGet();
      // notify 归一化：旧版本/数据迁移可能存入非枚举脏值（如空串），统一收敛为
      // 合法值，保证设置页下拉正确回显、pushToast 不再走兼容分支
      const validNotify = ['toast', 'system', 'both', 'none'];
      if (!validNotify.includes(settings.notify)) settings.notify = 'toast';
      // 侧边栏应用显示兜底：旧版本配置缺字段/脏值归一，固定应用非法回退 trae 且强制可见；
      // buddy-ai（国际版）不参与固定应用（脏值回退 trae，与后端 state.rs 归一口径一致）
      // hidden_apps 白名单对齐后端（state.rs:377，审查修复 L33）：仅 4 键且不含
      // buddy-ai——其可见性走独立 opt-in 开关 buddy_ai_visible，不进隐藏列表
      const validApps: AppKey[] = ['trae', 'buddy', 'qoder', 'doubao'];
      const pinnableApps: AppKey[] = ['trae', 'buddy', 'qoder', 'doubao'];
      if (!pinnableApps.includes(settings.pinned_app as AppKey)) settings.pinned_app = 'trae';
      settings.hidden_apps = (settings.hidden_apps ?? []).filter(
        (a): a is AppKey => validApps.includes(a) && a !== settings.pinned_app,
      );
      // BuddyAI 显示开关缺省归一为 false（默认不展示）
      settings.buddy_ai_visible = settings.buddy_ai_visible === true;
      // buddy-ai 打卡配置归一（开关默认关；时刻零值回填 09:20，与后端 state.rs 同口径）
      settings.buddy_ai_checkin_enabled = settings.buddy_ai_checkin_enabled === true;
      if (!settings.buddy_ai_checkin_hhmm?.trim()) settings.buddy_ai_checkin_hhmm = '09:20';
      settings.buddy_ai_checkin_gap_secs =
        typeof settings.buddy_ai_checkin_gap_secs === 'number' ? settings.buddy_ai_checkin_gap_secs : 3;
      // buddy-ai 定时同步/续期归一（开关默认开；时刻零值回填默认，与后端 serde default 同口径）
      settings.buddy_ai_renew_enabled = settings.buddy_ai_renew_enabled !== false;
      if (!settings.buddy_ai_renew_hhmm?.trim()) settings.buddy_ai_renew_hhmm = '10:35';
      settings.buddy_ai_credits_sync_enabled = settings.buddy_ai_credits_sync_enabled !== false;
      if (!settings.buddy_ai_credits_sync_hhmm?.trim()) settings.buddy_ai_credits_sync_hhmm = '23:45';
      // 同步模式白名单归一（审查 P3：历史/异常字符串会让 <select> 显示空选项；
      // 与后端 scheduler credits_sync_mode 兜底同口径，空值按旧 enabled 派生）
      {
        const m = String(settings.buddy_ai_credits_sync_mode ?? '').trim();
        const ok = ['off', 'hourly', 'every_hours', 'every_days', 'daily'].includes(m);
        settings.buddy_ai_credits_sync_mode = ok ? m : settings.buddy_ai_credits_sync_enabled ? 'daily' : 'off';
      }
      settings.buddy_ai_catalog_sync_enabled = settings.buddy_ai_catalog_sync_enabled !== false;
      if (!settings.buddy_ai_catalog_sync_hhmm?.trim()) settings.buddy_ai_catalog_sync_hhmm = '05:55';
      // 自定义图标兜底：非法应用 key / 空图标名丢弃（非法图标名由渲染层 resolveAppIcon 回退默认）。
      // 图标清理用独立白名单（审查修复 #7）：buddy-ai 的自定义图标合法——后端 state.rs:388
      // 的 retain 同样放行 'buddy-ai'；原复用上方 hidden_apps 的 validApps 会在每次
      // settingsGet 后剥除 buddy-ai 图标并随保存写回丢失
      const iconApps: AppKey[] = ['trae', 'buddy', 'qoder', 'doubao', 'buddy-ai'];
      const cleanIcons: Partial<Record<AppKey, string>> = {};
      for (const [k, v] of Object.entries(settings.app_icons ?? {})) {
        if (iconApps.includes(k as AppKey) && typeof v === 'string' && v.trim()) {
          cleanIcons[k as AppKey] = v.trim();
        }
      }
      settings.app_icons = cleanIcons;
      set({ settings, settingsLoaded: true });
    } catch {
      // F3：读取失败回退默认值仅供 UI 渲染；settingsLoaded 置 false 让 saveSettings
      // 拒绝写入，防止随后保存把默认值整表覆盖服务端真实配置（数据丢失）
      set({ settings: defaultSettings(), settingsLoaded: false });
    }
  },
  refreshLogs: async (q, manual) => {
    if (logsPollInflight && !manual) return;
    logsPollInflight = true;
    // 最新请求胜出：手动刷新绕过 inflight 防护会与轮询并发，旧响应不得覆盖新数据
    const seq = ++logsReqSeq;
    try {
      const logs = await api.misc.logsQuery({
        logType: q?.logType,
        date: q?.date,
        keyword: q?.keyword,
        limit: q?.limit ?? 500,
      });
      if (seq !== logsReqSeq) return;
      set({ logs });
    } catch (err) {
      if (seq !== logsReqSeq) return;
      const now = Date.now();
      if (manual || now - lastLogsErrToastAt > 60_000) {
        lastLogsErrToastAt = now;
        get().pushToast('error', `读取日志失败：${String(err)}`);
      }
    } finally {
      logsPollInflight = false;
    }
  },
  refreshCreditsHistory: async () => {
    try {
      const creditsHistory = await api.misc.creditsHistory();
      set({ creditsHistory });
    } catch (err) {
      get().pushToast('error', `读取积分历史失败：${String(err)}`);
    }
  },
  refreshCreditsDaily: async () => {
    try {
      const creditsDaily = await api.accounts.dailyList();
      set({ creditsDaily });
    } catch {
      /* ignore */
    }
  },

  startProxy: async () => {
    const port = get().settings?.proxy_port || 8899;
    try {
      const proxy = await api.proxy.start(port);
      set({ proxy, proxyLog: [] });
      get().pushToast('success', `代理已启动（端口 ${proxy.port}）`);
    } catch (err) {
      get().pushToast('error', `启动代理失败：${String(err)}`);
    }
  },
  stopProxy: async () => {
    try {
      const proxy = await api.proxy.stop();
      set({ proxy });
      get().pushToast('info', '代理已停止');
    } catch (err) {
      get().pushToast('error', `停止代理失败：${String(err)}`);
    }
  },
  openTrae: async () => {
    const env = get().env;
    if (!env?.installed) {
      get().pushToast('info', '未检测到 Trae 安装，正在打开下载页…');
      try {
        await api.env.openSite();
      } catch (err) {
        get().pushToast('error', `打开下载页失败：${String(err)}`);
      }
      return;
    }
    // 确保代理在请求路径上：未运行则先启动，否则打开 Trae 也不会走代理、无法捕获账号
    let port: number | undefined = get().proxy.running ? get().proxy.port : undefined;
    if (!port) {
      // 可能残留端口为 0 的无效代理，先停掉再以有效端口重启
      if (get().proxy.running) {
        try { await get().stopProxy(); } catch { /* ignore */ }
      }
      get().pushToast('info', '正在启动代理以确保 Trae 走本地代理…');
      await get().startProxy();
      port = get().proxy.running ? get().proxy.port : undefined;
    }
    try {
      if (port) {
        await api.env.openApp(port);
        get().pushToast('success', `已打开 Trae（代理已注入 127.0.0.1:${port}）`);
      } else {
        // 代理启动失败：仍打开客户端，但明确告知不会捕获账号
        await api.env.openApp(undefined);
        get().pushToast('warn', '代理启动失败，已直接打开 Trae（账号不会被自动捕获）');
      }
    } catch (err) {
      get().pushToast('error', `打开 Trae 失败：${String(err)}`);
    }
  },
  addAccount: async (name, jwt, groupId) => {
    try {
      await api.accounts.addManual(name, jwt, groupId);
      await get().refreshAccounts();
      get().pushToast('success', `账号「${name}」已添加`);
    } catch (err) {
      get().pushToast('error', `添加失败：${String(err)}`);
      throw err;
    }
  },
  deleteAccount: async (userId, deleteProfile) => {
    try {
      await api.accounts.delete(userId, deleteProfile);
      await get().refreshAccounts();
      get().pushToast('info', '账号已删除');
    } catch (err) {
      get().pushToast('error', `删除失败：${String(err)}`);
    }
  },
  updateAccount: async (userId, name, jwt) => {
    try {
      await api.accounts.update(userId, name, jwt);
      await get().refreshAccounts();
      get().pushToast('success', '账号已更新');
    } catch (err) {
      get().pushToast('error', `更新失败：${String(err)}`);
      throw err;
    }
  },
  createGroup: async (name, color) => {
    try {
      await api.groups.create(name, color);
      await get().refreshGroups();
      get().pushToast('success', `分组「${name}」已创建`);
    } catch (err) {
      get().pushToast('error', `创建分组失败：${String(err)}`);
      // F14：rethrow 供 GroupsModal 统一捕获（失败保留输入便于重试，防 unhandled rejection）
      throw err;
    }
  },
  updateGroup: async (id, patch) => {
    try {
      await api.groups.update(id, patch);
      await get().refreshGroups();
      // 写操作可见反馈（对齐 createGroup/removeGroup）
      get().pushToast(
        'success',
        patch.name ? `分组已重命名为「${patch.name}」` : '分组颜色已更新',
      );
    } catch (err) {
      get().pushToast('error', `更新分组失败：${String(err)}`);
      throw err;
    }
  },
  removeGroup: async (id) => {
    try {
      await api.groups.remove(id);
      await get().refreshGroups();
      await get().refreshAccounts();
      get().pushToast('info', '分组已删除');
    } catch (err) {
      get().pushToast('error', `删除分组失败：${String(err)}`);
      throw err;
    }
  },
  moveAccount: async (userId, groupId) => {
    try {
      await api.groups.move(userId, groupId);
      await get().refreshAccounts();
    } catch (err) {
      get().pushToast('error', `移动分组失败：${String(err)}`);
    }
  },
  resetDevice: async (userId) => {
    try {
      await api.misc.deviceReset(userId);
      await get().refreshAccounts();
      get().pushToast('success', '设备 ID 已重置');
    } catch (err) {
      get().pushToast('error', `重置失败：${String(err)}`);
    }
  },
  switchTo: async (userId, targetApp) => {
    try {
      set({ switchingTo: userId, switchProgress: [] });
      // withMinDelay：切换是高风险操作，保证 busy 态至少可见 1s（避免瞬间完成导致闪烁/误触连点）
      await withMinDelay(api.switchAccount(userId, targetApp));
      // 应用名后缀：Trae 静默；其余应用标注目标（Doubao/WorkBuddy/CodeBuddy/Qoder）
      get().pushToast('info', `正在切换登录态${targetApp && targetApp !== 'Trae' ? `（${targetApp}）` : ''}，请稍候…`);
    } catch (err) {
      set({ switchingTo: null });
      get().pushToast('error', `切换失败：${String(err)}`);
    }
  },
  clearSwitchLocks: () => {
    set({ switchingTo: null, savingLogin: null, renewPending: null });
  },
  saveCurrentLogin: async (userId, targetApp) => {
    try {
      set({ savingLogin: userId, saveLoginProgress: [] });
      await api.saveCurrentLogin(userId, targetApp);
      get().pushToast('info', '正在保存当前登录态，请稍候…');
    } catch (err) {
      set({ savingLogin: null });
      get().pushToast('error', `保存登录态失败：${String(err)}`);
    }
  },
  openDoubaoAs: async (userId, proxyPort) => {
    try {
      set({ switchingTo: userId, switchProgress: [] });
      await withMinDelay(api.doubao.openAs(userId, proxyPort));
      get().pushToast('info', `正在恢复账号 ${userId} 的快照并启动豆包，请稍候…`);
    } catch (err) {
      set({ switchingTo: null });
      get().pushToast('error', `以账号打开失败：${String(err)}`);
    }
  },
  renewJwt: async (userId) => {
    try {
      // 先占住 busy：preflight 探测最长 15s，期间按钮必须禁用，防止并发发起
      // 第二次切换/续期（对齐 switchTo「先 set busy 再 await」口径，防双击竞态）
      set({
        switchingTo: userId,
        switchProgress: ['[探测] 正在核验 JWT 服务端状态…'],
      });
      // 审查修复：先刷新账号列表取权威 expBefore——陈旧快照里 jwt_exp_hours=null 而
      // JWT 已被服务端吊销时，before==null 会让续期结论误报「续期成功」（expAfter>24
      // 即判成功，见 switch-done 分支）。refreshAccounts 失败自吞（内部 toast）不抛出，
      // 此处退化为沿用陈旧值，不劣于原行为；必须放在 set busy 之后，避免撕开防双击窗口
      await get().refreshAccounts();
      const acct = get().accounts.find((a) => a.user_id === userId);
      const expBefore = acct?.jwt_exp_hours ?? null;
      set({ renewPending: { uid: userId, expBefore } });
      // issue #126 前置判活：JWT 服务端仍存活时 TRAE 不会重新鉴权，切换+捕获必然空手——
      // 先探活，存活则直接给出路指引并中止，省一轮 30-90s 白切换。
      // 本地已过期/未知（expBefore<=0/null）不探测：判死放行，捕获链路正是恢复手段
      if (expBefore != null && expBefore > 0) {
        try {
          await api.accounts.renewPreflight(userId);
        } catch (guide) {
          set({ switchingTo: null, renewPending: null });
          get().pushToast('warn', String(guide));
          return;
        }
      }
      set((s) => ({
        switchProgress: [...s.switchProgress.slice(-49), '[探测] 通过，开始切换捕获'],
      }));
      // 若代理未运行则先启动
      if (!get().proxy.running) {
        get().pushToast('info', '正在启动代理以续期 JWT…');
        await get().startProxy();
      }
      // 代理在跑才注入端口：切换后 TRAE 以 --proxy-server 重启（Electron 单实例，
      // 切换流程会先关闭旧进程保证参数生效），客户端流量经 MITM，新 JWT 被自动捕获；
      // 代理未跑（启动失败等）则退化为普通切换，不带参数
      const proxy = get().proxy;
      const proxyPort = proxy?.running && proxy.port ? proxy.port : undefined;
      // 切换到目标账号，TRAE 重启后走代理，新 JWT 会被自动捕获
      // skipJwtProbe=true：续期场景目标账号 JWT 本就可能已被服务端吊销，跳过切换前预检
      // issue #44 遗留项：纳入切换状态机——进度面板与 90s 看门狗均依赖 switchingTo
      // 非空；终态由 switch-done 事件统一复位（含失败 toast）。busy 已在探测前置位，
      // 此处仅续写进度流（switchProgress 不可重置，会抹掉探测行）
      get().pushToast('info', '正在切换账号以捕获新 JWT，请稍候…');
      await api.switchAccount(userId, undefined, true, proxyPort);
    } catch (err) {
      set({ switchingTo: null, renewPending: null });
      get().pushToast('error', `续期失败：${String(err)}`);
    }
  },
  resetDeviceIds: async (targetApp) => {
    set({ deviceResetActive: true, deviceResetProgress: [] });
    try {
      await api.resetDeviceIds(targetApp);
      get().pushToast(
        'info',
        `正在执行 ${get().platform === 'macos' ? '5 层' : '6 层'}设备标识重置（Trae）…`,
      );
    } catch (err) {
      set({ deviceResetActive: false });
      get().pushToast('error', `设备标识重置失败：${String(err)}`);
    }
  },
  startCheckin: async (opts) => {
    // 重置签到状态，避免显示上一次的进度
    set({ checkin: { active: true, total: 0, index: 0, results: [], done: null, retry: null } });
    // F5：发起即武装看门狗（后续事件到达续期，done 解除）
    armCheckinWatchdog();
    try {
      await api.checkin.start(opts);
    } catch (err) {
      disarmCheckinWatchdog();
      set((s) => ({ checkin: { ...s.checkin, active: false } }));
      get().pushToast('error', `发起签到失败：${String(err)}`);
    }
  },
  refreshRemainingCredits: async () => {
    try {
      const ok = await api.accounts.refreshRemainingCredits();
      await get().refreshAccounts();
      if (ok > 0) {
        get().pushToast('success', `已刷新 ${ok} 个账号的可用积分`);
      }
    } catch (err) {
      get().pushToast('error', `刷新可用积分失败：${String(err)}`);
    }
  },
  cooldownClear: async (userId) => {
    try {
      await api.accounts.cooldownClear(userId);
      await get().refreshAccounts();
      get().pushToast('success', '已解除冷却');
    } catch (err) {
      get().pushToast('error', `解除冷却失败：${String(err)}`);
    }
  },
  refreshJwt: async (userId, force = true) => {
    try {
      await api.accounts.refreshJwt(userId, force);
      await get().refreshAccounts();
      get().pushToast('success', 'JWT 已自动刷新');
    } catch (err) {
      const msg = String(err);
      // 惰性刷新门（force=false）主动跳过：按中性提示呈现，不标红为失败
      if (msg.includes('暂无需刷新')) {
        get().pushToast('info', msg);
      } else {
        get().pushToast('error', `JWT 刷新失败：${msg}`);
      }
    }
  },
  saveSettings: async (patch) => {
    // F3：settings_get 失败时 store.settings 只是默认值兜底，此时保存会把默认值
    // 整表写回、覆盖服务端真实配置（数据丢失）——守卫拒绝写入并提示
    if (!get().settingsLoaded) {
      const msg = '设置读取失败，为避免用默认值覆盖真实配置已拒绝保存；请重启应用恢复后重试';
      get().pushToast('error', msg);
      // 以字符串抛出：调用方 String(err) 展示无 "Error: " 前缀
      throw msg;
    }
    const current = get().settings ?? defaultSettings();
    const next = { ...current, ...patch } as Settings;
    set({ settings: next });
    try {
      // 后端 settings_set 为真 patch 语义（仅写出现的字段）——只发 patch，
      // 不发合并后的全量对象：store.settings 可能落后于其他通道（如系统设置弹框）刚保存的值，
      // 全量发送会把那些字段回滚成旧值
      await api.misc.settingsSet(patch);
    } catch (err) {
      // 字段级回滚（C3 审查修复）：只还原本次 patch 涉及的字段，且以最新 state
      // 为基做函数式合并——整表回滚会把保存期间其他通道刚写入的字段一并回滚
      set((s) => {
        const revert: Partial<Settings> = {};
        for (const k of Object.keys(patch) as Array<keyof Settings>) {
          (revert as Record<string, unknown>)[k] = current[k];
        }
        return { settings: { ...(s.settings ?? defaultSettings()), ...revert } };
      });
      get().pushToast('error', `保存设置失败：${String(err)}`);
      // rethrow：让调用方 catch 感知失败，避免误弹「已保存」成功提示
      throw err;
    }
  },

  refreshProfiles: async () => {
    try {
      const profiles = await api.profiles.list('Trae');
      set({ profiles });
    } catch {
      /* ignore */
    }
  },
  refreshLocalEntitlement: async () => {
    try {
      const localEntitlement = await api.traeApps.localEntitlement();
      set({ localEntitlement });
    } catch {
      /* ignore */
    }
  },
  profileBackup: async (userId) => {
    set({ profileActive: true, profileProgress: [] });
    try {
      await api.profiles.backup(userId, 'Trae');
      get().pushToast('info', '正在备份登录态快照…');
    } catch (err) {
      set({ profileActive: false });
      get().pushToast('error', `备份失败：${String(err)}`);
    }
  },
  profileRestore: async (userId) => {
    set({ profileActive: true, profileProgress: [] });
    try {
      await api.profiles.restore(userId, 'Trae');
      get().pushToast('info', '正在恢复登录态快照…');
    } catch (err) {
      set({ profileActive: false });
      get().pushToast('error', `恢复失败：${String(err)}`);
    }
  },
  profileDelete: async (userId) => {
    try {
      await api.profiles.delete(userId, 'Trae');
      await get().refreshProfiles();
      get().pushToast('info', '快照已删除');
    } catch (err) {
      get().pushToast('error', `删除失败：${String(err)}`);
    }
  },
  oauthLogin: async (callbackUrl, accountName, groupId, expectUid) => {
    try {
      const result = await api.oauth.login(callbackUrl, accountName, groupId, expectUid);
      await get().refreshAccounts();
      // 同 uid 合并场景如实提示「更新已有账号」而非「已添加」（issue #80）
      if (result.merged) {
        const actual = result.existing_name || result.name;
        get().pushToast(
          'success',
          `OAuth 登录成功：与已有账号「${actual}」同 uid，已更新其凭证（未新增账号）`,
        );
      } else {
        get().pushToast('success', `OAuth 登录成功：账号「${result.name}」已添加`);
      }
    } catch (err) {
      get().pushToast('error', `OAuth 登录失败：${String(err)}`);
      throw err;
    }
  },

  pushToast: (kind, msg) => {
    // 通知方式兼容：仅 'none'/'system' 有特殊语义，其余值（含历史脏数据如中文标签）
    // 一律按应用内 toast 处理，避免非枚举值把提示静默吞掉（表现为「点了没任何反应」）
    const mode = get().settings?.notify ?? 'toast';
    if (mode !== 'none' && mode !== 'system') {
      if (mode !== 'toast' && mode !== 'both') {
        console.info('[notify] 未知通知方式，已按 toast 处理:', mode);
      }
      const id = ++toastSeq;
      set((s) => ({ toasts: [...s.toasts, { id, kind, msg }] }));
      // F12：error 级驻留 10s（长错误文案需要阅读时间），其余维持 4.2s
      setTimeout(() => get().dismissToast(id), kind === 'error' ? 10_000 : 4200);
    }

    if (mode === 'system' || mode === 'both') {
      // sendNotification v2 返回 void（fire-and-forget），用 try-catch 防御同步异常
      try {
        sendNotification({ title: APP_NAME, body: msg });
        console.info('[notify] 系统通知已发送:', msg);
      } catch (e) {
        console.warn('[notify] sendNotification 异常:', e);
      }
    }
  },
  dismissToast: (id) => set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) })),
}));
