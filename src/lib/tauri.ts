import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type {
  AccountView,
  ApiServiceStatus,
  ApiPoolFile,
  ApiKeyEntry,
  CcSwitchStatus,
  ApiKeysFileView,
  AppLocate,
  CheckinDone,
  CheckinOpts,
  CheckinTrendPoint,
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
  GatewaySettings,
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
  WbCheckinRecord,
  WbCliStatus,
  WbCliRotateResult,
  WbCliRotateLog,
  WbOauthDone,
  WbOauthProgress,
  WbResetItem,
  WbResetResult,
  WbTokenStats,
  WbUsageOfficial,
  WbUsageFallback,
  WbActivityInfo,
  WbPoolImportResult,
  WbModelInfo,
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
  switchAccount: (
    userId: string,
    targetApp?: 'TraeWork' | 'Trae' | 'Doubao' | 'WorkBuddy',
    skipJwtProbe?: boolean,
  ) =>
    invoke('switch_account', {
      userId,
      targetApp: targetApp ?? null,
      // 续期 JWT 场景目标账号 JWT 本就可能已吊销，跳过切换前预检避免拦死续期链路
      skipJwtProbe: skipJwtProbe ?? false,
    }),
  saveCurrentLogin: (userId: string, targetApp?: 'TraeWork' | 'Trae' | 'Doubao' | 'WorkBuddy') =>
    invoke('save_current_login', { userId, targetApp: targetApp ?? null }),
  resetDeviceIds: (targetApp?: 'TraeWork' | 'Trae') =>
    invoke('reset_device_ids', { targetApp: targetApp ?? null }),
  profiles: {
    list: (targetApp?: 'TraeWork' | 'Trae' | 'Doubao') =>
      invoke<ProfileInfo[]>('profile_list', { targetApp: targetApp ?? null }),
    backup: (userId: string, targetApp?: 'TraeWork' | 'Trae' | 'Doubao') =>
      invoke('profile_backup', { userId, targetApp: targetApp ?? null }),
    restore: (userId: string, targetApp?: 'TraeWork' | 'Trae' | 'Doubao') =>
      invoke('profile_restore', { userId, targetApp: targetApp ?? null }),
    delete: (userId: string, targetApp?: 'TraeWork' | 'Trae' | 'Doubao') =>
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
  },
  // ---- WorkBuddy（批次1；Rust workbuddy.rs；字段名严格 snake_case）----
  workbuddy: {
    envCheck: () => invoke<WorkBuddyEnvCheck>('workbuddy_env_check'),
    accountsList: () => invoke<WorkBuddyAccountView[]>('workbuddy_accounts_list'),
    accountSave: (userId: string, name?: string, note?: string) =>
      invoke('workbuddy_account_save', { userId, name: name ?? null, note: note ?? null }),
    accountRemove: (userId: string, deleteSnapshot?: boolean) =>
      invoke('workbuddy_account_remove', { userId, deleteSnapshot: deleteSnapshot ?? null }),
    scanAuthFile: () => invoke<WorkBuddyScanResult | null>('workbuddy_scan_auth_file'),
    accountImportAuth: (name?: string) =>
      invoke<WorkBuddyAccountView>('workbuddy_account_import_auth', { name: name ?? null }),
    refreshToken: (userId: string) => invoke<string>('workbuddy_refresh_token', { userId }),
    checkinStart: (opts: { user_ids?: string[]; skip_checked_in: boolean; skip_expired: boolean }) =>
      invoke('workbuddy_checkin_start', { opts }),
    growthRun: () => invoke('workbuddy_growth_run'),
    checkinResults: (days?: number) =>
      invoke<WbCheckinRecord[]>('workbuddy_checkin_results', { days: days ?? null }),
    checkinTaskRegister: (times: string[]) =>
      invoke('workbuddy_checkin_task_register', { times }),
    checkinTaskStatus: () => invoke<string[]>('workbuddy_checkin_task_status'),
    checkinTaskUnregister: () => invoke('workbuddy_checkin_task_unregister'),
    renewTaskRegister: (day?: string) => invoke('workbuddy_renew_task_register', { day: day ?? null }),
    renewTaskStatus: () => invoke<boolean>('workbuddy_renew_task_status'),
    renewTaskUnregister: () => invoke('workbuddy_renew_task_unregister'),
    creditsFetch: (userId?: string, fresh?: boolean) =>
      invoke<WbCreditsResult>('workbuddy_credits_fetch', { userId: userId ?? null, fresh: fresh ?? null }),
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
    chatdataBackup: (userId: string) =>
      invoke<{ ok: boolean; files: number; path: string }>('workbuddy_chatdata_backup', { userId }),
    chatdataRestore: (userId: string) =>
      invoke<{ ok: boolean; files: number }>('workbuddy_chatdata_restore', { userId }),
    chatdataInfo: (userId: string) =>
      invoke<{ backed: boolean; size_bytes?: number; files?: number; backed_at?: string; has_edge_mapping?: boolean }>(
        'workbuddy_chatdata_info',
        { userId },
      ),
    chatdataCopy: (sourceUserId: string, targetUserId: string) =>
      invoke<{ ok: boolean; copied: number; total_lines: number; sessions_cloned: number; mappings_registered: number }>(
        'workbuddy_chatdata_copy',
        { sourceUserId, targetUserId },
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
    usageOfficial: (userId?: string, fresh?: boolean) =>
      invoke<WbUsageOfficial>('workbuddy_usage_official', { userId: userId ?? null, fresh: fresh ?? null }),
    usageFallback: () => invoke<WbUsageFallback>('workbuddy_usage_fallback'),
    tokenStats: () => invoke<WbTokenStats>('workbuddy_token_stats'),
    activityInfo: (userId?: string, fresh?: boolean) =>
      invoke<WbActivityInfo>('workbuddy_activity_info', { userId: userId ?? null, refresh: fresh ?? null }),
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
    // T10：池设置扩展调度策略与分组筛选；T5.2/T5.3/T5.5/T5.6③ 扩展 WB 开关组
    poolSet: (
      uids: string[],
      strategy?: string,
      groupIds?: string[],
      wbFlags?: {
        wbEnabled?: boolean;
        wbDefaultThinking?: boolean;
        wbToolExec?: boolean;
        wbBgDowngrade?: boolean;
      },
    ) =>
      invoke('pool_set', {
        uids,
        strategy: strategy ?? null,
        groupIds: groupIds ?? null,
        wbEnabled: wbFlags?.wbEnabled ?? null,
        wbDefaultThinking: wbFlags?.wbDefaultThinking ?? null,
        wbToolExec: wbFlags?.wbToolExec ?? null,
        wbBgDowngrade: wbFlags?.wbBgDowngrade ?? null,
      }),
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
    // T5.1/F-37：WB 上游模型目录动态替换（手动触发；网关启动时已自动做一次）
    wbCatalogSync: () => invoke<number>('api_wb_catalog_sync'),
    // WB 目录模型列表（Buddy「资源调度」页展示）
    wbCatalogList: () => invoke<WbModelInfo[]>('api_wb_catalog_list'),
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
    // 自定义模型用量统计（custom_days 桶，API 管理·用量统计「自定义」筛选）
    customUsageStats: (days?: number) =>
      invoke<UsageDayView[]>('api_custom_usage_stats', { days: days ?? null }),
    // 自定义模型列表（custom_models.json，OpenAI 兼容上游直通）
    customModelsList: () => invoke<CustomModel[]>('custom_models_list'),
    // 保存自定义模型（upsert：id 空 = 新增；返回保存后的完整列表）
    customModelsSave: (model: CustomModel) =>
      invoke<CustomModel[]>('custom_models_save', { model }),
    // 按 id 删除自定义模型；返回是否确有删除
    customModelsRemove: (id: string) => invoke<boolean>('custom_models_remove', { id }),
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
    gatewaySettingsSet: (settings: GatewaySettings) =>
      invoke<GatewaySettings>('gateway_settings_set', { settings }),
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
