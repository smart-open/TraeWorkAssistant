// Web 版 API 适配层（T9）：桌面 Tauri invoke/listen 的服务端对位。
// - invoke(name, args) → `POST /api/cmd/{name}`（Tauri invoke 约定换皮 REST，
//   响应 `{"ok":true,"data":...}` / `{"ok":false,"error":...}`，契约见 aiwork-server cmd_bridge）
// - listen(event, cb)  → WebSocket `/api/ws`（T12c 双向推送，30s 心跳）；
//   WS 不可用时自动回退 SSE `GET /api/events/checkin`（广播通道全事件按名下发）
// 仅保留命令桥白名单内的命令封装；白名单外命令 404 fail-closed。

import type {
  AccountView,
  AdminTokenEntry,
  AdminTokenView,
  ApiPoolFile,
  ApiKeyEntry,
  ApiKeysFileView,
  CheckinOpts,
  CheckinTrendPoint,
  CreditDetail,
  CreditsDailySnapshot,
  CustomModel,
  DispatchPolicy,
  GatewaySettings,
  GroupView,
  ImportPreview,
  ImportReport,
  IpAllowlistConfig,
  JwtParseResult,
  LogLine,
  ModelOption,
  NotifyConfig,
  NotifyResult,
  OAuthLoginResult,
  OAuthLoginUrl,
  PoolStatus,
  QoderAccountView,
  QoderCheckinRecord,
  QoderCreditsResult,
  QoderCreditsSnapshot,
  QoderOauthDone,
  QoderOauthProgress,
  QoderPoolExport,
  QoderPoolImportResult,
  QoderSettings,
  SchedulerConfig,
  SchedulerTaskView,
  Settings,
  SanitizeMapView,
  SanitizeRule,
  TraeModelMeta,
  UnifiedModel,
  UsageDayView,
  UsageHistoryResult,
  WbActivityInfo,
  WbCheckinRecord,
  WbCreditsResult,
  WbCreditsSnapshot,
  WbModelInfo,
  WbPoolImportResult,
  WbUsageFallback,
  WbCreditsTrend,
  WbUsageOfficial,
  WbUsageOfficialAll,
  WorkBuddyAccountView,
  WorkBuddyScanResult,
  WorkBuddySettings,
} from '../types';

/** API 错误：携带 HTTP 状态码，401 供全局未登录拦截使用 */
export class ApiError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

/** camelCase → snake_case（仅顶层参数键；旧 tauri invoke 依赖框架自动转换，此处补齐） */
function toSnakeKey(key: string): string {
  return key.replace(/[A-Z]/g, (c) => `_${c.toLowerCase()}`);
}

/** 未登录事件名：invoke 收到 401 时派发，store 监听后回登录页 */
export const UNAUTHORIZED_EVENT = 'aiwork:unauthorized';

// 命令桥调用：body 按原命令参数名传键（顶层 camelCase 自动转 snake_case）；
// 成功解包 data，失败抛 ApiError；401 时同步派发全局未登录事件。
// 超时兜底（审查 F6.1）：后端/反代挂起时此前会永久等待（按钮 loading 转圈不归），
// 统一 120s 上限（覆盖导入/模型同步等慢命令）；网络层失败翻译为可读文案
//（审查 F4.1：此前 String(err) 直出 "TypeError: Failed to fetch"）。
const INVOKE_TIMEOUT_MS = 120_000;

async function fetchWithTimeout(url: string, init: RequestInit, timeoutMs: number): Promise<Response> {
  try {
    return await fetch(url, { ...init, signal: AbortSignal.timeout(timeoutMs) });
  } catch (e) {
    if (e instanceof DOMException && e.name === 'TimeoutError') {
      throw new ApiError(0, `请求超时（${Math.round(timeoutMs / 1000)}s 无响应），请检查服务端状态后重试`);
    }
    throw new ApiError(0, '网络连接失败：无法访问服务端（服务未启动或网络中断）');
  }
}

async function invoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  let body: Record<string, unknown> | undefined;
  if (args) {
    body = {};
    for (const [k, v] of Object.entries(args)) body[toSnakeKey(k)] = v;
  }
  const res = await fetchWithTimeout(
    `/api/cmd/${command}`,
    {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body ?? {}),
    },
    INVOKE_TIMEOUT_MS,
  );
  if (res.status === 401) {
    window.dispatchEvent(new Event(UNAUTHORIZED_EVENT));
    throw new ApiError(401, '未登录或登录已过期');
  }
  let payload: { ok?: boolean; data?: T; error?: string };
  try {
    payload = await res.json();
  } catch {
    throw new ApiError(res.status, `请求失败（HTTP ${res.status}）`);
  }
  if (!res.ok || !payload.ok) {
    throw new ApiError(res.status, payload.error ?? `请求失败（HTTP ${res.status}）`);
  }
  return payload.data as T;
}

// ---- 登录会话（ADR-4）----

/** token 登录：成功后服务端下发 HttpOnly 会话 cookie（Max-Age 7 天；HTTPS 到达附 Secure） */
export async function login(token: string): Promise<void> {
  const res = await fetchWithTimeout(
    '/api/login',
    {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ token }),
    },
    30_000,
  );
  if (res.ok) return;
  let msg = `登录失败（HTTP ${res.status}）`;
  try {
    const j = (await res.json()) as { error?: string };
    if (j?.error) msg = j.error;
  } catch {
    /* 非 JSON 响应，保留 HTTP 状态信息 */
  }
  throw new ApiError(res.status, msg);
}

// ---- 实时事件订阅（桌面 listen 的服务端对位）：WebSocket 优先，SSE 回退（T12c）----

export type UnlistenFn = () => void;

// 集中登记服务端会下发的事件名：WS 帧自带事件名，EventSource 需预先注册命名监听。
// 桌面单事件 checkin-progress 承载全部签到载荷（start/account/retry/done），
// 服务端拆为 checkin-progress / checkin-done 两个命名事件，别名归一分发。
const RT_EVENT_NAMES = [
  'checkin-progress',
  'checkin-done',
  'wb-checkin-progress',
  'wb-oauth-progress',
  'wb-oauth-done',
  'qoder-checkin-progress',
  'qoder-oauth-progress',
  'qoder-oauth-done',
  // 调度器看板数据同步成功（issue #61）：Dashboard 按 platform 静默重读缓存
  'board-data-synced',
  // WS 慢消费者丢帧通知（审查 F6.3）：此前前端对该事件名只 console.warn 丢弃，
  // 丢帧完全不可感知；纳入订阅后 store 可监听并提示用户数据可能滞后
  'ws-lagged',
];
const RT_ALIASES: Record<string, string[]> = {
  'checkin-progress': ['checkin-progress', 'checkin-done'],
};
type RtHandler = (payload: unknown) => void;
const rtHandlers = new Map<string, Set<RtHandler>>();

function dispatchEvent(name: string, payload: unknown) {
  rtHandlers.get(name)?.forEach((h) => h(payload));
}

function hasSubscribers() {
  return [...rtHandlers.values()].some((s) => s.size > 0);
}

// ---------- SSE 回退通道（EventSource，断线由浏览器自动重连） ----------
let sseSource: EventSource | null = null;

function ensureSseConnection() {
  if (sseSource || !hasSubscribers()) return;
  const es = new EventSource('/api/events/checkin');
  sseSource = es;
  for (const name of RT_EVENT_NAMES) {
    es.addEventListener(name, (ev) => {
      let payload: unknown;
      try {
        payload = JSON.parse((ev as MessageEvent).data as string);
      } catch {
        return;
      }
      dispatchEvent(name, payload);
    });
  }
  // 断线由浏览器自动重连；登录过期期间的重试无害，重登录后恢复
  es.onerror = () => {};
}

function closeSse() {
  sseSource?.close();
  sseSource = null;
}

// ---------- WebSocket 通道（T12c）：/api/ws，30s 心跳保活，断开自动回退 SSE ----------
let wsSock: WebSocket | null = null;
let wsHeartbeat: number | null = null;
let wsRetryTimer: number | null = null;
// 最近一次收到 pong 的时刻（审查 F6.2）：此前心跳只发不收，TCP 半开（休眠唤醒/
// 网络切换）时 readyState 仍 OPEN、onclose 不触发 → 事件通道静默死亡且永不回退 SSE。
// 连续两个心跳周期无 pong 即判定半开，主动关闭走 SSE 回退
let wsLastPong = 0;
const WS_PONG_DEAD_MS = 70_000;

function wsUrl() {
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  return `${proto}//${location.host}/api/ws`;
}

function closeWs() {
  if (wsHeartbeat !== null) {
    clearInterval(wsHeartbeat);
    wsHeartbeat = null;
  }
  wsLastPong = 0;
  const sock = wsSock;
  wsSock = null;
  if (sock) {
    // 先摘掉回调再 close，避免 close/error 触发回退逻辑
    sock.onopen = sock.onmessage = sock.onclose = sock.onerror = null;
    try {
      sock.close();
    } catch {
      /* 已关闭 */
    }
  }
}

function tryWs() {
  if (wsSock || !hasSubscribers()) return;
  if (typeof WebSocket === 'undefined') {
    ensureSseConnection();
    return;
  }
  let sock: WebSocket;
  try {
    sock = new WebSocket(wsUrl());
  } catch {
    ensureSseConnection();
    scheduleWsRetry();
    return;
  }
  wsSock = sock;
  sock.onopen = () => {
    // 显式订阅全部事件（防服务端默认语义漂移）；30s 心跳防反代 idle 断连
    sock.send(JSON.stringify({ type: 'subscribe', events: RT_EVENT_NAMES }));
    wsLastPong = Date.now();
    wsHeartbeat = window.setInterval(() => {
      if (wsSock?.readyState !== WebSocket.OPEN) return;
      if (Date.now() - wsLastPong > WS_PONG_DEAD_MS) {
        // 两个心跳周期无 pong：半开连接，主动关闭走 SSE 回退
        //（closeWs 会摘掉 onclose，回退需在此显式执行）
        closeWs();
        ensureSseConnection();
        scheduleWsRetry();
        return;
      }
      wsSock.send('{"type":"ping"}');
    }, 30_000);
    // WS 就绪后关闭 SSE，避免同一事件重复消费
    closeSse();
  };
  sock.onmessage = (ev) => {
    let frame: { event?: string; type?: string; payload?: unknown };
    try {
      frame = JSON.parse(ev.data as string);
    } catch {
      return; // 非 JSON 帧
    }
    // 心跳应答：刷新 pong 时间戳（半开检测依据）
    if (!frame.event && frame.type === 'pong') {
      wsLastPong = Date.now();
      return;
    }
    if (frame.event) dispatchEvent(frame.event, frame.payload);
  };
  // 关闭/异常统一走 onclose：回退 SSE + 30s 后重试 WS（重连成功会自动关掉 SSE）
  sock.onclose = () => {
    closeWs();
    ensureSseConnection();
    scheduleWsRetry();
  };
  sock.onerror = () => {};
}

function scheduleWsRetry() {
  if (wsRetryTimer !== null) return;
  wsRetryTimer = window.setTimeout(() => {
    wsRetryTimer = null;
    tryWs();
  }, 30_000);
}

function ensureRealtime() {
  if (wsRetryTimer !== null) {
    clearTimeout(wsRetryTimer);
    wsRetryTimer = null;
  }
  tryWs();
}

function closeRealtimeIfIdle() {
  if (hasSubscribers()) return;
  closeWs();
  closeSse();
  if (wsRetryTimer !== null) {
    clearTimeout(wsRetryTimer);
    wsRetryTimer = null;
  }
  rtHandlers.clear();
}

// 事件订阅：载荷经 `{ payload }` 包装对齐桌面版签名（e.payload）
export async function listen<T>(
  event: string,
  cb: (ev: { payload: T }) => void,
): Promise<UnlistenFn> {
  const names = RT_ALIASES[event] ?? (RT_EVENT_NAMES.includes(event) ? [event] : []);
  if (names.length === 0) {
    // 桌面专属事件（proxy-log/switch-*等）在 Web 版已随模块退役
    console.warn(`[rt] Web 版不支持的事件订阅: ${event}，已忽略`);
    return () => {};
  }
  const handler: RtHandler = (payload) => cb({ payload: payload as T });
  for (const n of names) {
    if (!rtHandlers.has(n)) rtHandlers.set(n, new Set());
    rtHandlers.get(n)!.add(handler);
  }
  ensureRealtime();
  return () => {
    for (const n of names) rtHandlers.get(n)?.delete(handler);
    closeRealtimeIfIdle();
  };
}

// 所有命令封装集中于此，字段名严格遵循 Rust 端 snake_case 约定（invoke 内自动转换）。
export const api = {
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
    // Trae 官网消耗明细（usage_history；fresh=true 增量拉取，false 纯缓存读取）
    usageHistory: (fresh?: boolean) =>
      invoke<UsageHistoryResult>('usage_history', { fresh: fresh ?? null }),
    cooldownClear: (userId: string) =>
      invoke('cooldown_clear', { userId }),
    cooldownClearAll: () =>
      invoke<number>('cooldown_clear_all'),
    refreshJwt: (userId: string, force = false) =>
      invoke<string>('refresh_jwt', { userId, force }),
    exportRaw: () => invoke<Record<string, unknown>>('accounts_export_raw'),
    importAccounts: (content: string, only?: number[]) =>
      invoke<ImportReport>('accounts_import', { content, only }),
    // F-46：导入前预览（解析账号/分组、标记已存在，不写盘）
    importPreview: (content: string) => invoke<ImportPreview>('accounts_import_preview', { content }),
    // 按需取完整 JWT（列表接口只回掩码值；弹窗/复制场景调用）
    getJwt: (userId: string) => invoke<string>('account_get_jwt', { userId }),
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
    // 命令桥约定：body 即 CheckinOpts（scope / user_ids / skip_checked_in / skip_expired）
    start: (opts: CheckinOpts) => invoke('checkin_start', { ...opts }),
    // T8：近 N 天签到结果趋势（Dashboard 堆叠图）
    trends: (days?: number) =>
      invoke<CheckinTrendPoint[]>('checkin_trends', { days: days ?? null }),
  },
  misc: {
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
  },
  scheduler: {
    // 内置定时任务状态（key/name/time/enabled + 最近一次执行情况）
    status: () =>
      invoke<{ tasks: SchedulerTaskView[] }>('scheduler_status'),
    // 任务开关配置：disabled_tasks 停用名单，缺省 = 全部启用（推荐配置）
    configGet: () => invoke<SchedulerConfig>('scheduler_config_get'),
    // 返回保存后的生效值；未知任务键整体拒绝
    configSet: (config: SchedulerConfig) => invoke<SchedulerConfig>('scheduler_config_set', { config }),
  },
  // ---- 通知渠道（Phase 3 T11：Bark / Server酱 / Webhook，独立 kv）----
  notify: {
    getConfig: () => invoke<NotifyConfig>('notify_config_get'),
    // 返回保存后的生效值（前端表单以返回值为准）
    setConfig: (config: NotifyConfig) => invoke<NotifyConfig>('notify_config_set', { config }),
    // 用表单当前值直接发测试通知（不落盘，不校验总开关）
    test: (config: NotifyConfig) => invoke<NotifyResult>('notify_test', { config }),
  },
  // ---- IP 允许列表（Phase 3 T12a：应用层访问控制，覆盖网关与管理面）----
  ipAllowlist: {
    getConfig: () => invoke<IpAllowlistConfig>('ip_allowlist_get'),
    // 返回保存后的生效值（归一化 trim/去重后）；存在无效 CIDR 条目时整体拒绝
    setConfig: (config: IpAllowlistConfig) => invoke<IpAllowlistConfig>('ip_allowlist_set', { config }),
  },
  // ---- 管理员令牌（Phase 3 T12b：主 token 之外的可吊销附加令牌）----
  adminTokens: {
    list: () => invoke<AdminTokenView[]>('admin_tokens_list'),
    // 创建成功返回含明文 token 的完整条目（仅此一次，前端应引导立即复制）
    create: (label: string) => invoke<AdminTokenEntry>('admin_token_create', { label }),
    revoke: (id: string) => invoke<void>('admin_token_revoke', { id }),
  },
  // ---- API 网关管理（Web 版网关常驻运行，无启停命令）----
  apiServer: {
    poolList: () => invoke<ApiPoolFile>('pool_list'),
    // T10：池设置扩展调度策略与分组筛选；wbStrategy 为 Buddy 池独立策略（空串 = 跟随 Trae 池策略）
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
        /** Qoder 池入池白名单（qd- 账号 id）；null/未传 = 保留原值 */
        qoderUids?: string[];
        /** Qoder 池分组筛选；null/未传 = 保留原值 */
        qoderGroupIds?: string[];
        /** Qoder 池内调度策略（空串 = 跟随 Trae 池）；null/未传 = 保留原值 */
        qoderStrategy?: string;
        /** Trae 池参与调度开关（默认开）；null/未传 = 保留原值 */
        traeEnabled?: boolean;
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
        qoderUids: wbFlags?.qoderUids ?? null,
        qoderGroupIds: wbFlags?.qoderGroupIds ?? null,
        qoderStrategy: wbFlags?.qoderStrategy ?? null,
        traeEnabled: wbFlags?.traeEnabled ?? null,
        wbStrategy: wbStrategy ?? null,
      }),
    poolStatus: () => invoke<PoolStatus[]>('pool_status'),
    /** WB 池实时状态（F-77⑤：含 per-account inflight 在途计数） */
    wbPoolStatus: () => invoke<PoolStatus[]>('wb_pool_status'),
    /** Qoder 池实时状态（Qoder「资源调度」页：含 per-account inflight 在途计数） */
    qoderPoolStatus: () => invoke<PoolStatus[]>('qoder_pool_status'),
    /** 手动同步 Qoder 模型目录（复用每日调度任务入口；返回采纳模型数） */
    qoderCatalogSync: () => invoke<number>('api_qoder_catalog_sync'),
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
    // T1：近 N 天 API 用量统计（Trae 模型请求桶；按日聚合）
    usageStats: (days?: number) =>
      invoke<UsageDayView[]>('api_usage_stats', { days: days ?? null }),
    // WB 上游用量统计（wb_days 桶，Buddy「资源调度」页专用，与 Trae 侧分账）
    wbUsageStats: (days?: number) =>
      invoke<UsageDayView[]>('api_wb_usage_stats', { days: days ?? null }),
    // 自定义模型用量统计（custom_days 桶，API 管理·用量统计「自定义」筛选）
    customUsageStats: (days?: number) =>
      invoke<UsageDayView[]>('api_custom_usage_stats', { days: days ?? null }),
    // Qoder 上游用量统计（qoder_days 桶，与 Trae/WB/Custom 侧分账）
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
    // ---- 统一网关命令（Phase 1 §8.1）：统一模型目录 / 网关设置 ----
    unifiedModels: (availableOnly?: boolean) =>
      invoke<UnifiedModel[]>('api_unified_models', { availableOnly: availableOnly ?? null }),
    // 池间调度策略（dispatch_policy.json：strategy/priority/per_model/fallback）
    dispatchPolicyGet: () => invoke<DispatchPolicy>('dispatch_policy_get'),
    // 返回规范化后的生效值（前端展示以返回值为准）；落盘即时生效无需重启网关
    dispatchPolicySet: (policy: DispatchPolicy) =>
      invoke<DispatchPolicy>('dispatch_policy_set', { policy }),
    gatewaySettingsGet: () => invoke<GatewaySettings>('gateway_settings_get'),
    // 返回规范化后的生效值（前端展示以返回值为准）；端口改动需重启服务生效
    gatewaySettingsSet: (settings: GatewaySettings) =>
      invoke<GatewaySettings>('gateway_settings_set', { settings }),
    // 全局模型白名单（issue #26，canonical 归一列表；空 = 不限）
    modelWhitelistGet: () => invoke<string[]>('model_whitelist_get'),
    // 返回归一后的生效值（canonical + 去空 + 去重）；落盘即时生效无需重启网关
    modelWhitelistSet: (models: string[]) =>
      invoke<string[]>('model_whitelist_set', { models }),
    // Trae 模型元数据 L1 覆盖层（§6.1 编辑弹框读写 data/trae_model_meta.json）
    metaGet: (model: string) => invoke<TraeModelMeta | null>('trae_model_meta_get', { model }),
    metaSet: (model: string, meta: TraeModelMeta) =>
      invoke<void>('trae_model_meta_set', { model, meta }),
    // 清除人工覆盖（恢复自动来源链）；返回是否确有删除
    metaClear: (model: string) => invoke<boolean>('trae_model_meta_clear', { model }),
    // T2：多 API Key 管理（统一列表，无主/子之分）
    keysList: () => invoke<ApiKeysFileView>('api_keys_list'),
    // authDisabled 不传时保留服务端现值（避免整表保存覆盖鉴权开关）
    keysSave: (keys: ApiKeyEntry[], authDisabled?: boolean) =>
      invoke('api_keys_save', { keys, authDisabled }),
  },
  // ---- WB 手工路由配置（wb_route_config / wb_template_map）----
  wbConfig: {
    routeGet: () => invoke<Record<string, unknown>>('wb_route_config_get'),
    routeSet: (config: Record<string, unknown>) => invoke('wb_route_config_set', { config }),
    templateMapGet: () => invoke<SanitizeMapView>('wb_template_map_get'),
    // 整体替换保存；load_templates 每请求读取，保存即对下一请求生效，无需重启网关
    templateMapSet: (templates: SanitizeRule[]) =>
      invoke<void>('wb_template_map_set', {
        map: { templates, updated_at: Math.floor(Date.now() / 1000) },
      }),
  },
  oauth: {
    getLoginUrl: () => invoke<OAuthLoginUrl>('oauth_get_login_url'),
    parseCallback: (callbackUrl: string) =>
      invoke('oauth_parse_callback', { callbackUrl }),
    login: (callbackUrl: string, accountName?: string, groupId?: string) =>
      invoke<OAuthLoginResult>('oauth_login', { callbackUrl, accountName, groupId }),
  },
  // ---- WorkBuddy（Web 版保留：账号/分组/签到/积分/设置/导入导出/OAuth/用量）----
  workbuddy: {
    accountsList: () => invoke<WorkBuddyAccountView[]>('workbuddy_accounts_list'),
    accountSave: (userId: string, name?: string, note?: string) =>
      invoke('workbuddy_account_save', { userId, name, note }),
    accountMove: (userId: string, groupId: string | null) =>
      invoke('workbuddy_account_move', { userId, groupId }),
    groups: {
      list: () => invoke<GroupView[]>('workbuddy_groups_list'),
      create: (name: string, color: string) =>
        invoke<string>('workbuddy_groups_create', { name, color }),
      update: (id: string, patch: { name?: string; color?: string; order?: number }) =>
        invoke('workbuddy_groups_update', { id, ...patch }),
      remove: (id: string) => invoke('workbuddy_groups_remove', { id }),
    },
    accountRemove: (userId: string, deleteSnapshot?: boolean) =>
      invoke('workbuddy_account_remove', { userId, deleteSnapshot }),
    scanAuthFile: () => invoke<WorkBuddyScanResult | null>('workbuddy_scan_auth_file'),
    accountImportAuth: (name?: string) =>
      invoke<WorkBuddyAccountView>('workbuddy_account_import_auth', { name }),
    refreshToken: (userId: string, force = false) =>
      invoke<string>('workbuddy_refresh_token', { userId, force }),
    checkinStart: (opts: { user_ids?: string[]; skip_checked_in: boolean; skip_expired: boolean }) =>
      invoke('workbuddy_checkin_start', { opts }),
    growthRun: () => invoke('workbuddy_growth_run'),
    checkinResults: (days?: number) =>
      invoke<WbCheckinRecord[]>('workbuddy_checkin_results', { days: days ?? null }),
    creditsFetch: (userId?: string, fresh?: boolean) =>
      invoke<WbCreditsResult>('workbuddy_credits_fetch', { userId, fresh }),
    // 快照时序读取（credits-dashboard-plan.md §2.2 方案 B：earned 已随快照落库）
    creditsHistoryList: () =>
      invoke<{ snapshots: WbCreditsSnapshot[] }>('workbuddy_credits_history_list'),
    editionsBackfill: () => invoke<number>('workbuddy_editions_backfill'),
    settingsGet: () => invoke<WorkBuddySettings>('workbuddy_settings_get'),
    settingsSet: (patch: WorkBuddySettings) => invoke('workbuddy_settings_set', { patch }),
    // 账号库导入导出（F-46）
    accountsExport: (includeCredentials?: boolean) =>
      invoke<Record<string, unknown>>('workbuddy_accounts_export', { includeCredentials }),
    accountsImport: (payload: Record<string, unknown>) =>
      invoke<WbPoolImportResult>('workbuddy_accounts_import', { payload }),
    // OAuth 扫码全流程（后台线程执行，进度经 SSE wb-oauth-progress / wb-oauth-done 下发）
    oauthLogin: () => invoke<void>('workbuddy_oauth_login'),
    // 官方用量 + 用量聚合（F-25/26/57/58）
    // 注意：Rust 端参数名为 refresh（Option<bool>），fresh=false 走缓存
    usageOfficial: (userId?: string, fresh?: boolean) =>
      invoke<WbUsageOfficial>('workbuddy_usage_official', { userId, refresh: fresh }),
    /** 全账号官方用量聚合（近 7 日积分消耗主数据源；31 天零填充） */
    usageOfficialAll: () => invoke<WbUsageOfficialAll>('workbuddy_usage_official_all'),
    usageFallback: () => invoke<WbUsageFallback>('workbuddy_usage_fallback'),
    /** 积分趋势序列（wb_credits_history 快照差分 + 签到奖励推导；积分看板三线图） */
    creditsTrend: () => invoke<WbCreditsTrend>('workbuddy_credits_trend'),
    activityInfo: (userId?: string, fresh?: boolean) =>
      invoke<WbActivityInfo>('workbuddy_activity_info', { userId, refresh: fresh }),
  },
  // ---- Qoder（Web 版裁剪：剔除桌面端 env/cli/scan/live-logins/计划任务命令）----
  qoder: {
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
     *  同步返回授权页链接：前端在点击手势内 window.open 自动打开（Web 化替代桌面 open_in_browser）。
     *  compat=true = 兼容模式（授权 URL 不带 client_id）：官方 client_id 常量被
     *  Qoder 轮换导致授权页「参数无效」时的降级链路；前端在授权超时后自动切换重试 */
    oauthLogin: (compat?: boolean) => invoke<string>('qoder_oauth_login', { compat: compat ?? null }),
    /** 取消进行中的 OAuth 轮询（弹框「取消授权」）：后端置标志后轮询线程自行发失败终态 */
    oauthCancel: () => invoke<void>('qoder_oauth_cancel'),
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
  },
};

// ---- 事件载荷 ----
/** start 事件账号清单项（scope 内全集：候选 pending / 跳过带原因 / 重试轮沿用上轮状态） */
export interface CheckinStartAccount {
  user_id: string;
  name: string;
  status: 'pending' | 'skip' | 'success' | 'already' | 'fail';
  skip_reason?: 'checked_in' | 'expired' | 'cooldown' | null;
}
export interface CheckinStartEvent {
  type: 'start';
  total: number;
  /** scope 内全集清单；脚本转发的 start 无此字段，前端仅同步 total 不重建列表 */
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

export interface ListenerHandlers {
  onCheckinProgress?: (e: CheckinProgressEvent) => void;
  /** Qoder 签到 NDJSON 进度（payload 为 JSON 字符串，归约前需 parse） */
  onQoderCheckinProgress?: (line: string) => void;
}

// 事件订阅集中注册（对位桌面版 setupListeners；Web 版保留签到进度流）
export async function setupListeners(
  handlers: ListenerHandlers,
): Promise<UnlistenFn[]> {
  const unsubs: UnlistenFn[] = [];
  if (handlers.onCheckinProgress) {
    unsubs.push(
      await listen<CheckinProgressEvent>('checkin-progress', (e) =>
        handlers.onCheckinProgress!(e.payload),
      ),
    );
  }
  if (handlers.onQoderCheckinProgress) {
    unsubs.push(
      await listen<string>('qoder-checkin-progress', (e) =>
        handlers.onQoderCheckinProgress!(e.payload as string),
      ),
    );
  }
  return unsubs;
}
