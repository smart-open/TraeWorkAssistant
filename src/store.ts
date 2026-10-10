import { create } from 'zustand';
import { api, setupListeners, ApiError, UNAUTHORIZED_EVENT, type CheckinProgressEvent } from './lib/tauri';
import type {
  AccountView,
  CheckinAccountResult,
  CheckinDone,
  CreditsDailySnapshot,
  GroupView,
  LogLine,
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

/** Qoder 签到进行态（store 化：事件监听在 store 层注册，不随页面卸载——
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
  /** 管理面登录态（ADR-4）：false 时整页显示登录页 */
  authed: boolean;
  view: ViewKey;
  /** 侧边栏应用切换（trae = Trae 菜单；buddy = WorkBuddy 菜单） */
  activeApp: AppKey;
  accounts: AccountView[];
  groups: GroupView[];
  settings: Settings | null;
  logs: LogLine[];
  creditsDaily: CreditsDailySnapshot[];
  checkin: CheckinState;
  /** Qoder 签到进行态（store 单例持有，页面切走不丢事件） */
  qoderCheckin: QoderCheckinState;
  toasts: Toast[];
  /** 全局 API 管理弹窗（unified-api-gateway-design §5.2；任意 activeApp 视图均可打开） */
  showApiManager: boolean;

  init: () => Promise<void>;
  /** 登录成功后调用：注册 SSE 监听 + 全量刷新数据 */
  afterLogin: () => Promise<void>;
  /** 会话失效/登出：断开监听、清空数据、回登录页 */
  resetAuth: () => void;
  setView: (v: ViewKey) => void;
  /** 切换侧边栏应用 Tab，并跳到该应用默认首页 */
  setActiveApp: (app: AppKey) => void;
  applyCheckinEvent: (e: CheckinProgressEvent) => void;
  /** Qoder 签到 NDJSON 进度归约（start/done/逐账号行/exit；skipped_busy 与
   * empty 计数口径对齐 Rust 侧） */
  applyQoderCheckinLine: (line: string) => void;
  /** 发起 Qoder 签到（连点自守；失败置回 running 并 toast） */
  startQoderCheckin: () => Promise<void>;

  refreshAccounts: () => Promise<void>;
  refreshGroups: () => Promise<void>;
  refreshSettings: () => Promise<void>;
  refreshLogs: (q?: LogQuery, manual?: boolean) => Promise<void>;
  refreshCreditsDaily: () => Promise<void>;
  setShowApiManager: (v: boolean) => void;

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
  oauthLogin: (callbackUrl: string, accountName?: string, groupId?: string) => Promise<void>;

  pushToast: (kind: ToastKind, msg: string) => void;
  dismissToast: (id: number) => void;
}

let toastSeq = 0;
// 已注册的事件监听取消函数；重复注册前先注销旧监听避免重复
let unsubs: Array<() => void> = [];
/** init 幂等锁：StrictMode 双跑（dev）时第二次调用直接返回，防并发双注册监听 */
let initStarted = false;
/** 全局 401 拦截是否已绑定（绑定一次即可） */
let unauthorizedBound = false;

function defaultSettings(): Settings {
  return {
    proxy_port: 8899,
    theme: 'system',
    launch_minimized: false,
    silent_checkin: false,
    auto_start_proxy: true,
    tray: true,
    language: 'zh-CN',
    checkin_skip_checked: true,
    checkin_skip_expired: true,
    retry: 1,
    notify: 'toast',
    trae_path: null,
    trae_cn_path: null,
    doubao_path: null,
    doubao_renew_url: 'https://www.doubao.com/info/v2/',
    doubao_quota_url: 'https://www.doubao.com/alice/commerce/sale/subscription/quota/summary/',
    doubao_snapshot_include_idb: false,
    workbuddy_path: null,
    codebuddy_path: null,
    wb_auth_file_path: null,
    data_dir: null,
    log_retention_days: 30,
    proxy_domains: 'trae.cn,trae.com.cn,mchost.guru,zijieapi.com,bytedance.com,volcengine.com,volces.com,treecode.com,doubao.com',
    proxy_log_path: null,
    api_port: 7864,
    api_default_model: 'deepseek-v4-flash',
    // F-74：切换时自动迁移会话——默认关（旧行为保持"只切登录态，不写会话"）
    buddy_switch_migrate_chats: false,
    // WebUI 免令牌访问——默认关（保持令牌登录）
    web_auth_disabled: false,
    // 侧边栏应用显示：默认固定 Trae，其余应用全部显示
    pinned_app: 'trae',
    hidden_apps: [],
    // 侧边栏应用自定义图标（key → 图标名；空 = 全部用内置默认）
    app_icons: {},
    // 签到多账号间隔秒（默认 3s 防上游频控，0=关闭；Trae/Buddy/Qoder 各自配置页可改，Buddy 签到与成长共用）
    trae_checkin_gap_secs: 3,
    wb_checkin_gap_secs: 3,
    qoder_checkin_gap_secs: 3,
  };
}

// 侧边栏应用显示兜底：旧版本配置缺字段/脏值归一，固定应用非法回退 trae 且强制可见
// （合法 key 按 docker 分支应用集合裁剪：无 doubao）
function normalizeAppDisplay(settings: Settings): void {
  const validApps: AppKey[] = ['trae', 'buddy', 'qoder'];
  if (!validApps.includes(settings.pinned_app as AppKey)) settings.pinned_app = 'trae';
  settings.hidden_apps = (settings.hidden_apps ?? []).filter(
    (a): a is AppKey => validApps.includes(a) && a !== settings.pinned_app,
  );
  // 自定义图标兜底：非法应用 key / 空图标名丢弃（非法图标名由渲染层 resolveAppIcon 回退默认）
  const cleanIcons: Partial<Record<AppKey, string>> = {};
  for (const [k, v] of Object.entries(settings.app_icons ?? {})) {
    if (validApps.includes(k as AppKey) && typeof v === 'string' && v.trim()) {
      cleanIcons[k as AppKey] = v.trim();
    }
  }
  settings.app_icons = cleanIcons;
}

// 日志轮询去重：上一轮未返回时跳过本轮（防 2s 轮询堆积与旧响应乱序覆盖）
let logsPollInflight = false;
// 日志查询并发序号（最新请求胜出）：手动刷新与轮询并发时，旧响应直接丢弃
let logsReqSeq = 0;
// 同因 toast 限频：读取持续失败期间每 60s 最多弹一次（防 toast 风暴）；手动调用直通
let lastLogsErrToastAt = 0;

export const useAppStore = create<AppState>((set, get) => ({
  ready: false,
  authed: false,
  view: 'dashboard',
  activeApp: 'trae',
  accounts: [],
  groups: [],
  settings: null,
  logs: [],
  creditsDaily: [],
  checkin: { active: false, total: 0, index: 0, results: [], done: null, retry: null },
  qoderCheckin: { running: false, lines: [], done: null, doneRev: 0 },
  toasts: [],
  showApiManager: false,

  init: async () => {
    // StrictMode 下 effect 会执行两次（dev）：幂等锁保证监听只注册一次（审查修复 P1-16）
    if (initStarted) return;
    initStarted = true;
    // 全局 401 拦截：任一命令返回未登录 → 清理本地会话状态回登录页
    if (!unauthorizedBound) {
      unauthorizedBound = true;
      window.addEventListener(UNAUTHORIZED_EVENT, () => get().resetAuth());
    }
    // 登录探测：settings_get 需鉴权，401 → 停在登录页等待用户输入 token
    try {
      const settings = await api.misc.settingsGet();
      // notify 归一化：旧版本/数据迁移可能存入非枚举脏值（如空串），统一收敛为合法值
      const validNotify = ['toast', 'system', 'both', 'none'];
      if (!validNotify.includes(settings.notify)) settings.notify = 'toast';
      normalizeAppDisplay(settings);
      set({ settings, authed: true });
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) {
        set({ authed: false, ready: true });
        return;
      }
      // 非 401（网络/服务异常）：仍进入主界面，由各页面 toast 呈现具体错误
      set({ settings: get().settings ?? defaultSettings(), authed: true });
    }
    await get().afterLogin();
  },

  afterLogin: async () => {
    // 重登录场景：先注销旧监听再注册，避免 SSE 双订阅
    unsubs.forEach((fn) => fn());
    unsubs = [];
    unsubs = await setupListeners({
      onCheckinProgress: (e) => get().applyCheckinEvent(e),
      onQoderCheckinProgress: (line) => get().applyQoderCheckinLine(line),
    });
    await Promise.all([
      get().refreshAccounts(),
      get().refreshGroups(),
      get().refreshSettings(),
      get().refreshCreditsDaily(),
    ]);
    set({ ready: true });
  },

  resetAuth: () => {
    unsubs.forEach((fn) => fn());
    unsubs = [];
    // 保留 ready 与主题等本地态；数据清空防止串号
    set({
      authed: false,
      accounts: [],
      groups: [],
      logs: [],
      creditsDaily: [],
      checkin: { active: false, total: 0, index: 0, results: [], done: null, retry: null },
      // 审查 #19：qoderCheckin 与 checkin 同款清空，防止重登录后残留上一会话进度串号
      qoderCheckin: { running: false, lines: [], done: null, doneRev: 0 },
    });
  },

  setView: (v) => set({ view: v }),
  setActiveApp: (app) => set({ activeApp: app, view: APP_HOME_VIEW[app] }),
  setShowApiManager: (v) => set({ showApiManager: v }),

  applyCheckinEvent: (e) => {
    set((s) => {
      if (e.type === 'start') {
        // start 带 scope 内全集清单（候选 pending / 跳过带原因 / 重试轮沿用上轮状态），
        // 重建列表使被跳过的账号也可见；无 accounts 时仅同步候选总数
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
        // 无 accounts 的 start 同样重置进度与旧结果，避免上一轮残留干扰本轮展示
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
      // JWT 吊销类失败的精确提示（issue #9）：401=服务端已吊销 JWT，重新 OAuth 录入即可恢复
      const deadCount = get().checkin.results.filter(
        (r) => r?.status === 'fail' && r.error_type === 'SessionDead',
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
          `${deadCount} 个账号 JWT 已被服务端吊销（该账号在别处重新登录/IDE 内退出过登录）：请在账号管理页对该账号重新执行 OAuth 登录录入`,
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
      // 跨进程锁被占用：整轮幂等跳过，不误报「成功完成」、不落全 0 的 done 徽标
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
      get().pushToast(
        failed - empty > 0 ? 'warn' : 'success',
        `Qoder 签到完成：成功 ${done.ok}，已签 ${done.already}，失败 ${done.failed}` +
          (empty > 0 ? `（其中 ${empty} 项为活动未开放）` : ''),
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

  refreshAccounts: async () => {
    try {
      const accounts = await api.accounts.list();
      set({ accounts });
    } catch (err) {
      get().pushToast('error', `读取账号失败：${String(err)}`);
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
      const validNotify = ['toast', 'system', 'both', 'none'];
      if (!validNotify.includes(settings.notify)) settings.notify = 'toast';
      normalizeAppDisplay(settings);
      set({ settings });
    } catch {
      set({ settings: defaultSettings() });
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
  refreshCreditsDaily: async () => {
    try {
      const creditsDaily = await api.accounts.dailyList();
      set({ creditsDaily });
    } catch {
      /* ignore */
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
    }
  },
  updateGroup: async (id, patch) => {
    try {
      await api.groups.update(id, patch);
      await get().refreshGroups();
    } catch (err) {
      get().pushToast('error', `更新分组失败：${String(err)}`);
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
  startCheckin: async (opts) => {
    // 重置签到状态，避免显示上一次的进度
    set({ checkin: { active: true, total: 0, index: 0, results: [], done: null, retry: null } });
    try {
      await api.checkin.start(opts);
    } catch (err) {
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
    const current = get().settings ?? defaultSettings();
    const next = { ...current, ...patch } as Settings;
    set({ settings: next });
    try {
      await api.misc.settingsSet(next);
    } catch (err) {
      // 回滚到修改前的值，避免 UI 显示与后端不一致
      set({ settings: current });
      get().pushToast('error', `保存设置失败：${String(err)}`);
      // rethrow：让调用方 catch 感知失败，避免误弹「已保存」成功提示
      throw err;
    }
  },
  oauthLogin: async (callbackUrl, accountName, groupId) => {
    try {
      const result = await api.oauth.login(callbackUrl, accountName, groupId);
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
    // Web 版仅应用内 Toast；notify='none' 保留静默语义
    const mode = get().settings?.notify ?? 'toast';
    if (mode === 'none') return;
    const id = ++toastSeq;
    set((s) => ({ toasts: [...s.toasts, { id, kind, msg }] }));
    setTimeout(() => get().dismissToast(id), 4200);
  },
  dismissToast: (id) => set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) })),
}));
