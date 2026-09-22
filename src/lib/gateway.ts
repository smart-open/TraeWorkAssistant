/**
 * 网关对外基础地址（仅用于界面展示与复制配置示例，不影响前端自身请求）：
 * 1. 构建时注入 VITE_GATEWAY_BASE_URL（如 https://gw.example.com）则优先使用（环境变量注入）；
 * 2. 默认取当前浏览器访问地址的 origin（协议+主机+端口）——Web 版网关与网站同端口
 *    （单端口 axum：/v1/* 与管理面同源），远程部署时自动跟随访问地址，
 *    页面为 https 时跟随 https（反代终结 TLS 场景），避免混合内容拦截。
 */
export function gatewayBaseUrl(): string {
  const env = (import.meta.env.VITE_GATEWAY_BASE_URL ?? '').trim().replace(/\/+$/, '');
  if (env) return env;
  return window.location.origin || 'http://127.0.0.1:8080';
}
