/**
 * 网关对外基础地址（仅用于界面展示与复制配置示例，不影响前端自身请求）：
 * 1. 构建时注入 VITE_GATEWAY_BASE_URL（如 https://gw.example.com）则优先使用（环境变量注入）；
 * 2. 默认取当前浏览器访问地址的主机名/IP + 网关端口——同机部署时与页面同源，
 *    远程部署时自动跟随访问域名，不再固定 127.0.0.1；
 *    页面为 https 时跟随 https（网关经反代终结 TLS 的场景），避免混合内容拦截。
 */
export function gatewayBaseUrl(port: number): string {
  const env = (import.meta.env.VITE_GATEWAY_BASE_URL ?? '').trim().replace(/\/+$/, '');
  if (env) return env;
  const host = window.location.hostname || '127.0.0.1';
  const proto = window.location.protocol === 'https:' ? 'https:' : 'http:';
  return `${proto}//${host}:${port}`;
}
