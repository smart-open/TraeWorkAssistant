import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type {
  AccountView,
  ApiServiceStatus,
  ApiPoolFile,
  ApiKeyEntry,
  AppLocate,
  CheckinDone,
  CheckinOpts,
  CheckinTrendPoint,
  CreditRecord,
  CreditDetail,
  CreditsDailySnapshot,
  DiscoveredAccount,
  EnvStatus,
  GroupView,
  JwtParseResult,
  LocalEntitlement,
  ImportReport,
  ImportPreview,
  LogLine,
  ModelOption,
  OAuthLoginUrl,
  OAuthLoginResult,
  PoolStatus,
  ProfileInfo,
  ProxyLogListResult,
  ProxyStatus,
  Settings,
  UpdateCheckResult,
  UpdateDownloaded,
  UpdateDownloadProgress,
  UsageDayView,
} from '../types';

// 所有 invoke 封装集中于此，字段名严格遵循 Rust 端 snake_case 约定。
export const api = {
  env: {
    check: () => invoke<EnvStatus>('env_check'),
    checkCn: () => invoke<EnvStatus>('env_check_trae_cn'),
    // F-01：跨应用安装位置自动识别（trae_work | trae | doubao | workbuddy）
    locate: (targetApp?: string) => invoke<AppLocate>('app_locate', { targetApp }),
    openSite: () => invoke('open_trae_website'),
    openApp: (proxyPort?: number) => invoke('open_trae_app', { proxyPort }),
    openCnApp: (proxyPort?: number) => invoke('open_trae_cn_app', { proxyPort }),
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
    update: (userId: string, name?: string, jwt?: string) =>
      invoke('account_update', { userId, name, jwt }),
    fetchRemainingCredits: (userId: string) =>
      invoke<number>('fetch_remaining_credits', { userId }),
    creditDetail: (userId: string) =>
      invoke<CreditDetail>('fetch_credit_detail', { userId }),
    refreshRemainingCredits: () =>
      invoke<number>('refresh_remaining_credits'),
    dailyList: () =>
      invoke<CreditsDailySnapshot[]>('credits_daily_list'),
    cooldownClear: (userId: string) =>
      invoke('cooldown_clear', { userId }),
    cooldownClearAll: () =>
      invoke<number>('cooldown_clear_all'),
    refreshJwt: (userId: string) =>
      invoke<string>('refresh_jwt', { userId }),
    exportRaw: () => invoke<Record<string, unknown>>('accounts_export_raw'),
    importAccounts: (content: string, only?: number[]) =>
      invoke<ImportReport>('accounts_import', { content, only }),
    // F-46：导入前预览（解析账号/分组、标记已存在，不写盘）
    importPreview: (content: string) => invoke<ImportPreview>('accounts_import_preview', { content }),
    // F-08 双应用账号自动发现
    discover: () => invoke<DiscoveredAccount[]>('apps_accounts_discover'),
    addDiscovered: (userId: string, name: string, app: string, dcUid?: string | null) =>
      invoke('apps_account_add', { userId, name, app, dcId: dcUid ?? null }),
    // 会员/套餐信息
    refreshPayStatus: () => invoke<number>('refresh_pay_status'),
  },
  traeApps: {
    localEntitlement: () => invoke<LocalEntitlement>('apps_entitlement_read'),
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
    // T8：近 N 天签到结果趋势（Dashboard 堆叠图）
    trends: (days?: number) =>
      invoke<CheckinTrendPoint[]>('checkin_trends', { days: days ?? null }),
  },
  misc: {
    deviceReset: (userId: string) => invoke('device_reset', { userId }),
    // T11：开机自启（开关即时生效）
    autostartStatus: () => invoke<boolean>('autostart_status'),
    autostartSet: (enabled: boolean) => invoke('autostart_set', { enabled }),
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
    // T6：按类型清理日志文件（all 为全清），返回删除的文件数
    logsClear: (logType: string) => invoke<number>('logs_clear', { logType }),
    settingsGet: () => invoke<Settings>('settings_get'),
    settingsSet: (patch: Settings) => invoke('settings_set', { patch }),
    creditsHistory: () => invoke<CreditRecord[]>('credits_history'),
    inviteLink: () => invoke<{ url: string }>('invite_link'),
    taskRegister: (time: string) => invoke('task_register', { time }),
    taskStatus: () => invoke<string>('task_status'),
    taskUnregister: () => invoke('task_unregister'),
    proxyLogsList: (opts: {
      keyword?: string;
      startTime?: string;
      endTime?: string;
      offset?: number;
      limit?: number;
    }) => invoke<ProxyLogListResult>('proxy_logs_list', {
      opts: {
        keyword: opts.keyword,
        start_time: opts.startTime,
        end_time: opts.endTime,
        offset: opts.offset,
        limit: opts.limit,
      },
    }),
    proxyLogDetail: (id: string) => invoke<string>('proxy_log_detail', { id }),
    writeTextFile: (path: string, content: string) =>
      invoke('write_text_file', { path, content }),
    readTextFile: (path: string) => invoke<string>('read_text_file', { path }),
  },
  switchAccount: (userId: string, targetApp?: 'TraeWork' | 'Trae') =>
    invoke('switch_account', { userId, targetApp: targetApp ?? null }),
  saveCurrentLogin: (userId: string, targetApp?: 'TraeWork' | 'Trae') =>
    invoke('save_current_login', { userId, targetApp: targetApp ?? null }),
  resetDeviceIds: (targetApp?: 'TraeWork' | 'Trae') =>
    invoke('reset_device_ids', { targetApp: targetApp ?? null }),
  profiles: {
    list: (targetApp?: 'TraeWork' | 'Trae') =>
      invoke<ProfileInfo[]>('profile_list', { targetApp: targetApp ?? null }),
    backup: (userId: string, targetApp?: 'TraeWork' | 'Trae') =>
      invoke('profile_backup', { userId, targetApp: targetApp ?? null }),
    restore: (userId: string, targetApp?: 'TraeWork' | 'Trae') =>
      invoke('profile_restore', { userId, targetApp: targetApp ?? null }),
    delete: (userId: string, targetApp?: 'TraeWork' | 'Trae') =>
      invoke('profile_delete', { userId, targetApp: targetApp ?? null }),
    formatSize: (bytes: number) => invoke<string>('profile_format_size', { bytes }),
  },
  oauth: {
    getLoginUrl: () => invoke<OAuthLoginUrl>('oauth_get_login_url'),
    parseCallback: (callbackUrl: string) =>
      invoke('oauth_parse_callback', { callbackUrl }),
    login: (callbackUrl: string, accountName?: string, groupId?: string) =>
      invoke<OAuthLoginResult>('oauth_login', { callbackUrl, accountName, groupId }),
  },
  apiServer: {
    start: () => invoke<ApiServiceStatus>('api_server_start'),
    stop: () => invoke('api_server_stop'),
    status: () => invoke<ApiServiceStatus>('api_server_status'),
    poolList: () => invoke<ApiPoolFile>('pool_list'),
    // T10：池设置扩展调度策略与分组筛选
    poolSet: (uids: string[], strategy?: string, groupIds?: string[]) =>
      invoke('pool_set', { uids, strategy: strategy ?? null, groupIds: groupIds ?? null }),
    poolStatus: () => invoke<PoolStatus[]>('pool_status'),
    logsList: () => invoke<string[]>('api_logs_list'),
    logsDetail: (date: string) => invoke<string | null>('api_logs_detail', { date }),
    logsSearch: (opts: {
      date: string;
      startTime?: string;
      endTime?: string;
      keyword?: string;
    }) => invoke<string | null>('api_logs_search', {
      opts: {
        date: opts.date,
        start_time: opts.startTime,
        end_time: opts.endTime,
        keyword: opts.keyword,
      },
    }),
    debugToggle: () => invoke<boolean>('api_debug_toggle'),
    debugStatus: () => invoke<boolean>('api_debug_status'),
    modelsList: () => invoke<ModelOption[]>('api_models_list'),
    modelsSync: () => invoke<ModelOption[]>('api_models_sync'),
    // T1：近 N 天 API 用量统计（按日聚合，服务未运行也可查）
    usageStats: (days?: number) =>
      invoke<UsageDayView[]>('api_usage_stats', { days: days ?? null }),
    // T2：多 API Key 管理（统一列表，无主/子之分）
    keysList: () => invoke<ApiKeyEntry[]>('api_keys_list'),
    keysSave: (keys: ApiKeyEntry[]) => invoke('api_keys_save', { keys }),
  },
  updater: {
    check: () => invoke<UpdateCheckResult>('update_check'),
    // 第一步：下载安装包（完成后返回本地路径，等待用户确认安装）
    // 注意：key 必须是 expectedVersion（Rust 参数 expected_version 的 Tauri 驼峰匹配），传 version 会报 missing required key
    download: (p: { downloadUrl: string; assetName: string; expectedVersion: string }) =>
      invoke<UpdateDownloaded>('update_download', p),
    // 第二步：启动安装器（/P /UPDATE /R，完成后自动重启应用）
    runInstaller: (p: { filePath: string; assetName: string }) =>
      invoke<void>('update_run_installer', p),
    onDownloadProgress: async (
      cb: (e: UpdateDownloadProgress) => void,
    ): Promise<UnlistenFn> =>
      listen<UpdateDownloadProgress>('update-download-progress', (ev) =>
        cb(ev.payload),
      ),
    onInstalling: async (cb: (assetName: string) => void): Promise<UnlistenFn> =>
      listen<string>('update-installing', (ev) => cb(ev.payload)),
  },
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
  error_type?: string | null;
  cooldown_until?: number | null;
}
export interface CheckinDoneEvent {
  type: 'done';
  ok: number;
  already: number;
  failed: number;
  total?: number;
}
/** 失败自动重试倒计时事件（T5） */
export interface CheckinRetryEvent {
  type: 'retry';
  round: number;
  delay: number;
  total: number;
}
export type CheckinProgressEvent =
  | CheckinStartEvent
  | CheckinAccountEvent
  | CheckinRetryEvent
  | CheckinDoneEvent;

export interface SwitchDoneEvent {
  success: boolean;
  raw: string;
}

export interface SaveLoginDoneEvent {
  success: boolean;
  raw: string;
}

export interface DeviceResetDoneEvent {
  success: boolean;
  raw: string;
}

export interface ProfileDoneEvent {
  success: boolean;
  raw: string;
  action: 'backup' | 'restore';
}

export interface ListenerHandlers {
  onProxyLog?: (line: string) => void;
  onAccountCaptured?: (uid: string) => void;
  onCheckinProgress?: (e: CheckinProgressEvent) => void;
  onSwitchProgress?: (line: string) => void;
  onSwitchDone?: (e: SwitchDoneEvent) => void;
  onSaveLoginProgress?: (line: string) => void;
  onSaveLoginDone?: (e: SaveLoginDoneEvent) => void;
  onDeviceResetProgress?: (line: string) => void;
  onDeviceResetDone?: (e: DeviceResetDoneEvent) => void;
  onProfileProgress?: (line: string) => void;
  onProfileDone?: (e: ProfileDoneEvent) => void;
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
  if (handlers.onSwitchDone) {
    unsubs.push(
      await listen<SwitchDoneEvent>('switch-done', (e) =>
        handlers.onSwitchDone!(e.payload),
      ),
    );
  }
  if (handlers.onSaveLoginProgress) {
    unsubs.push(
      await listen<string>('save-login-progress', (e) =>
        handlers.onSaveLoginProgress!(e.payload),
      ),
    );
  }
  if (handlers.onSaveLoginDone) {
    unsubs.push(
      await listen<SaveLoginDoneEvent>('save-login-done', (e) =>
        handlers.onSaveLoginDone!(e.payload),
      ),
    );
  }
  if (handlers.onDeviceResetProgress) {
    unsubs.push(
      await listen<string>('device-reset-progress', (e) =>
        handlers.onDeviceResetProgress!(e.payload),
      ),
    );
  }
  if (handlers.onDeviceResetDone) {
    unsubs.push(
      await listen<DeviceResetDoneEvent>('device-reset-done', (e) =>
        handlers.onDeviceResetDone!(e.payload),
      ),
    );
  }
  if (handlers.onProfileProgress) {
    unsubs.push(
      await listen<string>('profile-progress', (e) =>
        handlers.onProfileProgress!(e.payload),
      ),
    );
  }
  if (handlers.onProfileDone) {
    unsubs.push(
      await listen<ProfileDoneEvent>('profile-done', (e) =>
        handlers.onProfileDone!(e.payload),
      ),
    );
  }
  return unsubs;
}
