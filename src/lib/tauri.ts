import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type {
  AccountView,
  CheckinDone,
  CheckinOpts,
  EnvStatus,
  GroupView,
  JwtParseResult,
  LogLine,
  ProxyStatus,
  Settings,
} from '../types';

// 所有 invoke 封装集中于此，字段名严格遵循 Rust 端 snake_case 约定。
export const api = {
  env: {
    check: () => invoke<EnvStatus>('env_check'),
    openSite: () => invoke('open_trae_website'),
  },
  cert: {
    status: () => invoke<{ installed: boolean }>('cert_status'),
    install: () => invoke<{ installed: boolean }>('cert_install'),
  },
  proxy: {
    start: (port: number) => invoke<ProxyStatus>('proxy_start', { port }),
    stop: () => invoke<ProxyStatus>('proxy_stop'),
    status: () => invoke<ProxyStatus>('proxy_status'),
  },
  accounts: {
    list: () => invoke<AccountView[]>('accounts_list'),
    addManual: (name: string, jwt: string, groupId?: string) =>
      invoke('account_add_manual', { name, jwt, groupId }),
    delete: (userId: string, deleteProfile: boolean) =>
      invoke('account_delete', { userId, deleteProfile }),
  },
  groups: {
    list: () => invoke<GroupView[]>('groups_list'),
    create: (name: string, color: string) => invoke<string>('group_create', { name, color }),
    update: (id: string, patch: { name?: string; color?: string; order?: number }) =>
      invoke('group_update', { id, ...patch }),
    remove: (id: string) => invoke('group_delete', { id }),
    move: (userId: string, groupId: string | null) => invoke('group_move', { userId, groupId }),
  },
  checkin: {
    start: (opts: CheckinOpts) => invoke('checkin_start', { opts }),
  },
  misc: {
    deviceReset: (userId: string) => invoke('device_reset', { userId }),
    jwtParse: (jwt: string) => invoke<JwtParseResult>('jwt_parse', { jwt }),
    logsQuery: (opts: {
      logType?: string;
      date?: string;
      keyword?: string;
      limit?: number;
    }) =>
      invoke<LogLine[]>('logs_query', {
        opts: {
          log_type: opts.logType,
          date: opts.date,
          keyword: opts.keyword,
          limit: opts.limit,
        },
      }),
    settingsGet: () => invoke<Settings>('settings_get'),
    settingsSet: (patch: Settings) => invoke('settings_set', { patch }),
    inviteLink: () => invoke<{ url: string }>('invite_link'),
    taskRegister: (time: string) => invoke('task_register', { time }),
    taskStatus: () => invoke<string>('task_status'),
    taskUnregister: () => invoke('task_unregister'),
  },
  switchAccount: (userId: string) => invoke('switch_account', { userId }),
};

// ---- 事件载荷 ----
export interface CheckinStartEvent {
  type: 'start';
  total: number;
}
export interface CheckinAccountEvent {
  type: 'account';
  index: number;
  user_id: string;
  name: string;
  status: 'already' | 'success' | 'fail';
  credits?: number;
  delta?: number;
  elapsed?: number;
  code?: number;
  message?: string;
}
export interface CheckinDoneEvent {
  type: 'done';
  ok: number;
  already: number;
  failed: number;
  total?: number;
}
export type CheckinProgressEvent =
  | CheckinStartEvent
  | CheckinAccountEvent
  | CheckinDoneEvent;

export interface ListenerHandlers {
  onProxyLog?: (line: string) => void;
  onAccountCaptured?: (uid: string) => void;
  onCheckinProgress?: (e: CheckinProgressEvent) => void;
  onSwitchProgress?: (line: string) => void;
}

export async function setupListeners(
  handlers: ListenerHandlers,
): Promise<UnlistenFn[]> {
  const unsubs: UnlistenFn[] = [];
  if (handlers.onProxyLog) {
    unsubs.push(
      await listen<string>('proxy-log', (e) => handlers.onProxyLog!(e.payload)),
    );
  }
  if (handlers.onAccountCaptured) {
    unsubs.push(
      await listen<string>('account-captured', (e) =>
        handlers.onAccountCaptured!(e.payload),
      ),
    );
  }
  if (handlers.onCheckinProgress) {
    unsubs.push(
      await listen<CheckinProgressEvent>('checkin-progress', (e) =>
        handlers.onCheckinProgress!(e.payload),
      ),
    );
  }
  if (handlers.onSwitchProgress) {
    unsubs.push(
      await listen<string>('switch-progress', (e) =>
        handlers.onSwitchProgress!(e.payload),
      ),
    );
  }
  return unsubs;
}
