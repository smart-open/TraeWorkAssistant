// 与 Rust 端 DTO 对齐的类型定义。注意：Tauri 命令参数默认使用 snake_case，
// 嵌套对象（CheckinOpts / LogsOpts / Settings）的字段必须保持 snake_case。

export type ViewKey =
  | 'dashboard'
  | 'accounts'
  | 'checkin'
  | 'credits'
  | 'logs'
  | 'settings';

export interface EnvStatus {
  installed: boolean;
  running: boolean;
  version: string | null;
  path: string | null;
}

export interface ProxyStatus {
  running: boolean;
  port: number;
  captured: number;
  started_at: number | null;
}

export interface AccountView {
  user_id: string;
  name: string;
  group_id: string | null;
  jwt: string;
  jwt_exp_hours: number | null;
  jwt_exp_timestamp: number | null;
  checked_today: boolean | null;
  credits: number | null;
  device_id_masked: string | null;
}

export interface GroupView {
  id: string;
  name: string;
  color: string;
  order: number;
  count: number;
}

export type JwtStatus = 'ok' | 'warn' | 'expired' | 'unknown';

export interface JwtParseResult {
  user_id: string | null;
  exp_hours: number | null;
  exp_timestamp: number | null;
  status: JwtStatus;
}

export interface LogLine {
  time: string;
  log_type: string;
  message: string;
}

// snake_case 必须与 Rust Settings 完全一致
export interface Settings {
  proxy_port: number;
  theme: string;
  launch_minimized: boolean;
  auto_start_proxy: boolean;
  tray: boolean;
  language: string;
  checkin_skip_checked: boolean;
  checkin_skip_expired: boolean;
  retry: number;
  notify: string;
  trae_path: string | null;
  data_dir: string | null;
  log_retention_days: number;
  proxy_domains: string;
  proxy_log_path: string | null;
}

export interface CheckinOpts {
  scope: string;
  user_ids?: string[];
  skip_checked_in: boolean;
  skip_expired: boolean;
}

export type CheckinAccountStatus = 'pending' | 'already' | 'success' | 'fail';

export interface CheckinAccountResult {
  index: number;
  user_id: string;
  name: string;
  status: CheckinAccountStatus;
  credits?: number;
  delta?: number;
  elapsed?: number;
  code?: number;
  message?: string;
}

export interface CheckinDone {
  ok: number;
  already: number;
  failed: number;
  total?: number;
}

export interface CreditRecord {
  date: string;
  user_id: string;
  credits: number;
  delta: number;
}

export interface ProxyLogEntry {
  id: string;
  timestamp: string;
  method: string;
  host: string;
  path: string;
  status: string;
  size: number;
}

export interface ProxyLogListResult {
  entries: ProxyLogEntry[];
  total: number;
}
