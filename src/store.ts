import { create } from 'zustand';
import { api, setupListeners, type CheckinProgressEvent } from './lib/tauri';
import type {
  AccountView,
  CheckinAccountResult,
  CheckinDone,
  EnvStatus,
  GroupView,
  LogLine,
  ProxyStatus,
  Settings,
} from './types';

export type ToastKind = 'info' | 'success' | 'error' | 'warn';
export interface Toast {
  id: number;
  kind: ToastKind;
  msg: string;
}

export interface CheckinState {
  active: boolean;
  total: number;
  index: number;
  results: CheckinAccountResult[];
  done: CheckinDone | null;
}

export interface LogQuery {
  logType?: string;
  date?: string;
  keyword?: string;
  limit?: number;
}

interface AppState {
  ready: boolean;
  env: EnvStatus | null;
  certInstalled: boolean;
  proxy: ProxyStatus;
  accounts: AccountView[];
  groups: GroupView[];
  settings: Settings | null;
  logs: LogLine[];
  proxyLog: string[];
  switchProgress: string[];
  checkin: CheckinState;
  toasts: Toast[];

  init: () => Promise<void>;
  applyCheckinEvent: (e: CheckinProgressEvent) => void;

  refreshEnv: () => Promise<void>;
  refreshCert: () => Promise<void>;
  refreshProxy: () => Promise<void>;
  refreshAccounts: () => Promise<void>;
  refreshGroups: () => Promise<void>;
  refreshSettings: () => Promise<void>;
  refreshLogs: (q?: LogQuery) => Promise<void>;

  startProxy: () => Promise<void>;
  stopProxy: () => Promise<void>;
  addAccount: (name: string, jwt: string, groupId?: string) => Promise<void>;
  deleteAccount: (userId: string, deleteProfile: boolean) => Promise<void>;
  createGroup: (name: string, color: string) => Promise<void>;
  updateGroup: (
    id: string,
    patch: { name?: string; color?: string; order?: number },
  ) => Promise<void>;
  removeGroup: (id: string) => Promise<void>;
  moveAccount: (userId: string, groupId: string | null) => Promise<void>;
  resetDevice: (userId: string) => Promise<void>;
  switchTo: (userId: string) => Promise<void>;
  startCheckin: (opts: {
    scope: string;
    user_ids?: string[];
    skip_checked_in: boolean;
    skip_expired: boolean;
  }) => Promise<void>;
  saveSettings: (patch: Partial<Settings>) => Promise<void>;

  pushToast: (kind: ToastKind, msg: string) => void;
  dismissToast: (id: number) => void;
}

let toastSeq = 0;

function defaultSettings(): Settings {
  return {
    proxy_port: 8899,
    theme: 'system',
    launch_minimized: false,
    auto_start_proxy: true,
    tray: true,
    language: 'zh-CN',
    checkin_skip_checked: true,
    checkin_skip_expired: true,
    retry: 1,
    notify: 'toast',
    trae_path: null,
    data_dir: null,
    log_retention_days: 30,
  };
}

export const useAppStore = create<AppState>((set, get) => ({
  ready: false,
  env: null,
  certInstalled: false,
  proxy: { running: false, port: 0, captured: 0, started_at: null },
  accounts: [],
  groups: [],
  settings: null,
  logs: [],
  proxyLog: [],
  switchProgress: [],
  checkin: { active: false, total: 0, index: 0, results: [], done: null },
  toasts: [],

  init: async () => {
    await setupListeners({
      onProxyLog: (line) =>
        set((s) => ({ proxyLog: [...s.proxyLog.slice(-199), line] })),
      onAccountCaptured: (uid) => {
        get().pushToast('success', `已捕获账号 ${uid}`);
        void get().refreshAccounts();
      },
      onCheckinProgress: (e) => get().applyCheckinEvent(e),
      onSwitchProgress: (line) =>
        set((s) => ({ switchProgress: [...s.switchProgress.slice(-49), line] })),
    });
    await Promise.all([
      get().refreshEnv(),
      get().refreshCert(),
      get().refreshProxy(),
      get().refreshAccounts(),
      get().refreshGroups(),
      get().refreshSettings(),
    ]);
    set({ ready: true });
  },

  applyCheckinEvent: (e) => {
    set((s) => {
      if (e.type === 'start') {
        return {
          checkin: { active: true, total: e.total, index: 0, results: [], done: null },
        };
      }
      if (e.type === 'account') {
        const results = s.checkin.results.slice();
        const i = e.index - 1;
        results[i] = {
          index: e.index,
          user_id: e.user_id,
          name: e.name,
          status: e.status,
          credits: e.credits,
          delta: e.delta,
          elapsed: e.elapsed,
          code: e.code,
          message: e.message,
        };
        return { checkin: { ...s.checkin, index: e.index, results } };
      }
      return {
        checkin: {
          ...s.checkin,
          active: false,
          done: { ok: e.ok, already: e.already, failed: e.failed, total: e.total },
        },
      };
    });
    if (e.type === 'done') {
      void get().refreshAccounts();
      get().pushToast(
        e.failed > 0 ? 'warn' : 'success',
        `签到完成：成功 ${e.ok}，已签 ${e.already}，失败 ${e.failed}`,
      );
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
      set({ settings });
    } catch {
      set({ settings: defaultSettings() });
    }
  },
  refreshLogs: async (q) => {
    try {
      const logs = await api.misc.logsQuery({
        logType: q?.logType,
        date: q?.date,
        keyword: q?.keyword,
        limit: q?.limit ?? 500,
      });
      set({ logs });
    } catch (err) {
      get().pushToast('error', `读取日志失败：${String(err)}`);
    }
  },

  startProxy: async () => {
    const port = get().settings?.proxy_port ?? 8899;
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
  resetDevice: async (userId) => {
    try {
      await api.misc.deviceReset(userId);
      await get().refreshAccounts();
      get().pushToast('success', '设备 ID 已重置');
    } catch (err) {
      get().pushToast('error', `重置失败：${String(err)}`);
    }
  },
  switchTo: async (userId) => {
    try {
      await api.switchAccount(userId);
      get().pushToast('info', '已发起登录态切换，请稍候…');
    } catch (err) {
      get().pushToast('error', `切换失败：${String(err)}`);
    }
  },
  startCheckin: async (opts) => {
    try {
      await api.checkin.start(opts);
    } catch (err) {
      get().pushToast('error', `发起签到失败：${String(err)}`);
    }
  },
  saveSettings: async (patch) => {
    const current = get().settings ?? defaultSettings();
    const next = { ...current, ...patch } as Settings;
    set({ settings: next });
    try {
      await api.misc.settingsSet(next);
    } catch (err) {
      get().pushToast('error', `保存设置失败：${String(err)}`);
    }
  },

  pushToast: (kind, msg) => {
    const id = ++toastSeq;
    set((s) => ({ toasts: [...s.toasts, { id, kind, msg }] }));
    setTimeout(() => get().dismissToast(id), 4200);
  },
  dismissToast: (id) => set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) })),
}));
