import { create } from 'zustand';
import { api, setupListeners, ApiError, UNAUTHORIZED_EVENT, type CheckinProgressEvent } from './lib/tauri';
import type {
  AccountView,
  CheckinAccountResult,
  CheckinDone,
  CreditsDailySnapshot,
  GroupView,
  LogLine,
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
  };
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
      get().pushToast('success', `OAuth 登录成功：账号「${result.name}」已添加`);
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
