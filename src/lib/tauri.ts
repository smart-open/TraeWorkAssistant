import { invoke as tauriInvoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type {
  AccountView,
  ApiServiceStatus,
  BuddyAiAccountView,
  BuddyAiUsageOfficialAll,
  BuddyAiTokenStats,
  ApiPoolFile,
  ApiKeyEntry,
  CcSwitchStatus,
  ApiKeysFileView,
  SanitizeMapView,
  SanitizeRule,
  SanitizeSwitches,
  SanitizeTier,
  ToolAliasMapView,
  FingerprintSample,
  AppLocate,
  CheckinDone,
  CheckinOpts,
  CheckinTrendPoint,
  DeviceConflictGroup,
  CodeBuddyEnvCheck,
  CreditRecord,
  CreditDetail,
  CreditsDailySnapshot,
  DiscoveredAccount,
  DoubaoAccountView,
  DoubaoCapturedCredential,
  DoubaoChatdataInfo,
  DoubaoChatdataResult,
  DoubaoExportResult,
  DoubaoRenewSummary,
  DoubaoQuotaResult,
  DoubaoHistoryEvent,
  DoubaoSnapshotMeta,
  EnvStatus,
  GroupView,
  JwtParseResult,
  AppEntitlement,
  TraeWorkMigrationReport,
  ImportReport,
  ImportPreview,
  LogLine,
  ModelOption,
  OAuthLoginUrl,
  OAuthLoginResult,
  OAuthLoginDoneEvent,
  PoolStatus,
  ProfileInfo,
  ProxyLogListResult,
  ProxyStatus,
  Settings,
  TagView,
  UpdateCheckResult,
  UpdateDownloaded,
  UpdateDownloadProgress,
  GatewaySettings,
  LanIfaceIp,
  DispatchPolicy,
  TraeModelMeta,
  UnifiedModel,
  UsageDayView,
  CustomModel,
  WorkBuddyAccountView,
  WorkBuddyEnvCheck,
  WorkBuddyScanResult,
  WorkBuddySettings,
  WbCreditsResult,
  WbCreditsSnapshot,
  WbCheckinRecord,
  WbCliStatus,
  WbCliRotateResult,
  WbCliRotateLog,
  WbOauthDone,
  WbOauthProgress,
  WbResetItem,
  WbResetResult,
  WbTokenStats,
  BuddyChatApp,
  WbUsageOfficialAll,
  WbUsageFallback,
  WbActivityInfo,
  WbPoolImportResult,
  WbModelInfo,
  UsageHistoryResult,
  UsageSessionView,
  UsageSessionsResult,
  TraeExpiredEntsResult,
  QoderAccountView,
  QoderCheckinRecord,
  QoderCliStatus,
  QoderLiveLogins,
  QoderCreditsResult,
  QoderCreditsSnapshot,
  QoderEnvCheck,
  QoderIdeScanResult,
  QoderOauthDone,
  QoderOauthProgress,
  QoderPoolExport,
  QoderPoolImportResult,
  QoderResetItem,
  QoderResetResult,
  QoderSettings,
} from '../types';

// ---- F1/F11：invoke 统一超时 + 错误源头翻译 ----
// Tauri IPC 的 Promise 无法中止（invoke 无 AbortSignal 支持），用 Promise.race 竞速超时：
// 超时后外层 Promise 以可读文案 reject（底层调用若最终完成其结果被丢弃），避免按钮 loading 永久转圈。
const INVOKE_TIMEOUT_MS = 120_000;
/** 长耗时命令覆盖表：null = 不设超时（后端有意不限时），数值 = 放宽后的毫秒上限 */
const INVOKE_TIMEOUT_OVERRIDES: Record<string, number | null> = {
  // 大文件安装包下载：后端注释明示「大文件慢速下载不能被整体超时掐断」，前端同步豁免
  update_download: null,
  // 多账号串行网络操作：耗时随账号数线性增长，放宽到 10 分钟
  doubao_renew_run: 600_000,
  workbuddy_env_reset: 600_000,
  workbuddy_cli_rotate_run: 600_000,
  workbuddy_usage_official_all: 600_000,
  workbuddy_credits_fetch: 600_000,
  // 全量账号积分刷新：逐账号串行网络请求（单请求内部超时即 120s），耗时随账号数增长，放宽到 10 分钟
  refresh_remaining_credits: 600_000,
  // 证书安装：PowerShell RunAs 触发 UAC，等待用户点击无时长上限（取消/失败会立即返回错误）
  cert_install: null,
  // OAuth 回调交换：后端最多 5 个协议变体串行兜底探测（单请求内部超时 120s），
  // 最坏总耗时可达 600s，默认 120s 会让前端先报超时而后端仍在交换，放宽到 11 分钟。
  // oauth_login 内部调用同一套 oauth_parse_callback 交换逻辑（手动粘贴登录的实际链路），
  // 两者须同放宽，否则覆盖形同虚设
  oauth_parse_callback: 660_000,
  oauth_login: 660_000,
};

/** F11：底层错误在源头翻译为可读文案——剥冗余 "Error: " 前缀、网络类错误转人话；
 *  全库 catch 均 String(err) 展示，此处统一处理后各处文案自动受益 */
function translateInvokeError(err: unknown, cmd: string): string {
  let msg = typeof err === 'string' ? err : err instanceof Error ? err.message : String(err);
  msg = msg.replace(/^Error:\s*/, '');
  if (/failed to fetch|networkerror|network error|load failed|err_connection/i.test(msg)) {
    return '网络连接失败，请检查网络后重试';
  }
  if (!msg.trim()) return `命令 ${cmd} 调用失败（无错误详情）`;
  return msg;
}

/** 全库唯一 invoke 入口：api.* 各命令均经此发出（titlebar 窗口控制等非后端命令除外） */
async function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  const timeout = Object.prototype.hasOwnProperty.call(INVOKE_TIMEOUT_OVERRIDES, cmd)
    ? INVOKE_TIMEOUT_OVERRIDES[cmd]
    : INVOKE_TIMEOUT_MS;
  try {
    if (timeout === null) return await tauriInvoke<T>(cmd, args);
    // 竞速超时；以字符串 reject 保证 String(err) 无 "Error: " 前缀
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      return await Promise.race([
        tauriInvoke<T>(cmd, args),
        new Promise<never>((_, reject) => {
          timer = setTimeout(
            () => reject(`命令 ${cmd} 超过 ${Math.round(timeout / 1000)} 秒未响应，已中止等待；请重试，若持续失败请重启应用`),
            timeout,
          );
        }),
      ]);
    } finally {
      if (timer) clearTimeout(timer);
    }
  } catch (err) {
    throw translateInvokeError(err, cmd);
  }
}

// 所有 invoke 封装集中于此，字段名严格遵循 Rust 端 snake_case 约定。
export const api = {
  /** F-75 M0-0.5：平台标志（os/arch），init 时拉取一次存 store.platform */
  platform: {
    info: () => invoke<{ os: 'windows' | 'macos'; arch: string }>('platform_info'),
  },
  env: {
    // TraeWork 合并后唯一客户端检测（原 env_check 已删，统一走 Trae CN 通道）
    check: () => invoke<EnvStatus>('env_check_trae_cn'),
    // F-01：跨应用安装位置自动识别（trae | doubao | workbuddy | codebuddy；
    // qoder 系客户端定位在 env.rs 的 qoder 专用通道，不走 app_locate）
    locate: (targetApp?: string) => invoke<AppLocate>('app_locate', { targetApp }),
    openSite: () => invoke('open_trae_website'),
    openApp: (proxyPort?: number) => invoke('open_trae_cn_app', { proxyPort }),
    /** Buddy 双应用：打开 WorkBuddy 桌面客户端（代理运行中传端口注入 --proxy-server 抓包） */
    openWorkbuddyApp: (proxyPort?: number) => invoke('open_workbuddy_app', { proxyPort }),
    /** Buddy 双应用：打开 CodeBuddy 桌面客户端（代理注入同上） */
    openCodebuddyApp: (proxyPort?: number) => invoke('open_codebuddy_app', { proxyPort }),
    /** BuddyAI 国际客户端：打开 WorkBuddy AI（Programs\WorkBuddyAI，独立探测档案；代理运行中传端口注入 --proxy-server 抓包） */
    openWorkbuddyAiApp: (proxyPort?: number) => invoke('open_workbuddy_ai_app', { proxyPort }),
    /** BuddyAI 国际客户端：打开 CodeBuddy AI（Programs\CodeBuddy；代理注入同上） */
    openCodebuddyAiApp: (proxyPort?: number) => invoke('open_codebuddy_ai_app', { proxyPort }),
    /** CodeBuddy 桌面环境检测（exe/进程/auth uid；与 WorkBuddy 共享 auth 文件） */
    codebuddyEnvCheck: () => invoke<CodeBuddyEnvCheck>('codebuddy_env_check'),
  },
  cert: {
    status: () => invoke<{ installed: boolean; ca_exists: boolean }>('cert_status'),
    install: () => invoke<{ installed: boolean; ca_exists: boolean }>('cert_install'),
    /** 打开证书目录（资源管理器）：安装失败时引导用户手动导入 ca.cer（issue #12/#79） */
    openFolder: () => invoke('cert_open_folder'),
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
    refreshJwt: (userId: string, force = false) =>
      invoke<string>('refresh_jwt', { userId, force }),
    // issue #126：续期前置判活——JWT 服务端仍存活时 Err(出路指引)，判死/fail-open 时 Ok
    renewPreflight: (userId: string) =>
      invoke<void>('renew_jwt_preflight', { userId }),
    exportRaw: () => invoke<Record<string, unknown>>('accounts_export_raw'),
    importAccounts: (content: string, only?: number[]) =>
      invoke<ImportReport>('accounts_import', { content, only }),
    // F-46：导入前预览（解析账号/分组、标记已存在，不写盘）
    importPreview: (content: string) => invoke<ImportPreview>('accounts_import_preview', { content }),
    // F-08 双应用账号自动发现
    discover: () => invoke<DiscoveredAccount[]>('apps_accounts_discover'),
    addDiscovered: (userId: string, name: string, app: string, dcUid?: string | null, uidConfident?: boolean) =>
      invoke('apps_account_add', { userId, name, app, dcId: dcUid ?? null, uidConfident: uidConfident ?? null }),
    // 按需取完整 JWT（列表接口只回掩码值；弹窗/复制场景调用）
    getJwt: (userId: string) => invoke<string>('account_get_jwt', { userId }),
    // 会员/套餐信息
    refreshPayStatus: () => invoke<number>('refresh_pay_status'),
    // 积分消耗历史（Trae query_user_usage_group_by_session；fresh=true 增量拉取，false 纯缓存）
    usageHistory: (fresh?: boolean) =>
      invoke<UsageHistoryResult>('usage_history_fetch', { fresh: fresh ?? null }),
    // 逐包到期（issue #82 T5.1：expired_ents 实时查询，含已过期包；credit_packs 只含未过期可用包）
    traeExpiredEnts: () => invoke<TraeExpiredEntsResult>('trae_expired_ents'),
    // 会话明细（会话级行缓存查询：区间/模型/账号过滤 + 分页，纯缓存零网络）
    usageSessions: (opts: {
      start: string;
      end: string;
      model?: string | null;
      userId?: string | null;
      page?: number;
      pageSize?: number;
    }) =>
      invoke<UsageSessionsResult>('usage_sessions_query', {
        start: opts.start,
        end: opts.end,
        model: opts.model ?? null,
        userId: opts.userId ?? null,
        page: opts.page ?? null,
        pageSize: opts.pageSize ?? null,
      }),
  },
  traeApps: {
    localEntitlement: () => invoke<AppEntitlement>('apps_entitlement_read'),
  },
  groups: {
    list: () => invoke<GroupView[]>('groups_list'),
    create: (name: string, color: string) => invoke<string>('group_create', { name, color }),
    update: (id: string, patch: { name?: string; color?: string; order?: number }) =>
      invoke('group_update', { id, ...patch }),
    remove: (id: string) => invoke('group_delete', { id }),
    move: (userId: string, groupId: string | null) => invoke('group_move', { userId, groupId }),
    /** 共享分组列表（三池成员并集；API Key 分组绑定等跨池消费方用） */
    sharedList: () => invoke<GroupView[]>('shared_groups_list'),
  },
  tags: {
    /** 共享标签列表（uids 反查自 account_tags，跨池全局） */
    list: () => invoke<TagView[]>('tags_list'),
    create: (name: string, color: string) => invoke<string>('tags_create', { name, color }),
    update: (id: string, patch: { name?: string; color?: string; order?: number }) =>
      invoke('tags_update', { id, ...patch }),
    remove: (id: string) => invoke('tags_delete', { id }),
    /** 全量覆盖式设置账号标签绑定（uid 跨池通用） */
    setAccountTags: (uid: string, tagIds: string[]) =>
      invoke('account_tags_set', { uid, tagIds }),
    /** 全量 uid → tag_ids 映射（一次拉取，避免 N+1 invoke） */
    map: () => invoke<Record<string, string[]>>('account_tags_map'),
  },
  checkin: {
    start: (opts: CheckinOpts) => invoke('checkin_start', { opts }),
    // T8：近 N 天签到结果趋势（Dashboard 堆叠图）
    trends: (days?: number) =>
      invoke<CheckinTrendPoint[]>('checkin_trends', { days: days ?? null }),
    /** 设备冲突组（issue #110 签到前预警）：device_map 中共用同一凭证的账号组 */
    deviceConflicts: () =>
      invoke<DeviceConflictGroup[]>('checkin_device_conflicts', {}),
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
    /** 待取启动通知（一次性；后端启动期消息如分组统一迁移结果，拉取后即清） */
    startupNoticeTake: () => invoke<string | null>('startup_notice_take'),
    /** TraeWork 合并迁移升级引导：有迁移报告且未关闭过 → 返回报告；否则 null */
    traeworkMigrationGuide: () =>
      invoke<TraeWorkMigrationReport | null>('traework_migration_guide'),
    /** 关闭升级引导（后端落「已读」标记，之后启动不再展示） */
    traeworkMigrationGuideDismiss: () => invoke('traework_migration_guide_dismiss'),
    settingsGet: () => invoke<Settings>('settings_get'),
    settingsSet: (patch: Partial<Settings>) => invoke('settings_set', { patch }),
    notifyTest: () => invoke<string>('notify_test'),
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
        // 后端 ProxyLogQueryOpts 为 serde camelCase：此前发 snake_case 被静默忽略，UI 日期筛选失效
        startTime: opts.startTime,
        endTime: opts.endTime,
        offset: opts.offset,
        limit: opts.limit,
      },
    }),
    proxyLogDetail: (id: string) => invoke<string>('proxy_log_detail', { id }),
    writeTextFile: (path: string, content: string) =>
      invoke('write_text_file', { path, content }),
    readTextFile: (path: string) => invoke<string>('read_text_file', { path }),
  },
  switchAccount: (
    userId: string,
    targetApp?: 'Trae' | 'Doubao' | 'WorkBuddy' | 'CodeBuddy' | 'WorkBuddyAI' | 'CodeBuddyAI' | 'Qoder' | 'QoderWork',
    skipJwtProbe?: boolean,
    proxyPort?: number,
  ) =>
    invoke('switch_account', {
      userId,
      targetApp: targetApp ?? null,
      // 续期 JWT 场景目标账号 JWT 本就可能已吊销，跳过切换前预检避免拦死续期链路
      skipJwtProbe: skipJwtProbe ?? false,
      // 续期捕获场景传代理端口：切换后 TRAE 以 --proxy-server 重启，客户端流量走 MITM
      proxyPort: proxyPort ?? null,
    }),
  saveCurrentLogin: (userId: string, targetApp?: 'Trae' | 'Doubao' | 'WorkBuddy' | 'CodeBuddy' | 'WorkBuddyAI' | 'CodeBuddyAI' | 'Qoder' | 'QoderWork') =>
    invoke('save_current_login', { userId, targetApp: targetApp ?? null }),
  resetDeviceIds: (targetApp?: 'Trae') =>
    invoke('reset_device_ids', { targetApp: targetApp ?? null }),
  profiles: {
    // Buddy 双应用：profile_list / profile_restore / profile_delete 支持 WorkBuddy / CodeBuddy 档案映射
    // Qoder：M3 Icube 档案（data/profiles_qoder；切号快照保存/恢复）
    // QoderWork：2026-10-02 Qoder Work 独立客户端档案（data/profiles_qoder_work；electron-root 布局）
    // fresh=true 绕过体积缓存强制重算（「刷新列表」按钮用；默认命中缓存，避免每次开弹窗遍历快照目录）
    list: (
      targetApp?: 'Trae' | 'Doubao' | 'WorkBuddy' | 'CodeBuddy' | 'Qoder' | 'QoderWork',
      fresh?: boolean,
    ) => invoke<ProfileInfo[]>('profile_list', { targetApp: targetApp ?? null, fresh: fresh ?? null }),
    backup: (userId: string, targetApp?: 'Trae' | 'Doubao' | 'Qoder' | 'QoderWork') =>
      invoke('profile_backup', { userId, targetApp: targetApp ?? null }),
    restore: (userId: string, targetApp?: 'Trae' | 'Doubao' | 'WorkBuddy' | 'CodeBuddy' | 'Qoder' | 'QoderWork') =>
      invoke('profile_restore', { userId, targetApp: targetApp ?? null }),
    delete: (userId: string, targetApp?: 'Trae' | 'Doubao' | 'WorkBuddy' | 'CodeBuddy' | 'Qoder' | 'QoderWork') =>
      invoke('profile_delete', { userId, targetApp: targetApp ?? null }),
    formatSize: (bytes: number) => invoke<string>('profile_format_size', { bytes }),
  },
  // ---- 豆包账号池（Rust doubao.rs；字段名严格 snake_case）----
  doubao: {
    accountsList: () => invoke<DoubaoAccountView[]>('doubao_accounts_list'),
    accountSave: (userId: string, name?: string, note?: string) =>
      invoke('doubao_account_save', { userId, name: name ?? null, note: note ?? null }),
    accountRemove: (userId: string) => invoke('doubao_account_remove', { userId }),
    detectUid: () => invoke<string | null>('doubao_detect_uid'),
    launch: (proxyPort?: number) => invoke('open_doubao_app', { proxyPort: proxyPort ?? null }),
    /** C1：一键以账号打开（恢复快照后拉起客户端；代理运行中时注入 --proxy-server） */
    openAs: (userId: string, proxyPort?: number) =>
      invoke('doubao_open_as_account', { userId, proxyPort: proxyPort ?? null }),
    /** C3：快照版本元数据（旧版快照返回 schema_version=0 或 null） */
    snapshotMeta: (userId: string) =>
      invoke<DoubaoSnapshotMeta | null>('doubao_snapshot_meta', { userId }),
    /** 运维历史（keepalive/renew/quota 事件，旧→新；健康度卡与额度趋势数据源） */
    history: () => invoke<DoubaoHistoryEvent[]>('doubao_history'),
    // ---- 额度定时巡检任务（A1/B4） ----
    quotaTaskRegister: (time: string) => invoke('doubao_quota_task_register', { time }),
    quotaTaskStatus: () => invoke<string>('doubao_quota_task_status'),
    quotaTaskUnregister: () => invoke('doubao_quota_task_unregister'),
    // ---- 会话凭证（代理自动抓包） ----
    capturedCredential: () => invoke<DoubaoCapturedCredential | null>('doubao_captured_credential'),
    /** 抓包凭证自动回写当前账号（幂等）；返回写入说明或 null（无凭证/无目标/内容未变） */
    credentialAutoApply: () => invoke<string | null>('doubao_credential_auto_apply'),
    // ---- 会话续期 ----
    renewRun: (syncOnly?: boolean) =>
      invoke<DoubaoRenewSummary>('doubao_renew_run', { syncOnly: syncOnly ?? false }),
    keepaliveRun: () => invoke('doubao_keepalive_run'),
    accountSetCredential: (userId: string, sessionId?: string, sidGuard?: string, ttwid?: string) =>
      invoke('doubao_account_set_credential', {
        userId,
        sessionId: sessionId ?? null,
        sidGuard: sidGuard ?? null,
        ttwid: ttwid ?? null,
      }),
    // ---- D1 对话数据（客户端状态）独立备份/恢复 ----
    /** 备份对话数据：IndexedDB / DoubaoStorage → data/doubao_chats/<uid>/（自动先关豆包） */
    chatdataBackup: (userId: string) => invoke<DoubaoChatdataResult>('doubao_chatdata_backup', { userId }),
    /** 恢复对话数据备份到豆包 User Data（自动先关豆包） */
    chatdataRestore: (userId: string) => invoke<DoubaoChatdataResult>('doubao_chatdata_restore', { userId }),
    /** 对话数据备份状态（backed / files / size_bytes / backed_at） */
    chatdataInfo: (userId: string) => invoke<DoubaoChatdataInfo>('doubao_chatdata_info', { userId }),
    // ---- D2 对话记录导出（官方 API 拉取 → markdown/json） ----
    /** 导出对话记录：需要账号已录入凭证（sessionid/sid_guard/ttwid），输出到 data/exports/ */
    exportChats: (userId: string) => invoke<DoubaoExportResult>('doubao_export_chats', { userId }),
    taskRegister: (time: string) => invoke('doubao_renew_task_register', { time }),
    taskStatus: () => invoke<string>('doubao_renew_task_status'),
    taskUnregister: () => invoke('doubao_renew_task_unregister'),
    // ---- 会员额度 ----
    fetchQuota: (userId: string) => invoke<DoubaoQuotaResult>('doubao_quota_fetch', { userId }),
    // 按需取完整会话凭证（列表接口只回掩码值；编辑弹框回填场景调用）
    getCredential: (userId: string) =>
      invoke<{ session_id: string | null; sid_guard: string | null; ttwid: string | null }>(
        'doubao_account_get_credential',
        { userId },
      ),
  },
  // ---- Buddy 国际版（buddy-ai 独立池；Rust commands/workbuddy/buddy_ai.rs）----
  buddyai: {
    /** buddy-ai OAuth 扫码登录（workbuddy.ai + platform=workbuddy-ai）；
     *  事件 buddy-ai-oauth-progress / buddy-ai-oauth-done（与 wb-oauth 同构、独立前缀） */
    oauthLogin: () => invoke<void>('buddy_ai_oauth_login'),
    accountsList: () => invoke<BuddyAiAccountView[]>('buddy_ai_accounts_list'),
    /** 手工导入国际账号凭证（区域必选校验：domain 须含 .workbuddy.ai） */
    accountImportCreds: (name?: string, accessToken?: string, refreshToken?: string, domain?: string) =>
      invoke<{ id: string; domain: string }>('buddy_ai_account_import_creds', {
        name: name ?? null,
        accessToken: accessToken ?? null,
        refreshToken: refreshToken ?? null,
        domain: domain ?? null,
      }),
    accountRemove: (userId: string) => invoke('buddy_ai_account_remove', { userId }),
    /** 凭证续期（R-2a 实测端点，rt 轮换回写；失败按暂态不标 needs_relogin） */
    refreshToken: (userId: string) => invoke<string>('buddy_ai_refresh_token', { userId }),
    /** 每日活跃打卡（手动触发；进度经 buddy-ai-checkin-progress NDJSON 推送） */
    checkinStart: () => invoke<void>('buddy_ai_checkin_start'),
    /** 打卡结果按日查询（近 N 天，默认 30；概述趋势/签到日历共用） */
    checkinResults: (days?: number) =>
      invoke<WbCheckinRecord[]>('buddy_ai_checkin_results', { days: days ?? null }),
    /** 编辑账号（昵称/备注/分组；P3 对齐 workbuddy_account_save） */
    accountSave: (userId: string, patch: { name?: string; note?: string; group_id?: string }) =>
      invoke<void>('buddy_ai_account_save', {
        userId,
        name: patch.name ?? null,
        note: patch.note ?? null,
        groupId: patch.group_id ?? null,
      }),
    /** 账号启停（停用即退出打卡花名册与网关取号） */
    accountToggle: (userId: string, disabled: boolean) =>
      invoke<void>('buddy_ai_account_toggle', { userId, disabled }),
    /** 官网请求流水（P9：get-user-request-usage 分页，credit/model/client/requestTime/inputTrunc） */
    requestUsage: (userId: string, days?: number, page?: number, pageSize?: number) =>
      invoke<{
        total: number;
        page: number;
        pageSize: number;
        rows: Array<{
          requestId: string;
          credit: number;
          model: string;
          client: string;
          requestTime: string;
          inputTrunc: string;
          agentPurpose: string;
        }>;
      }>('buddy_ai_request_usage', {
        userId,
        days: days ?? null,
        page: page ?? null,
        pageSize: pageSize ?? null,
      }),
    /** 积分历史查询（P8 趋势曲线数据源；同日覆盖快照，默认 90 天） */
    creditsHistory: (days?: number) =>
      invoke<Array<{ date: string; ts: number; total_balance: number; accounts: Array<{ id: string; name: string; balance: number | null }>; earned: number | null }>>(
        'buddy_ai_credits_history',
        { days: days ?? null },
      ),
    /** 账号池导出（includeCredentials=true 时须提供 password：凭证以 AIWQENC1 信封加密写入导出文件，不回传明文） */
    accountsExport: (includeCredentials?: boolean, password?: string) =>
      invoke<Record<string, unknown>>('buddy_ai_accounts_export', {
        includeCredentials: includeCredentials ?? null,
        password: password ?? null,
      }),
    /** 账号池导入（accounts_export 回导；加密信封文件须传 password 解密，明文旧文件 password 传 null 兼容） */
    accountsImport: (poolJson: Record<string, unknown>, password?: string | null) =>
      invoke<{ added: number; skipped: number; invalid: number; tokens_added: number }>(
        'buddy_ai_accounts_import',
        { poolJson, password: password ?? null },
      ),
    /** 扫描本机 WorkBuddy AI 客户端登录账号（account-snapshot.json；识别+池内匹配） */
    localScan: () =>
      invoke<{
        found: boolean;
        reason?: string;
        uid?: string;
        nickname?: string;
        edition_type?: string;
        is_pro?: boolean;
        saved_at_ms?: number;
        in_pool?: boolean;
        pool_account_id?: string;
      }>('buddy_ai_local_scan'),
    /** 环境重置（白名单目录清理；items 含 login = 登录态清除，前端强确认） */
    envReset: (items: string[]) =>
      invoke<{ results: { item: string; ok: boolean; detail: string }[]; failed: number }>(
        'buddy_ai_env_reset',
        { items },
      ),
    /** 打开 WorkBuddy AI 国际客户端数据目录（%USERPROFILE%\.workbuddy-ai） */
    openDataDir: () => invoke<void>('buddy_ai_open_data_dir'),
    /** 手动同步 Intl 模型目录（/v3/config 直连 → 动态替换；返回替换模型数） */
    catalogSync: () => invoke<number>('buddy_ai_catalog_sync'),
    /** 积分同步（get-user-resource TotalDosage → 池快照三键回写），返回刷新后账号视图 */
    creditsFetch: () => invoke<BuddyAiAccountView[]>('buddy_ai_credits_fetch'),
    /** 全账号官网用量聚合（积分看板「官网」源；31 天零填充，10min 缓存 + stale-on-error） */
    usageOfficialAll: () => invoke<BuddyAiUsageOfficialAll>('buddy_ai_usage_official_all'),
    /** 本地 Token 统计（~/.workbuddy-ai/projects 会话 jsonl 扫描；fresh=true 跳过 10min 结果缓存） */
    tokenStats: (fresh?: boolean) =>
      invoke<BuddyAiTokenStats>('buddy_ai_token_stats', { fresh: fresh ?? null }),
    /** 会话明细（定时落地的官网请求级行缓存，纯缓存零网络；镜像国内 workbuddy.usageSessions） */
    usageSessions: (opts: {
      start: string;
      end: string;
      model?: string | null;
      userId?: string | null;
      page?: number;
      pageSize?: number;
    }) =>
      invoke<UsageSessionsResult>('buddy_ai_sessions_query', {
        start: opts.start,
        end: opts.end,
        model: opts.model ?? null,
        userId: opts.userId ?? null,
        page: opts.page ?? null,
        pageSize: opts.pageSize ?? null,
      }),
  },
  // ---- WorkBuddy（批次1；Rust workbuddy.rs；字段名严格 snake_case）----
  workbuddy: {
    envCheck: () => invoke<WorkBuddyEnvCheck>('workbuddy_env_check'),
    accountsList: () => invoke<WorkBuddyAccountView[]>('workbuddy_accounts_list'),
    accountSave: (userId: string, name?: string, note?: string) =>
      invoke('workbuddy_account_save', { userId, name: name ?? null, note: note ?? null }),
    accountMove: (userId: string, groupId: string | null) =>
      invoke('workbuddy_account_move', { userId, groupId: groupId ?? null }),
    groups: {
      list: () => invoke<GroupView[]>('workbuddy_groups_list'),
      create: (name: string, color: string) =>
        invoke<string>('workbuddy_groups_create', { name, color }),
      update: (id: string, patch: { name?: string; color?: string; order?: number }) =>
        invoke('workbuddy_groups_update', {
          id,
          name: patch.name ?? null,
          color: patch.color ?? null,
          order: patch.order ?? null,
        }),
      remove: (id: string) => invoke('workbuddy_groups_remove', { id }),
    },
    accountRemove: (userId: string, deleteSnapshot?: boolean) =>
      invoke('workbuddy_account_remove', { userId, deleteSnapshot: deleteSnapshot ?? null }),
    scanAuthFile: () => invoke<WorkBuddyScanResult | null>('workbuddy_scan_auth_file'),
    accountImportAuth: (name?: string) =>
      invoke<WorkBuddyAccountView>('workbuddy_account_import_auth', { name: name ?? null }),
    refreshToken: (userId: string, force = false) =>
      invoke<string>('workbuddy_refresh_token', { userId, force }),
    checkinStart: (opts: { user_ids?: string[]; skip_checked_in: boolean; skip_expired: boolean }) =>
      invoke('workbuddy_checkin_start', { opts }),
    growthRun: () => invoke('workbuddy_growth_run'),
    checkinResults: (days?: number) =>
      invoke<WbCheckinRecord[]>('workbuddy_checkin_results', { days: days ?? null }),
    checkinTaskRegister: (times: string[]) =>
      invoke('workbuddy_checkin_task_register', { times }),
    checkinTaskStatus: () => invoke<string[]>('workbuddy_checkin_task_status'),
    checkinTaskUnregister: () => invoke('workbuddy_checkin_task_unregister'),
    renewTaskRegister: (day?: string, time?: string) =>
      invoke('workbuddy_renew_task_register', { day: day ?? null, time: time ?? null }),
    renewTaskStatus: () => invoke<boolean>('workbuddy_renew_task_status'),
    renewTaskUnregister: () => invoke('workbuddy_renew_task_unregister'),
    creditsFetch: (userId?: string, fresh?: boolean) =>
      invoke<WbCreditsResult>('workbuddy_credits_fetch', { userId: userId ?? null, fresh: fresh ?? null }),
    // 快照时序读取（credits-dashboard-plan.md §2.2 方案 B：earned 已随快照落库）
    creditsHistoryList: () =>
      invoke<{ snapshots: WbCreditsSnapshot[] }>('workbuddy_credits_history_list'),
    editionsBackfill: () => invoke<number>('workbuddy_editions_backfill'),
    settingsGet: () => invoke<WorkBuddySettings>('workbuddy_settings_get'),
    settingsSet: (patch: WorkBuddySettings) => invoke('workbuddy_settings_set', { patch }),
    // 打开 auth 文件所在目录（资源管理器；人工覆盖路径优先）
    openAuthDir: () => invoke('workbuddy_open_auth_dir'),
    // UI 坐标点击签到兜底（F-18，批次4）：仅手动触发、默认关闭
    uiClickCapture: () => invoke<{ ok: boolean; x: number; y: number; message: string }>('workbuddy_ui_click_capture'),
    uiClickCheckin: () => invoke<{ ok: boolean; x: number; y: number; message: string }>('workbuddy_ui_click_checkin'),
    // CLI 切号桥 + 五重防护轮换（F-06/F-59，批次3）
    cliStatus: () => invoke<WbCliStatus>('workbuddy_cli_status'),
    cliBridgeSet: (userId: string) => invoke<WbCliStatus>('workbuddy_cli_bridge_set', { userId }),
    cliRotateRun: () => invoke<WbCliRotateResult>('workbuddy_cli_rotate_run'),
    cliRotateLogs: (limit?: number) =>
      invoke<WbCliRotateLog[]>('workbuddy_cli_rotate_logs', { limit: limit ?? null }),
    // 会话三件套备份/恢复 + 复制迁移（F-44/F-45，批次3）
    // F-74：app 决定会话域（~/.workbuddy / ~/.codebuddy）与备份根；省略 = WorkBuddy（旧行为）
    chatdataBackup: (userId: string, app?: BuddyChatApp) =>
      invoke<{ ok: boolean; files: number; path: string }>('workbuddy_chatdata_backup', {
        userId,
        app: app ?? 'WorkBuddy',
      }),
    chatdataRestore: (userId: string, app?: BuddyChatApp) =>
      invoke<{ ok: boolean; files: number }>('workbuddy_chatdata_restore', { userId, app: app ?? 'WorkBuddy' }),
    chatdataInfo: (userId: string, app?: BuddyChatApp) =>
      invoke<{ backed: boolean; size_bytes?: number; files?: number; backed_at?: string; has_edge_mapping?: boolean }>(
        'workbuddy_chatdata_info',
        { userId, app: app ?? 'WorkBuddy' },
      ),
    chatdataCopy: (sourceUserId: string, targetUserId: string, app?: BuddyChatApp) =>
      invoke<{ ok: boolean; copied: number; total_lines: number; sessions_cloned: number; mappings_registered: number }>(
        'workbuddy_chatdata_copy',
        { sourceUserId, targetUserId, app: app ?? 'WorkBuddy' },
      ),
    // 账号库导入导出扩展（F-46，批次3）
    accountsExport: (includeCredentials?: boolean) =>
      invoke<Record<string, unknown>>('workbuddy_accounts_export', { includeCredentials: includeCredentials ?? null }),
    accountsImport: (payload: Record<string, unknown>) =>
      invoke<WbPoolImportResult>('workbuddy_accounts_import', { payload }),
    // OAuth 扫码 + 环境重置（F-50/F-14，批次3）
    oauthLogin: () => invoke<void>('workbuddy_oauth_login'),
    envResetItems: () => invoke<WbResetItem[]>('workbuddy_env_reset_items'),
    envReset: (items: string[], keycloakLogout: boolean) =>
      invoke<WbResetResult[]>('workbuddy_env_reset', { items, keycloakLogout }),
    // 官方用量 + 本地 Token 统计（F-25/26/57/58，批次3）
    // 注意：Rust 端参数名为 refresh（Option<bool>），key 必须写 refresh；此前误写 fresh 被静默忽略导致「刷新」永远走缓存
    /** 全账号官方用量聚合（近 7 日积分消耗主数据源；31 天零填充，含按模型汇总） */
    usageOfficialAll: () => invoke<WbUsageOfficialAll>('workbuddy_usage_official_all'),
    usageFallback: () => invoke<WbUsageFallback>('workbuddy_usage_fallback'),
    // fresh=true 强制重扫（跳过 10 分钟结果缓存；按文件增量缓存仍生效）
    tokenStats: (fresh?: boolean) =>
      invoke<WbTokenStats>('workbuddy_token_stats', { fresh: fresh ?? null }),
    activityInfo: (userId?: string, fresh?: boolean) =>
      invoke<WbActivityInfo>('workbuddy_activity_info', { userId: userId ?? null, refresh: fresh ?? null }),
    // Buddy 会话明细（官网请求级明细行缓存查询；上游无 token/会话说明字段 → 前端留空）
    usageSessions: (opts: {
      start: string;
      end: string;
      model?: string | null;
      userId?: string | null;
      page?: number;
      pageSize?: number;
    }) =>
      invoke<UsageSessionsResult>('workbuddy_sessions_query', {
        start: opts.start,
        end: opts.end,
        model: opts.model ?? null,
        userId: opts.userId ?? null,
        page: opts.page ?? null,
        pageSize: opts.pageSize ?? null,
      }),
  },
  // ---- Qoder（F-80；Rust commands/qoder；字段名严格 snake_case）----
  qoder: {
    envCheck: () => invoke<QoderEnvCheck>('qoder_env_check'),
    /** 打开客户端（后端 spawn；未检测到 exe 时 reject 文案含「未检测到」） */
    openIde: () => invoke('qoder_open_ide'),
    openWork: () => invoke('qoder_open_work'),
    accountsList: () => invoke<QoderAccountView[]>('qoder_accounts_list'),
    accountSave: (userId: string, name?: string, note?: string) =>
      invoke('qoder_account_save', { userId, name: name ?? null, note: note ?? null }),
    accountRemove: (userId: string) => invoke('qoder_account_remove', { userId }),
    /** 移动账号到分组（groupId=null 回落「未分组」；对齐 Buddy 账号分组） */
    accountMove: (userId: string, groupId: string | null) =>
      invoke('qoder_account_move', { userId, groupId: groupId ?? null }),
    groups: {
      list: () => invoke<GroupView[]>('qoder_groups_list'),
      create: (name: string, color: string) =>
        invoke<string>('qoder_groups_create', { name, color }),
      update: (id: string, patch: { name?: string; color?: string; order?: number }) =>
        invoke('qoder_groups_update', {
          id,
          name: patch.name ?? null,
          color: patch.color ?? null,
          order: patch.order ?? null,
        }),
      remove: (id: string) => invoke('qoder_groups_remove', { id }),
    },
    /** PAT 手工导入（M1 最可靠凭证通道；qoder.com.cn/account/integrations 自建，pt- 前缀）。pat 后端必填 */
    accountImportPat: (name: string | undefined, pat: string) =>
      invoke<QoderAccountView>('qoder_account_import_pat', { name: name ?? null, pat }),
    /** 单账号凭证续期（手动按钮，force 恒刷；成功返回续期后的最新账号视图） */
    accountRefreshToken: (accountId: string) =>
      invoke<QoderAccountView>('qoder_account_refresh_token', { accountId }),
    /** OAuth 设备流登录（浏览器授权页 + deviceToken/poll 轮询；事件 qoder-oauth-progress/done）。
     *  compat=true = 兼容模式（授权 URL 不带 client_id）：官方 client_id 常量被
     *  Qoder 轮换导致授权页「参数无效」时的降级链路；前端在授权超时后自动切换重试 */
    oauthLogin: (compat?: boolean) => invoke<void>('qoder_oauth_login', { compat: compat ?? null }),
    /** 取消进行中的 OAuth 轮询（弹框「取消授权」）：后端置标志后轮询线程自行发失败终态 */
    oauthCancel: () => invoke<void>('qoder_oauth_cancel'),
    /** IDE 存储账号发现/导入（M3；secret://aicoding.auth.userInfo DPAPI+AES-GCM 解密） */
    ideScan: () => invoke<QoderIdeScanResult>('qoder_ide_scan'),
    /** CLI 登录状态只读桥（M4；~/.qoder-cn/.qoder-app-status.json 白名单透传，无凭证） */
    cliStatus: () => invoke<QoderCliStatus>('qoder_cli_status'),
    /** 两个客户端当前实际登录的账号 id（徽标用；IDE=state.vscdb / Work=auth.v1.dat 解密） */
    liveLogins: () => invoke<QoderLiveLogins>('qoder_live_logins'),
    settingsGet: () => invoke<QoderSettings>('qoder_settings_get'),
    settingsSet: (patch: QoderSettings) => invoke('qoder_settings_set', { patch }),
    checkinStart: (opts?: { user_ids?: string[]; skip_checked_in?: boolean; lazy_hours?: number }) =>
      invoke('qoder_checkin_start', {
        opts: {
          user_ids: opts?.user_ids ?? null,
          skip_checked_in: opts?.skip_checked_in ?? true,
          lazy_hours: opts?.lazy_hours ?? null,
        },
      }),
    checkinResults: (days?: number) =>
      invoke<QoderCheckinRecord[]>('qoder_checkin_results', { days: days ?? null }),
    creditsFetch: (userId?: string, fresh?: boolean) =>
      invoke<QoderCreditsResult>('qoder_credits_fetch', { userId: userId ?? null, fresh: fresh ?? null }),
    creditsHistoryList: () =>
      invoke<{ snapshots: QoderCreditsSnapshot[] }>('qoder_credits_history_list'),
    /** Qoder 会话明细（issue #82 问题5：官网无明细接口，数据源为网关请求级本地流水 qoder_sessions） */
    usageSessions: (opts: {
      start: string;
      end: string;
      model?: string | null;
      userId?: string | null;
      page?: number;
      pageSize?: number;
    }) =>
      invoke<UsageSessionsResult>('qoder_sessions_query', {
        start: opts.start,
        end: opts.end,
        model: opts.model ?? null,
        userId: opts.userId ?? null,
        page: opts.page ?? null,
        pageSize: opts.pageSize ?? null,
      }),
    /** Qoder 会话展开明细（按会话行代表键查请求级行；键口径 = session_id ∪ preview） */
    sessionDetail: (opts: { date: string; uid: string; sessionId: string; preview: string }) =>
      invoke<UsageSessionView[]>('qoder_session_detail_query', {
        date: opts.date,
        uid: opts.uid,
        sessionId: opts.sessionId,
        preview: opts.preview,
      }),
    /**
     * 账号池导出（M4；includeCredentials=true 必须提供 password——凭证经 AES-256-GCM
     * 加密为 AIWQENC1 信封后才写入导出文件，后端对无密码含凭证导出直接拒绝）
     */
    accountsExport: (includeCredentials?: boolean, password?: string) =>
      invoke<QoderPoolExport>('qoder_accounts_export', {
        includeCredentials: includeCredentials ?? null,
        password: password ?? null,
      }),
    /**
     * 账号池导入（M4；kind 强校验 + uid 幂等原位更新 + device_profile 仅本地为空才补入）。
     * payload 为加密信封（AIWQENC1）时必须提供 password 解密；旧明文导出文件免密向后兼容
     */
    accountsImport: (payload: Record<string, unknown>, password?: string) =>
      invoke<QoderPoolImportResult>('qoder_accounts_import', { payload, password: password ?? null }),
    /** 环境重置清单（M4；8 项语义块 + 动态存在性标注） */
    envResetItems: () => invoke<QoderResetItem[]>('qoder_env_reset_items'),
    /** 环境重置执行（M4；自动关闭 Qoder CN，单项失败不中断） */
    envReset: (items: string[]) => invoke<QoderResetResult[]>('qoder_env_reset', { items }),
  },
  oauth: {
    getLoginUrl: () => invoke<OAuthLoginUrl>('oauth_get_login_url'),
    parseCallback: (callbackUrl: string) =>
      invoke('oauth_parse_callback', { callbackUrl }),
    login: (callbackUrl: string, accountName?: string, groupId?: string, expectUid?: string) =>
      invoke<OAuthLoginResult>('oauth_login', {
        callbackUrl,
        accountName,
        groupId,
        // 重新登录模式：目标账号 uid，后端校验不一致则拒绝落库
        expectUid: expectUid ?? null,
      }),
    // F-78 批次 1：本机回环监听（127.0.0.1:17388），浏览器回调自动落库收尾；
    // startLoopback 端口被占用时后端返回明确错误，前端降级为手动粘贴兜底
    startLoopback: (accountName?: string, groupId?: string, expectUid?: string) =>
      invoke<void>('oauth_start_loopback', {
        accountName: accountName ?? null,
        groupId: groupId ?? null,
        expectUid: expectUid ?? null,
      }),
    stopLoopback: () => invoke<void>('oauth_stop_loopback'),
    // 登录完成事件：回环监听器自动落库后发出；ok=false 时提示改用手动粘贴兜底
    onLoginDone: (cb: (e: OAuthLoginDoneEvent) => void): Promise<UnlistenFn> =>
      listen<OAuthLoginDoneEvent>('oauth-login-done', (ev) => cb(ev.payload)),
  },
  apiServer: {
    start: () => invoke<ApiServiceStatus>('api_server_start'),
    stop: () => invoke('api_server_stop'),
    status: () => invoke<ApiServiceStatus>('api_server_status'),
    poolList: () => invoke<ApiPoolFile>('pool_list'),
    // T10：池设置扩展调度策略与分组筛选；T5.2/T5.3/T5.5/T5.6③ 扩展 WB 开关组；
    // 任务7：wbStrategy 为 Buddy 池独立策略（空串 = 跟随 Trae 池策略）
    poolSet: (
      uids: string[],
      strategy?: string,
      groupIds?: string[],
      wbFlags?: {
        wbEnabled?: boolean;
        wbDefaultThinking?: boolean;
        wbToolExec?: boolean;
        wbBgDowngrade?: boolean;
        /** F-76④ 长上下文降档开关 */
        wbLongctxDowngrade?: boolean;
        /** F-76③ 竞速对冲阈值毫秒（0 = 关闭） */
        wbHedgeThresholdMs?: number;
        /** Trae 池账号并发上限（0 = 不限）；null/未传 = 保留原值 */
        traeAccountConcurrencyLimit?: number;
        /** Trae 池粘性 TTL 秒；null/未传 = 保留原值 */
        traePoolStickyTtlSecs?: number;
        /** Trae 池会话粘性 TTL 秒（显式 conversationId 绑定有效期）；null/未传 = 保留原值 */
        traeStickyTtlSecs?: number;
        /** Trae 池竞速对冲阈值毫秒（0 = 关闭）；null/未传 = 保留原值 */
        traeHedgeThresholdMs?: number;
        /** Buddy 池账号并发上限（0 = 不限）；null/未传 = 保留原值 */
        wbAccountConcurrencyLimit?: number;
        /** Buddy 池粘性 TTL 秒；null/未传 = 保留原值 */
        wbPoolStickyTtlSecs?: number;
        /** Buddy 会话粘性 TTL 秒；null/未传 = 保留原值 */
        wbStickyTtlSecs?: number;
        /** Qoder 池账号并发上限（0 = 不限）；null/未传 = 保留原值 */
        qoderAccountConcurrencyLimit?: number;
        /** Qoder 池粘性 TTL 秒；null/未传 = 保留原值 */
        qoderPoolStickyTtlSecs?: number;
        /** Qoder 会话粘性 TTL 秒（显式 conversationId 绑定有效期）；null/未传 = 保留原值 */
        qoderStickyTtlSecs?: number;
        /** Buddy 池入池白名单（wb- 前缀账号 id）；null/未传 = 保留原值（含旧数据迁移） */
        wbUids?: string[] | null;
        /** Buddy 池分组筛选；null/未传 = 保留原值，空数组 = 清空（不限分组） */
        wbGroupIds?: string[] | null;
        /** Qoder 上游开关（p3-3）；null/未传 = 保留原值 */
        qoderEnabled?: boolean;
        /** Qoder 竞速对冲阈值毫秒（F-80-余 v2，0 = 关闭）；null/未传 = 保留原值 */
        qoderHedgeThresholdMs?: number;
        /** Qoder 会话粘性开关（F-80-余 v2）；null/未传 = 保留原值 */
        qoderStickyEnabled?: boolean;
        /** 免费活动模型倍率覆盖开关（issue #111/#156，默认开）；null/未传 = 保留原值 */
        qoderFlashFreeRate?: boolean;
        /** Qoder 池入池白名单（qd- 账号 id）；null/未传 = 保留原值 */
        qoderUids?: string[];
        /** Qoder 池分组筛选；null/未传 = 保留原值 */
        qoderGroupIds?: string[];
        /** Qoder 池内调度策略（空串 = 跟随 Trae 池）；null/未传 = 保留原值 */
        qoderStrategy?: string;
        /** Trae 池参与调度开关（默认开）；null/未传 = 保留原值 */
        traeEnabled?: boolean;
        /** Trae 池标签筛选（共享标签 id）；null/未传 = 保留原值，空数组 = 清空 */
        traeTagIds?: string[] | null;
        /** Buddy 池标签筛选；null/未传 = 保留原值 */
        wbTagIds?: string[] | null;
        /** Qoder 池标签筛选；null/未传 = 保留原值 */
        qoderTagIds?: string[] | null;
        /** Buddy 国际版（buddy-ai）独立池开关（阶段 2，显式开启制）；null/未传 = 保留原值 */
        buddyAiEnabled?: boolean;
        /** buddy-ai 显式会话粘性开关；null/未传 = 保留原值 */
        buddyAiStickyEnabled?: boolean;
        /** buddy-ai 池粘性 TTL 秒（P6 顺延补齐）；null/未传 = 保留原值 */
        buddyAiPoolStickyTtlSecs?: number;
        /** buddy-ai 池账号并发上限（0 = 不限）；null/未传 = 保留原值 */
        buddyAiAccountConcurrencyLimit?: number;
        /** buddy-ai 会话粘性 TTL 秒；null/未传 = 保留原值 */
        buddyAiStickyTtlSecs?: number;
        /** buddy-ai 竞速对冲阈值毫秒（0 = 关闭）；null/未传 = 保留原值 */
        buddyAiHedgeThresholdMs?: number;
        /** buddy-ai 池分组筛选；null/未传 = 保留原值，空数组 = 清空（不限分组） */
        buddyAiGroupIds?: string[] | null;
        /** buddy-ai 池标签筛选；null/未传 = 保留原值，空数组 = 清空 */
        buddyAiTagIds?: string[] | null;
        /** buddy-ai 池内调度策略（空串 = 跟随 Trae 池）；null/未传 = 保留原值 */
        buddyAiStrategy?: string;
      },
      wbStrategy?: string,
    ) =>
      invoke('pool_set', {
        uids,
        strategy: strategy ?? null,
        groupIds: groupIds ?? null,
        wbGroupIds: wbFlags?.wbGroupIds ?? null,
        wbEnabled: wbFlags?.wbEnabled ?? null,
        wbDefaultThinking: wbFlags?.wbDefaultThinking ?? null,
        wbToolExec: wbFlags?.wbToolExec ?? null,
        wbBgDowngrade: wbFlags?.wbBgDowngrade ?? null,
        wbLongctxDowngrade: wbFlags?.wbLongctxDowngrade ?? null,
        wbHedgeThresholdMs: wbFlags?.wbHedgeThresholdMs ?? null,
        traeAccountConcurrencyLimit: wbFlags?.traeAccountConcurrencyLimit ?? null,
        traePoolStickyTtlSecs: wbFlags?.traePoolStickyTtlSecs ?? null,
        traeStickyTtlSecs: wbFlags?.traeStickyTtlSecs ?? null,
        traeHedgeThresholdMs: wbFlags?.traeHedgeThresholdMs ?? null,
        wbAccountConcurrencyLimit: wbFlags?.wbAccountConcurrencyLimit ?? null,
        wbPoolStickyTtlSecs: wbFlags?.wbPoolStickyTtlSecs ?? null,
        wbStickyTtlSecs: wbFlags?.wbStickyTtlSecs ?? null,
        qoderAccountConcurrencyLimit: wbFlags?.qoderAccountConcurrencyLimit ?? null,
        qoderPoolStickyTtlSecs: wbFlags?.qoderPoolStickyTtlSecs ?? null,
        qoderStickyTtlSecs: wbFlags?.qoderStickyTtlSecs ?? null,
        wbUids: wbFlags?.wbUids ?? null,
        qoderEnabled: wbFlags?.qoderEnabled ?? null,
        qoderHedgeThresholdMs: wbFlags?.qoderHedgeThresholdMs ?? null,
        qoderStickyEnabled: wbFlags?.qoderStickyEnabled ?? null,
        qoderFlashFreeRate: wbFlags?.qoderFlashFreeRate ?? null,
        qoderUids: wbFlags?.qoderUids ?? null,
        qoderGroupIds: wbFlags?.qoderGroupIds ?? null,
        qoderStrategy: wbFlags?.qoderStrategy ?? null,
        traeEnabled: wbFlags?.traeEnabled ?? null,
        traeTagIds: wbFlags?.traeTagIds ?? null,
        wbTagIds: wbFlags?.wbTagIds ?? null,
        qoderTagIds: wbFlags?.qoderTagIds ?? null,
        buddyAiEnabled: wbFlags?.buddyAiEnabled ?? null,
        buddyAiStickyEnabled: wbFlags?.buddyAiStickyEnabled ?? null,
        buddyAiPoolStickyTtlSecs: wbFlags?.buddyAiPoolStickyTtlSecs ?? null,
        buddyAiAccountConcurrencyLimit: wbFlags?.buddyAiAccountConcurrencyLimit ?? null,
        buddyAiStickyTtlSecs: wbFlags?.buddyAiStickyTtlSecs ?? null,
        buddyAiHedgeThresholdMs: wbFlags?.buddyAiHedgeThresholdMs ?? null,
        buddyAiGroupIds: wbFlags?.buddyAiGroupIds ?? null,
        buddyAiTagIds: wbFlags?.buddyAiTagIds ?? null,
        buddyAiStrategy: wbFlags?.buddyAiStrategy ?? null,
        wbStrategy: wbStrategy ?? null,
      }),
    poolStatus: () => invoke<PoolStatus[]>('pool_status'),
    /** Key 组/标签约束覆盖预览（D4/D5 实时展开；{key_id: {trae, buddy, qoder, total}}） */
    keysOrgPreview: (keys: ApiKeyEntry[]) =>
      invoke<Record<string, Record<string, number>>>('api_keys_org_preview', { keys }),
    /** WB 池实时状态（F-77⑤：含 per-account inflight 在途计数） */
    wbPoolStatus: () => invoke<PoolStatus[]>('wb_pool_status'),
    /** Qoder 池实时状态（Qoder「资源调度」页：含 per-account inflight 在途计数） */
    qoderPoolStatus: () => invoke<PoolStatus[]>('qoder_pool_status'),
    /** Buddy 国际版（buddy-ai）池实时状态（全局 API 管理·API Keys 调度配置候选） */
    buddyAiPoolStatus: () => invoke<PoolStatus[]>('buddy_ai_pool_status'),
    /** 手动同步 Qoder 模型目录（复用每日调度任务入口；返回采纳模型数） */
    qoderCatalogSync: () => invoke<number>('qoder_catalog_sync'),
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
    // T5.1/F-37：WB 上游模型目录动态替换（手动触发；网关启动时已自动做一次）
    wbCatalogSync: () => invoke<number>('api_wb_catalog_sync'),
    // WB 目录模型列表（Buddy「资源调度」页展示）
    wbCatalogList: () => invoke<WbModelInfo[]>('api_wb_catalog_list'),
    /** Buddy 国际版（buddy-ai）模型目录（§4.4 首版静态配置） */
    buddyAiCatalogList: () => invoke<WbModelInfo[]>('api_buddy_ai_catalog_list'),
    // T5.7/F-43：CC Switch 协同（注册网关 provider 条目，不自建切换器）
    // side：trae（Trae 模型网关）/ wb（WB 上游网关），两套条目互不覆盖
    ccSwitchStatus: () => invoke<CcSwitchStatus>('ccswitch_status'),
    ccSwitchRegister: (
      appType: 'claude' | 'codex',
      side: 'trae' | 'wb' = 'trae',
      apiKey?: string,
      model?: string,
    ) =>
      invoke<string>('ccswitch_register', {
        appType,
        side,
        apiKey: apiKey ?? null,
        model: model ?? null,
        port: null,
      }),
    // T1：近 N 天 API 用量统计（Trae 模型请求桶；按日聚合，服务未运行也可查）
    usageStats: (days?: number) =>
      invoke<UsageDayView[]>('api_usage_stats', { days: days ?? null }),
    // WB 上游用量统计（wb_days 桶，Buddy「资源调度」页专用，与 Trae 侧分账）
    wbUsageStats: (days?: number) =>
      invoke<UsageDayView[]>('api_wb_usage_stats', { days: days ?? null }),
    // buddy-ai 国际池用量统计（buddy_ai_days 桶，BuddyAI「积分看板·模型消耗排行」专用）
    buddyAiUsageStats: (days?: number) =>
      invoke<UsageDayView[]>('api_buddy_ai_usage_stats', { days: days ?? null }),
    // 自定义模型用量统计（custom_days 桶，API 管理·用量统计「自定义」筛选）
    customUsageStats: (days?: number) =>
      invoke<UsageDayView[]>('api_custom_usage_stats', { days: days ?? null }),
    // 自定义池会话明细（issue #149 P2：custom_sessions 请求级流水，读时两遍归并）
    customSessionsQuery: (opts: {
      start: string;
      end: string;
      model?: string | null;
      userId?: string | null;
      page?: number;
      pageSize?: number;
    }) =>
      invoke<UsageSessionsResult>('custom_sessions_query', {
        start: opts.start,
        end: opts.end,
        model: opts.model ?? null,
        userId: opts.userId ?? null,
        page: opts.page ?? null,
        pageSize: opts.pageSize ?? null,
      }),
    // 自定义池会话展开明细（session_id = conversation_key 精确匹配；mode 位 = 阶段标签）
    customSessionDetail: (opts: { date: string; uid: string; sessionId: string; preview: string }) =>
      invoke<UsageSessionView[]>('custom_session_detail_query', {
        date: opts.date,
        uid: opts.uid,
        sessionId: opts.sessionId,
        preview: opts.preview,
      }),
    // Qoder 上游用量统计（qoder_days 桶，与 Trae/WB/Custom 侧分账；上游接入前恒空）
    qoderUsageStats: (days?: number) =>
      invoke<UsageDayView[]>('api_qoder_usage_stats', { days: days ?? null }),
    // 自定义模型列表（custom_models.json，OpenAI 兼容上游直通）
    customModelsList: () => invoke<CustomModel[]>('custom_models_list'),
    // 保存自定义模型（upsert：id 空 = 新增；返回保存后的完整列表）
    customModelsSave: (model: CustomModel) =>
      invoke<CustomModel[]>('custom_models_save', { model }),
    // 按 id 删除自定义模型；返回是否确有删除
    customModelsRemove: (id: string) => invoke<boolean>('custom_models_remove', { id }),
    // 自定义模型连通性测试：发一条最小 chat 请求（max_tokens=16），成功返回摘要 / 失败返回原因
    customModelTest: (model: CustomModel) => invoke<string>('custom_model_test', { model }),
    // 自定义上游熔断状态快照（P3 可观测）：每条仍在冷却中的上游 (id, fails, cooling_secs)
    customLbStatus: () =>
      invoke<{ id: string; fails: number; cooling_secs: number }[]>('custom_lb_status'),
    // ---- 统一网关命令（Phase 1 §8.1）：统一模型目录 / 网关设置 ----
    // 顶层参数用 camelCase（availableOnly），嵌套结构体字段保持 snake_case
    unifiedModels: (availableOnly?: boolean) =>
      invoke<UnifiedModel[]>('api_unified_models', { availableOnly: availableOnly ?? null }),
    // 池间调度策略（dispatch_policy.json：strategy/priority/per_model/fallback）
    dispatchPolicyGet: () => invoke<DispatchPolicy>('dispatch_policy_get'),
    // 返回规范化后的生效值（前端展示以返回值为准）；落盘即时生效无需重启网关
    dispatchPolicySet: (policy: DispatchPolicy) =>
      invoke<DispatchPolicy>('dispatch_policy_set', { policy }),
    gatewaySettingsGet: () => invoke<GatewaySettings>('gateway_settings_get'),
    // 返回规范化后的生效值（前端展示以返回值为准）；端口改动下次启动 API 服务后生效
    gatewaySettingsSet: (settings: Partial<GatewaySettings>) =>
      invoke<GatewaySettings>('gateway_settings_set', { settings }),
    // 局域网网卡 IPv4 列表（issue #34：过滤回环/链路本地/Docker/虚拟化/代理虚拟网卡，
    // 多物理网卡多 IP；网关 0.0.0.0 监听后作为局域网接入地址展示）
    lanIfaceIps: () => invoke<LanIfaceIp[]>('lan_iface_ips'),
    // 全局模型白名单（issue #26，canonical 归一列表；空 = 不限）
    modelWhitelistGet: () => invoke<string[]>('model_whitelist_get'),
    // 返回归一后的生效值（canonical + 去空 + 去重）；落盘即时生效无需重启网关
    modelWhitelistSet: (models: string[]) =>
      invoke<string[]>('model_whitelist_set', { models }),
    // Trae 模型元数据 L1 覆盖层（§6.1 编辑弹框读写 data/trae_model_meta.json）：
    // 顶层参数 camelCase；meta 嵌套字段保持 snake_case；null 字段 = 未设置，交由下层兜底
    // metaGet 用于编辑弹框回显跨会话人工值（null = 无人工值，全字段交由下层兜底）
    metaGet: (model: string) => invoke<TraeModelMeta | null>('trae_model_meta_get', { model }),
    metaSet: (model: string, meta: TraeModelMeta) =>
      invoke<void>('trae_model_meta_set', { model, meta }),
    // 清除人工覆盖（恢复自动来源链）；返回是否确有删除
    metaClear: (model: string) => invoke<boolean>('trae_model_meta_clear', { model }),
    // T2：多 API Key 管理（统一列表，无主/子之分）
    keysList: () => invoke<ApiKeysFileView>('api_keys_list'),
    // authDisabled 不传时保留服务端现值（避免整表保存覆盖鉴权开关）
    keysSave: (keys: ApiKeyEntry[], authDisabled?: boolean) =>
      invoke('api_keys_save', { keys, authDisabled: authDisabled ?? null }),
    // ---- 指纹清洗规则表（wb_template_map KV）----
    // builtin=true = 当前生效内置默认规则（尚未自定义）；保存后成为独立自定义副本。
    // issue #129：档位分区——'wb'（wb 池 + Trae 池共用键）| 'buddy-ai'（国际档独立键）
    templateMapGet: (tier?: SanitizeTier) =>
      invoke<SanitizeMapView>('wb_template_map_get', { tier: tier ?? null }),
    // 整体替换保存；load_templates 每请求读取，保存即对下一请求生效，无需重启网关
    templateMapSet: (templates: SanitizeRule[], tier?: SanitizeTier) =>
      invoke<void>('wb_template_map_set', {
        tier: tier ?? null,
        map: { templates, updated_at: Math.floor(Date.now() / 1000) },
      }),
    // ---- 工具名别名表（issue #150：tool_alias_map KV，单键不分档）----
    // Trae 池 tools 工具集指纹识别治本：请求侧工具名→别名、响应侧逆映射回改。
    // builtin=true = 当前生效内置默认表（尚未自定义）；保存后成为独立自定义副本
    // （合并语义：内置表全量生效，自定义按 from 覆盖/追加，to=null 停用）。
    // load_tool_alias_rules 每请求读取，保存即对下一请求生效，无需重启网关
    toolAliasMapGet: () => invoke<ToolAliasMapView>('wb_tool_alias_map_get'),
    toolAliasMapSet: (rules: SanitizeRule[]) =>
      invoke<void>('wb_tool_alias_map_set', {
        map: { rules, updated_at: Math.floor(Date.now() / 1000) },
      }),
    // ---- 指纹样本（issue #125 增强：一键从样本生成规则）----
    // 网关命中（11128/空完成影子风控）落下的样本环形缓冲（最新在前，重启即清空）
    fingerprintSamplesGet: () => invoke<FingerprintSample[]>('fingerprint_samples_get'),
    fingerprintSamplesClear: () => invoke<void>('fingerprint_samples_clear'),
    // ---- 池级指纹清洗开关（issue #158 P1-4：仅内存热切换，重启复位默认开）----
    // wb = wb_sanitize（wb + Trae 池共用）；qoder = qoder_sanitize（Qoder 池独立）
    sanitizeSwitchesGet: () => invoke<SanitizeSwitches>('sanitize_switches_get'),
    sanitizeSwitchSet: (pool: 'wb' | 'qoder', enabled: boolean) =>
      invoke<void>('sanitize_switch_set', { pool, enabled }),
  },
  updater: {
    check: () => invoke<UpdateCheckResult>('update_check'),
    // 第一步：下载安装包（完成后返回本地路径，等待用户确认安装）
    // 注意：key 必须是 expectedVersion（Rust 参数 expected_version 的 Tauri 驼峰匹配），传 version 会报 missing required key
    download: (p: {
      downloadUrl: string;
      assetName: string;
      expectedVersion: string;
      expectedSha256?: string | null;
    }) => invoke<UpdateDownloaded>('update_download', p),
    // 第二步：启动安装器（win：/P /UPDATE /R 自动重启；mac：open dmg 人工拖拽安装）
    runInstaller: (p: { filePath: string; assetName: string }) =>
      invoke<void>('update_run_installer', p),
    // mac 更新专用：人工拖拽完成后一键重启（LaunchServices 拉起新版后旧进程退出）
    restartApp: () => invoke<void>('update_restart_app'),
    // mac 手动安装引导：主动退出应用释放 .app 占用（Finder 无法替换运行中的应用）
    quitApp: () => invoke<void>('update_quit_app'),
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
/** start 事件账号清单项（Rust 侧发出，scope 内全集：候选 pending / 跳过带原因 / 重试轮沿用上轮状态） */
export interface CheckinStartAccount {
  user_id: string;
  name: string;
  status: 'pending' | 'skip' | 'success' | 'already' | 'fail';
  skip_reason?: 'checked_in' | 'expired' | 'cooldown' | null;
}
export interface CheckinStartEvent {
  type: 'start';
  total: number;
  /** scope 内全集清单；Python 脚本转发的 start 无此字段，前端仅同步 total 不重建列表 */
  accounts?: CheckinStartAccount[];
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
  /** true=过滤后无候选账号（全部已签/过期/冷却中），未启动签到脚本 */
  empty?: boolean;
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
  /** Qoder 签到 NDJSON 进度（payload 为 JSON 字符串，归约前需 parse） */
  onQoderCheckinProgress?: (line: string) => void;
}

export async function setupListeners(
  handlers: ListenerHandlers,
): Promise<UnlistenFn[]> {
  // M10 修复：中途注册失败时清理已注册监听再抛出——原实现前 N-1 个监听泄漏，
  // 且 store.init 中断后部分事件通道静默失效
  const unsubs: UnlistenFn[] = [];
  try {
    await registerAll(handlers, unsubs);
    return unsubs;
  } catch (err) {
    // 审查修复（L31）：单个注销失败不阻断清理，继续注销其余监听
    for (const u of unsubs) {
      try {
        u();
      } catch {
        /* ignore */
      }
    }
    throw err;
  }
}

async function registerAll(
  handlers: ListenerHandlers,
  unsubs: UnlistenFn[],
): Promise<void> {
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
  if (handlers.onQoderCheckinProgress) {
    unsubs.push(
      await listen<string>('qoder-checkin-progress', (e) =>
        handlers.onQoderCheckinProgress!(e.payload),
      ),
    );
  }
}
